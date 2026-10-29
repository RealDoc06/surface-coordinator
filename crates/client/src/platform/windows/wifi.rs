//! Wi-Fi through the Native Wi-Fi API (`wlanapi`) and the IP Helper API.
//!
//! `netsh` would be simpler, but its output is localised. Everything here
//! blocks; callers run it on a blocking thread.

use std::ffi::{c_void, CStr};
use std::net::{IpAddr, Ipv4Addr};
use std::ptr::{addr_of, null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BUFFER_OVERFLOW, ERROR_INVALID_STATE, ERROR_NO_DATA,
    ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_NOT_ACTIVE, ERROR_SUCCESS, HANDLE,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST,
    GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
};
use windows_sys::Win32::NetworkManagement::WiFi::{
    dot11_BSS_type_infrastructure, wlan_connection_mode_profile,
    wlan_interface_state_connected as WLAN_CONNECTED,
    wlan_interface_state_disconnected as WLAN_DISCONNECTED,
    wlan_interface_state_not_ready as WLAN_NOT_READY,
    wlan_intf_opcode_current_connection, wlan_intf_opcode_interface_state,
    wlan_notification_acm_connection_complete, wlan_notification_acm_disconnected,
    WlanCloseHandle, WlanConnect,
    WlanEnumInterfaces,
    WlanFreeMemory, WlanGetAvailableNetworkList, WlanOpenHandle, WlanQueryInterface,
    WlanReasonCodeToString, WlanRegisterNotification, WlanScan, WlanSetProfile, DOT11_SSID,
    L2_NOTIFICATION_DATA, WLAN_API_VERSION_2_0,
    WLAN_AVAILABLE_NETWORK, WLAN_AVAILABLE_NETWORK_LIST, WLAN_CONNECTION_ATTRIBUTES,
    WLAN_CONNECTION_NOTIFICATION_DATA, WLAN_CONNECTION_PARAMETERS, WLAN_INTERFACE_INFO,
    WLAN_INTERFACE_INFO_LIST, WLAN_INTERFACE_STATE, WLAN_NOTIFICATION_SOURCE_ACM,
    WLAN_NOTIFICATION_SOURCE_NONE, WLAN_PROFILE_USER,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN, SOCKET_ADDRESS};

use crate::platform::{PlatformError, WifiConnection, WifiJoinPolicy, WifiJoinRequest};

/// Windows gives no completion signal without registering for
/// notifications; a scan is done within about four seconds.
const SCAN_WAIT: Duration = Duration::from_secs(4);
/// Association plus DHCP.
const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
const WITHHELD: &str = "Windows does not reveal Wi-Fi networks to this client \
     (Location services are off)";

/// What Windows says about the network the interface is on.
enum Association {
    Connected(String),
    NotConnected,
    /// Windows is already changing the connection. Do not interrupt it.
    Connecting,
    /// Windows 11 withholds the name while Location services are off.
    Withheld,
}
const JOIN_POLL: Duration = Duration::from_millis(500);
/// `GetAdaptersAddresses` recommends starting with 15 KB.
const ADAPTERS_BUFFER: u32 = 15 * 1024;

fn failed(what: &str, code: u32) -> PlatformError {
    PlatformError::Failed(format!("{what} failed with error {code}"))
}

fn check(code: u32, what: &str) -> Result<(), PlatformError> {
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(failed(what, code))
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Memory allocated by `wlanapi`, freed on drop.
struct WlanMemory<T>(*mut T);

impl<T> Drop for WlanMemory<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from a wlanapi call that hands over
            // ownership, and is freed exactly once.
            unsafe { WlanFreeMemory(self.0 as *const c_void) };
        }
    }
}

/// A wlanapi client handle and the Wi-Fi interface it works on.
struct Wlan {
    handle: HANDLE,
    interface: GUID,
    /// Kept alive until notifications are unregistered in Drop.
    join_notification: Option<Box<JoinNotification>>,
}

impl Drop for Wlan {
    fn drop(&mut self) {
        if self.join_notification.is_some() {
            // SAFETY: unregistering waits for in-flight callbacks before
            // returning. Their boxed context still lives in `self`.
            unsafe {
                WlanRegisterNotification(
                    self.handle, WLAN_NOTIFICATION_SOURCE_NONE, 0, None,
                    null(), null(), null_mut(),
                );
            }
        }
        // SAFETY: the handle is owned by `self` and closed exactly once.
        unsafe { WlanCloseHandle(self.handle, null()) };
    }
}

impl Wlan {
    /// Prefers an active interface so a disconnected secondary adapter does
    /// not hide the user's connection on another one.
    fn open() -> Result<Wlan, PlatformError> {
        let mut negotiated = 0u32;
        let mut handle: HANDLE = null_mut();
        // SAFETY: plain FFI call with valid out pointers.
        let code = unsafe { WlanOpenHandle(WLAN_API_VERSION_2_0, null(), &mut negotiated, &mut handle) };
        match code {
            ERROR_SUCCESS => {}
            // No WLAN AutoConfig service: a machine without Wi-Fi support
            // (e.g. Windows Server without the wireless feature).
            ERROR_SERVICE_NOT_ACTIVE | ERROR_SERVICE_DOES_NOT_EXIST => {
                return Err(PlatformError::Unsupported(
                    "the WLAN AutoConfig service is not running".into(),
                ))
            }
            code => return Err(failed("WlanOpenHandle", code)),
        }
        let mut wlan = Wlan {
            handle,
            interface: GUID { data1: 0, data2: 0, data3: 0, data4: [0; 8] },
            join_notification: None,
        };

        let mut list: *mut WLAN_INTERFACE_INFO_LIST = null_mut();
        // SAFETY: valid handle and out pointer; the list is freed by the guard.
        let code = unsafe { WlanEnumInterfaces(wlan.handle, null(), &mut list) };
        let list = WlanMemory(list);
        check(code, "WlanEnumInterfaces")?;
        // SAFETY: on success `list` points to a valid list whose array holds
        // `dwNumberOfItems` entries. The array pointer is derived from the
        // list pointer so it may cover all of them.
        unsafe {
            if (*list.0).dwNumberOfItems == 0 {
                return Err(PlatformError::Unsupported("this device has no Wi-Fi interface".into()));
            }
            let interfaces = std::slice::from_raw_parts(
                addr_of!((*list.0).InterfaceInfo).cast::<WLAN_INTERFACE_INFO>(),
                (*list.0).dwNumberOfItems as usize,
            );
            let preferred = interfaces.iter().max_by_key(|interface| match interface.isState {
                WLAN_CONNECTED => 3,
                WLAN_DISCONNECTED => 1,
                WLAN_NOT_READY => 0,
                _ => 2,
            }).expect("the interface list is not empty");
            wlan.interface = preferred.InterfaceGuid;
        }
        Ok(wlan)
    }

    /// The network the interface is connected to, as far as Windows tells.
    fn association(&self) -> Result<Association, PlatformError> {
        // Unlike current_connection, interface_state contains no location
        // information. Association without a DHCP address is still a real
        // connection and must not be mistaken for permission to replace it.
        match self.interface_state()? {
            WLAN_DISCONNECTED | WLAN_NOT_READY => {
                return Ok(Association::NotConnected);
            }
            WLAN_CONNECTED => {}
            _ => return Ok(Association::Connecting),
        }
        let mut size = 0u32;
        let mut data: *mut c_void = null_mut();
        // SAFETY: valid handle, interface and out pointers; the data is
        // freed by the guard.
        let code = unsafe {
            WlanQueryInterface(
                self.handle,
                &self.interface,
                wlan_intf_opcode_current_connection,
                null(),
                &mut size,
                &mut data,
                null_mut(),
            )
        };
        let data = WlanMemory(data.cast::<WLAN_CONNECTION_ATTRIBUTES>());
        match code {
            ERROR_SUCCESS => {}
            // The state changed after interface_state reported connected.
            // Wait for a fresh state query before treating it as idle.
            ERROR_INVALID_STATE => return Ok(Association::Connecting),
            ERROR_ACCESS_DENIED => return Ok(Association::Withheld),
            code => return Err(failed("WlanQueryInterface", code)),
        }
        if data.0.is_null() || (size as usize) < std::mem::size_of::<WLAN_CONNECTION_ATTRIBUTES>() {
            return Err(PlatformError::Failed("WlanQueryInterface returned too little data".into()));
        }
        // SAFETY: checked above that the buffer holds the structure.
        let attributes = unsafe { &*data.0 };
        if attributes.isState != WLAN_CONNECTED {
            return Ok(Association::Connecting);
        }
        Ok(Association::Connected(ssid_string(&attributes.wlanAssociationAttributes.dot11Ssid)))
    }

    fn interface_state(&self) -> Result<WLAN_INTERFACE_STATE, PlatformError> {
        let mut size = 0u32;
        let mut data = null_mut();
        // SAFETY: valid handle and output pointers; successful output is
        // checked for size and freed by WlanMemory.
        let code = unsafe {
            WlanQueryInterface(
                self.handle,
                &self.interface,
                wlan_intf_opcode_interface_state,
                null(),
                &mut size,
                &mut data,
                null_mut(),
            )
        };
        let data = WlanMemory(data.cast::<WLAN_INTERFACE_STATE>());
        check(code, "querying Wi-Fi interface state")?;
        if data.0.is_null() || (size as usize) < std::mem::size_of::<WLAN_INTERFACE_STATE>() {
            return Err(PlatformError::Failed("Wi-Fi interface state returned too little data".into()));
        }
        // SAFETY: checked above that the buffer holds the state.
        Ok(unsafe { *data.0 })
    }
}

const JOIN_PENDING: u32 = u32::MAX;
const JOIN_REPLACED: u32 = u32::MAX - 1;

struct JoinNotification {
    interface: GUID,
    ssid: String,
    /// A WLAN reason code, or one of the sentinel values above.
    result: AtomicU32,
}

impl JoinNotification {
    fn record(&self, result: u32) {
        // A successful selection of another network ends this request. Later
        // automatic retries must not turn it back into our successful join.
        let _ = self.result.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |previous| {
            (previous != JOIN_REPLACED).then_some(result)
        });
    }
}

/// ACM notifications do not request location access. They prove that the
/// requested SSID connected, unlike an IP address left from another network.
unsafe extern "system" fn joined(notification: *mut L2_NOTIFICATION_DATA, context: *mut c_void) {
    if notification.is_null() || context.is_null() {
        return;
    }
    // SAFETY: wlanapi owns the notification for this callback. The boxed
    // context is kept alive until unregistration has waited for callbacks.
    let (notification, context) = unsafe { (&*notification, &*context.cast::<JoinNotification>()) };
    let interface = &notification.InterfaceGuid;
    if notification.NotificationSource != WLAN_NOTIFICATION_SOURCE_ACM
        || interface.data1 != context.interface.data1
        || interface.data2 != context.interface.data2
        || interface.data3 != context.interface.data3
        || interface.data4 != context.interface.data4
    {
        return;
    }
    if notification.NotificationCode == wlan_notification_acm_disconnected as u32 {
        // Invalidate an earlier association, but retain a failed attempt's
        // reason for the deadline and never erase another network's selection.
        let _ = context.result.compare_exchange(
            ERROR_SUCCESS, JOIN_PENDING, Ordering::SeqCst, Ordering::SeqCst,
        );
        return;
    }
    if notification.NotificationCode != wlan_notification_acm_connection_complete as u32
        || notification.pData.is_null()
        || (notification.dwDataSize as usize) < std::mem::size_of::<WLAN_CONNECTION_NOTIFICATION_DATA>()
    {
        return;
    }
    // SAFETY: this notification code carries the structure, whose size was
    // checked above; the data is valid for this callback.
    let connection = unsafe { &*notification.pData.cast::<WLAN_CONNECTION_NOTIFICATION_DATA>() };
    let result = if ssid_string(&connection.dot11Ssid) == context.ssid {
        connection.wlanReasonCode
    } else if connection.wlanReasonCode == ERROR_SUCCESS {
        JOIN_REPLACED
    } else {
        // AutoConfig may attempt other profiles while our network is still
        // becoming available. A failed attempt is not a new selection.
        return;
    };
    context.record(result);
}

/// SSIDs are bytes, usually but not necessarily UTF-8.
fn ssid_string(ssid: &DOT11_SSID) -> String {
    let length = (ssid.uSSIDLength as usize).min(ssid.ucSSID.len());
    String::from_utf8_lossy(&ssid.ucSSID[..length]).into_owned()
}

/// `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`, the form `AdapterName` uses.
fn guid_string(guid: &GUID) -> String {
    let d = guid.data4;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        guid.data1, guid.data2, guid.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
    )
}

/// The IPv4 address in a socket address, if it holds one.
///
/// # Safety
/// `address` must come from `GetAdaptersAddresses` and its buffer must still
/// be alive.
unsafe fn ipv4(address: &SOCKET_ADDRESS) -> Option<Ipv4Addr> {
    if address.lpSockaddr.is_null()
        || (address.iSockaddrLength as usize) < std::mem::size_of::<SOCKADDR_IN>()
    {
        return None;
    }
    // SAFETY: non-null and long enough for a SOCKADDR_IN, per the caller.
    unsafe {
        if (*address.lpSockaddr).sa_family != AF_INET {
            return None;
        }
        let socket = &*address.lpSockaddr.cast::<SOCKADDR_IN>();
        // Stored in network byte order, i.e. in address order in memory.
        Some(Ipv4Addr::from(socket.sin_addr.S_un.S_addr.to_ne_bytes()))
    }
}

/// IPv4 address and gateway of the adapter behind a Wi-Fi interface.
fn adapter_addresses(interface: &GUID) -> Result<(Option<IpAddr>, Option<IpAddr>), PlatformError> {
    let name = guid_string(interface);
    let flags = GAA_FLAG_INCLUDE_GATEWAYS
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size = ADAPTERS_BUFFER;
    // `u64` keeps the buffer aligned for the structures written into it.
    let mut buffer: Vec<u64>;
    let mut attempts = 0;
    loop {
        buffer = vec![0u64; (size as usize).div_ceil(8)];
        // SAFETY: the buffer is `size` bytes long and suitably aligned.
        let code = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_INET),
                flags,
                null(),
                buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
                &mut size,
            )
        };
        attempts += 1;
        match code {
            ERROR_SUCCESS => break,
            // Adapters can appear between the two calls.
            ERROR_BUFFER_OVERFLOW if attempts < 4 => continue,
            ERROR_NO_DATA => return Ok((None, None)),
            code => return Err(failed("GetAdaptersAddresses", code)),
        }
    }

    let mut adapter = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    // SAFETY: a linked list inside `buffer`, which outlives the walk.
    unsafe {
        while !adapter.is_null() {
            let current = &*adapter;
            adapter = current.Next;
            if current.AdapterName.is_null() {
                continue;
            }
            let adapter_name = CStr::from_ptr(current.AdapterName.cast()).to_string_lossy();
            if !adapter_name.eq_ignore_ascii_case(&name) {
                continue;
            }
            let mut ip = None;
            let mut unicast = current.FirstUnicastAddress;
            while !unicast.is_null() && ip.is_none() {
                ip = ipv4(&(*unicast).Address);
                unicast = (*unicast).Next;
            }
            let mut gateway = None;
            let mut next = current.FirstGatewayAddress;
            while !next.is_null() && gateway.is_none() {
                gateway = ipv4(&(*next).Address).filter(|address| !address.is_unspecified());
                next = (*next).Next;
            }
            return Ok((ip.map(IpAddr::V4), gateway.map(IpAddr::V4)));
        }
    }
    Ok((None, None))
}

pub fn scan() -> Result<Vec<String>, PlatformError> {
    let wlan = Wlan::open()?;
    // SAFETY: valid handle and interface; no SSID or IE data.
    let code = unsafe { WlanScan(wlan.handle, &wlan.interface, null(), null(), null()) };
    if code == ERROR_SUCCESS {
        std::thread::sleep(SCAN_WAIT);
    } else {
        // E.g. a scan already running: the list below is still useful.
        tracing::debug!(code, "WlanScan failed; using the networks seen so far");
    }

    let mut list: *mut WLAN_AVAILABLE_NETWORK_LIST = null_mut();
    // SAFETY: valid handle, interface and out pointer; freed by the guard.
    let code = unsafe { WlanGetAvailableNetworkList(wlan.handle, &wlan.interface, 0, null(), &mut list) };
    let list = WlanMemory(list);
    if code == ERROR_ACCESS_DENIED {
        return Err(PlatformError::AccessDenied(WITHHELD.into()));
    }
    check(code, "WlanGetAvailableNetworkList")?;
    // SAFETY: on success the list holds `dwNumberOfItems` entries; the array
    // pointer is derived from the list pointer.
    let networks = unsafe {
        std::slice::from_raw_parts(
            addr_of!((*list.0).Network).cast::<WLAN_AVAILABLE_NETWORK>(),
            (*list.0).dwNumberOfItems as usize,
        )
    };
    let mut ssids: Vec<String> = Vec::new();
    for network in networks {
        let ssid = ssid_string(&network.dot11Ssid);
        // Hidden networks have no SSID; a network with a profile is listed
        // twice.
        if !ssid.is_empty() && !ssids.contains(&ssid) {
            ssids.push(ssid);
        }
    }
    Ok(ssids)
}

pub fn current() -> Result<Option<WifiConnection>, PlatformError> {
    let wlan = Wlan::open()?;
    let ssid = match wlan.association()? {
        Association::Connected(ssid) => Some(ssid),
        Association::NotConnected => return Ok(None),
        Association::Withheld | Association::Connecting => None,
    };
    let (ip, gateway) = adapter_addresses(&wlan.interface)?;
    Ok(Some(WifiConnection { ssid, ip, gateway }))
}

/// An address from a DHCP server rather than the link-local fallback.
fn usable(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(ip) if !ip.is_link_local() && !ip.is_unspecified() && !ip.is_loopback())
}

fn reason_text(reason: u32) -> String {
    let mut buffer = [0u16; 512];
    // SAFETY: the buffer length is passed along with the buffer.
    let code = unsafe { WlanReasonCodeToString(reason, buffer.len() as u32, buffer.as_mut_ptr(), null()) };
    let length = buffer.iter().position(|&unit| unit == 0).unwrap_or(buffer.len());
    if code == ERROR_SUCCESS && length > 0 {
        String::from_utf16_lossy(&buffer[..length])
    } else {
        format!("reason code {reason}")
    }
}

pub fn join(request: WifiJoinRequest) -> Result<(), PlatformError> {
    let mut wlan = Wlan::open()?;
    let preserve_connection = |wlan: &Wlan| -> Result<(), PlatformError> {
        let allowed = match &request.policy {
            WifiJoinPolicy::Replace => true,
            WifiJoinPolicy::Disconnected => matches!(wlan.association()?, Association::NotConnected),
            WifiJoinPolicy::RecoverFrom(ssid) => match wlan.association()? {
                Association::NotConnected => true,
                Association::Connected(current) => current == *ssid,
                Association::Connecting | Association::Withheld => false,
            },
        };
        if !allowed {
            return Err(PlatformError::Failed(
                "Wi-Fi connected while discovering the hotspot; preserving that connection".into(),
            ));
        }
        Ok(())
    };
    preserve_connection(&wlan)?;
    let ssid = request.ssid.as_str();
    let xml = wide(&profile_xml(ssid, request.passphrase.as_deref(), request.auto_connect));
    let store = |flags: u32| {
        let mut reason = 0u32;
        // SAFETY: valid handle, interface and NUL-terminated profile XML;
        // the default security descriptor; overwrite an existing profile.
        let code = unsafe {
            WlanSetProfile(wlan.handle, &wlan.interface, flags, xml.as_ptr(), null(), 1, null(), &mut reason)
        };
        (code, reason)
    };
    let (mut code, mut reason) = store(0);
    if code == ERROR_ACCESS_DENIED {
        // An all-user profile needs administrator rights. The service has
        // them; a console run by a user may not.
        (code, reason) = store(WLAN_PROFILE_USER);
    }
    if code != ERROR_SUCCESS {
        return Err(PlatformError::Failed(format!(
            "could not store the Wi-Fi profile for {ssid}: error {code}, {}",
            reason_text(reason)
        )));
    }

    if matches!(wlan.association()?, Association::Connected(current) if current == ssid)
        && adapter_addresses(&wlan.interface)?.0.is_some_and(usable)
    {
        return Ok(());
    }
    let notification = Box::new(JoinNotification {
        interface: wlan.interface,
        ssid: ssid.to_owned(),
        result: AtomicU32::new(JOIN_PENDING),
    });
    // SAFETY: this context stays at the same boxed address until Wlan::drop
    // unregisters callbacks. ACM does not require location access.
    let code = unsafe {
        WlanRegisterNotification(
            wlan.handle, WLAN_NOTIFICATION_SOURCE_ACM, 0, Some(joined),
            (&*notification as *const JoinNotification).cast(), null(), null_mut(),
        )
    };
    check(code, "registering Wi-Fi connection notifications")?;
    wlan.join_notification = Some(notification);

    // The profile is named after the SSID.
    let profile = wide(ssid);
    let parameters = WLAN_CONNECTION_PARAMETERS {
        wlanConnectionMode: wlan_connection_mode_profile,
        strProfile: profile.as_ptr(),
        pDot11Ssid: null_mut(),
        pDesiredBssidList: null_mut(),
        dot11BssType: dot11_BSS_type_infrastructure,
        dwFlags: 0,
    };
    preserve_connection(&wlan)?;
    // SAFETY: valid handle, interface and parameters; `profile` outlives
    // the call.
    let code = unsafe { WlanConnect(wlan.handle, &wlan.interface, &parameters, null()) };
    if code != ERROR_SUCCESS {
        return Err(PlatformError::Failed(format!("could not connect to {ssid}: error {code}")));
    }

    // `WlanConnect` only starts the connection. Ready means associated with
    // this network and an address from its DHCP server (not a link-local
    // fallback).
    wait_for_join(ssid, JOIN_TIMEOUT, || {
        let notification = wlan.join_notification.as_ref().expect("registered above");
        let confirmed = notification.result.load(Ordering::SeqCst);
        if confirmed == JOIN_REPLACED {
            return Ok((confirmed, false));
        }
        let association = wlan.association()?;
        let withheld = matches!(association, Association::Withheld);
        let associated = match association {
            Association::Connected(current) => current == ssid,
            Association::NotConnected | Association::Connecting => false,
            Association::Withheld => confirmed == ERROR_SUCCESS,
        };
        let ready = associated && adapter_addresses(&wlan.interface)?.0.is_some_and(usable);
        // Recheck after the native queries so a concurrent manual selection
        // takes precedence over an address observed for the requested network.
        let confirmed = notification.result.load(Ordering::SeqCst);
        Ok((confirmed, ready && (!withheld || confirmed == ERROR_SUCCESS)))
    }, std::thread::sleep)
}

/// AutoConfig can report a failed attempt and then associate successfully a
/// few seconds later. Observe until the existing deadline without reconnecting.
fn wait_for_join(
    ssid: &str,
    timeout: Duration,
    mut observe: impl FnMut() -> Result<(u32, bool), PlatformError>,
    mut wait: impl FnMut(Duration),
) -> Result<(), PlatformError> {
    let deadline = Instant::now() + timeout;
    loop {
        let (confirmed, ready) = observe()?;
        if confirmed == JOIN_REPLACED {
            return Err(PlatformError::Failed(
                "another Wi-Fi network was selected while joining; preserving that selection".into(),
            ));
        }
        if ready {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let detail = match confirmed {
                ERROR_SUCCESS | JOIN_PENDING =>
                    "wrong passphrase, out of range, or no address from the network".into(),
                reason => reason_text(reason),
            };
            return Err(PlatformError::Failed(format!(
                "could not connect to {ssid} within {} s ({detail})", timeout.as_secs()
            )));
        }
        wait(JOIN_POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

fn xml_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// A WLAN profile for `ssid`, named after it: WPA2-Personal (AES) with a
/// passphrase, or open without one. `auto_connect` decides whether Windows
/// connects to it later by itself.
///
/// The SSID is given both as hex (exact bytes, which Windows prefers) and
/// as a name.
pub fn profile_xml(ssid: &str, passphrase: Option<&str>, auto_connect: bool) -> String {
    let name = xml_escape(ssid);
    let hex: String = ssid.bytes().map(|byte| format!("{byte:02X}")).collect();
    let mode = if auto_connect { "auto" } else { "manual" };
    let security = match passphrase {
        Some(passphrase) => format!(
            "<authEncryption>\
             <authentication>WPA2PSK</authentication>\
             <encryption>AES</encryption>\
             <useOneX>false</useOneX>\
             </authEncryption>\
             <sharedKey>\
             <keyType>passPhrase</keyType>\
             <protected>false</protected>\
             <keyMaterial>{}</keyMaterial>\
             </sharedKey>",
            xml_escape(passphrase)
        ),
        None => "<authEncryption>\
                 <authentication>open</authentication>\
                 <encryption>none</encryption>\
                 <useOneX>false</useOneX>\
                 </authEncryption>"
            .to_owned(),
    };
    format!(
        "<?xml version=\"1.0\"?>\
         <WLANProfile xmlns=\"http://www.microsoft.com/networking/WLAN/profile/v1\">\
         <name>{name}</name>\
         <SSIDConfig><SSID><hex>{hex}</hex><name>{name}</name></SSID></SSIDConfig>\
         <connectionType>ESS</connectionType>\
         <connectionMode>{mode}</connectionMode>\
         <autoSwitch>false</autoSwitch>\
         <MSM><security>{security}</security></MSM>\
         </WLANProfile>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join_notification() -> JoinNotification {
        JoinNotification {
            interface: GUID { data1: 42, data2: 0, data3: 0, data4: [0; 8] },
            ssid: "Office".into(),
            result: AtomicU32::new(JOIN_PENDING),
        }
    }

    fn notify_join(context: &JoinNotification, ssid: &str, reason: u32, disconnected: bool) {
        // SAFETY: all fields are integers, fixed arrays, or raw pointers;
        // zero initializes a valid fixture before filling the relevant fields.
        let mut connection: WLAN_CONNECTION_NOTIFICATION_DATA = unsafe { std::mem::zeroed() };
        connection.dot11Ssid.uSSIDLength = ssid.len() as u32;
        connection.dot11Ssid.ucSSID[..ssid.len()].copy_from_slice(ssid.as_bytes());
        connection.wlanReasonCode = reason;
        let mut notification = L2_NOTIFICATION_DATA {
            NotificationSource: WLAN_NOTIFICATION_SOURCE_ACM,
            NotificationCode: if disconnected {
                wlan_notification_acm_disconnected as u32
            } else {
                wlan_notification_acm_connection_complete as u32
            },
            InterfaceGuid: context.interface,
            dwDataSize: std::mem::size_of_val(&connection) as u32,
            pData: (&mut connection as *mut WLAN_CONNECTION_NOTIFICATION_DATA).cast(),
        };
        // SAFETY: the structures live throughout this synchronous callback;
        // it only reads context fields and writes the atomic result.
        unsafe { joined(&mut notification, std::ptr::from_ref(context).cast_mut().cast()) };
    }

    fn element<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        xml.match_indices(&open)
            .map(|(start, _)| {
                let rest = &xml[start + open.len()..];
                &rest[..rest.find(&close).expect("element is closed")]
            })
            .collect()
    }

    #[test]
    fn profile_for_a_wpa2_network_carries_the_passphrase() {
        let xml = profile_xml("Office", Some("fake-passphrase-123"), true);
        assert_eq!(element(&xml, "name"), ["Office", "Office"]);
        assert_eq!(element(&xml, "hex"), ["4F6666696365"]);
        assert_eq!(element(&xml, "connectionMode"), ["auto"]);
        assert_eq!(element(&xml, "authentication"), ["WPA2PSK"]);
        assert_eq!(element(&xml, "encryption"), ["AES"]);
        assert_eq!(element(&xml, "keyType"), ["passPhrase"]);
        assert_eq!(element(&xml, "protected"), ["false"]);
        assert_eq!(element(&xml, "keyMaterial"), ["fake-passphrase-123"]);
        assert!(xml.starts_with("<?xml version=\"1.0\"?><WLANProfile xmlns=\"http://www.microsoft.com/networking/WLAN/profile/v1\">"));
        assert!(xml.ends_with("</WLANProfile>"));
    }

    #[test]
    fn profile_for_an_open_network_has_no_key() {
        let xml = profile_xml("Guest", None, false);
        assert_eq!(element(&xml, "connectionMode"), ["manual"]);
        assert_eq!(element(&xml, "autoSwitch"), ["false"]);
        assert_eq!(element(&xml, "authentication"), ["open"]);
        assert_eq!(element(&xml, "encryption"), ["none"]);
        assert!(!xml.contains("sharedKey"));
        assert!(!xml.contains("keyMaterial"));
    }

    #[test]
    fn profile_escapes_markup_in_ssid_and_passphrase() {
        let xml = profile_xml(r#"A&B <"x'>"#, Some(r#"p&ss<w>"o'rd"#), false);
        let escaped_ssid = "A&amp;B &lt;&quot;x&apos;&gt;";
        assert_eq!(element(&xml, "name"), [escaped_ssid, escaped_ssid]);
        assert_eq!(element(&xml, "keyMaterial"), ["p&amp;ss&lt;w&gt;&quot;o&apos;rd"]);
        // The hex form has the raw bytes.
        assert_eq!(element(&xml, "hex"), ["412642203C2278273E"]);
    }

    #[test]
    fn profile_hex_is_the_utf8_bytes_of_the_ssid() {
        let xml = profile_xml("Café ✓", None, true);
        assert_eq!(element(&xml, "hex"), ["436166C3A920E29C93"]);
        assert_eq!(element(&xml, "name"), ["Café ✓", "Café ✓"]);
    }

    #[test]
    fn guid_is_formatted_like_an_adapter_name() {
        let guid = GUID {
            data1: 0x4D36E972,
            data2: 0xE325,
            data3: 0x11CE,
            data4: [0xBF, 0xC1, 0x08, 0x00, 0x2B, 0xE1, 0x03, 0x18],
        };
        assert_eq!(guid_string(&guid), "{4D36E972-E325-11CE-BFC1-08002BE10318}");
    }

    #[test]
    fn ssid_bytes_are_decoded_leniently() {
        let mut ssid = DOT11_SSID { uSSIDLength: 4, ucSSID: [0; 32] };
        ssid.ucSSID[..4].copy_from_slice(b"ab\xffc");
        assert_eq!(ssid_string(&ssid), "ab\u{fffd}c");
        // A length beyond the array is not trusted.
        ssid.uSSIDLength = 99;
        assert_eq!(ssid_string(&ssid).chars().count(), 32);
    }

    #[test]
    fn hidden_ssid_join_requires_confirmation_for_the_requested_network() {
        let interface = GUID { data1: 42, data2: 0, data3: 0, data4: [0; 8] };
        let mut context = JoinNotification {
            interface,
            ssid: "Office".into(),
            result: AtomicU32::new(JOIN_PENDING),
        };
        // SAFETY: the notification data is a C structure of integers and
        // fixed arrays; zero is a valid starting value for each field.
        let mut connection: WLAN_CONNECTION_NOTIFICATION_DATA = unsafe { std::mem::zeroed() };
        connection.dot11Ssid.uSSIDLength = 6;
        connection.dot11Ssid.ucSSID[..6].copy_from_slice(b"Office");
        let mut notification = L2_NOTIFICATION_DATA {
            NotificationSource: WLAN_NOTIFICATION_SOURCE_ACM,
            NotificationCode: wlan_notification_acm_connection_complete as u32,
            InterfaceGuid: interface,
            dwDataSize: std::mem::size_of_val(&connection) as u32,
            pData: (&mut connection as *mut WLAN_CONNECTION_NOTIFICATION_DATA).cast(),
        };
        let context_ptr = (&mut context as *mut JoinNotification).cast();
        // A completion from a different adapter cannot verify this join.
        notification.InterfaceGuid.data1 = 99;
        // SAFETY: both pointers refer to the live fixture structures above.
        unsafe { joined(&mut notification, context_ptr) };
        assert_eq!(context.result.load(Ordering::SeqCst), JOIN_PENDING);

        notification.InterfaceGuid = interface;
        notification.dwDataSize = 0;
        unsafe { joined(&mut notification, context_ptr) };
        assert_eq!(context.result.load(Ordering::SeqCst), JOIN_PENDING);

        notification.dwDataSize = std::mem::size_of_val(&connection) as u32;
        unsafe { joined(&mut notification, context_ptr) };
        assert_eq!(context.result.load(Ordering::SeqCst), ERROR_SUCCESS);

        // Losing that association invalidates its earlier success, even
        // while Windows retains an address or automatically joins elsewhere.
        notification.NotificationCode = wlan_notification_acm_disconnected as u32;
        unsafe { joined(&mut notification, context_ptr) };
        assert_eq!(context.result.load(Ordering::SeqCst), JOIN_PENDING);

        notification.NotificationCode = wlan_notification_acm_connection_complete as u32;
        connection.dot11Ssid.ucSSID[..6].copy_from_slice(b"Manual");
        unsafe { joined(&mut notification, context_ptr) };
        assert_eq!(context.result.load(Ordering::SeqCst), JOIN_REPLACED);
    }

    #[test]
    fn transient_join_failure_waits_for_later_matching_success_and_dhcp() {
        let context = join_notification();
        // A real adapter reported network-not-available (163851), then
        // AutoConfig associated with the same SSID four seconds later.
        let mut observations = [
            ("Office", 163851, false, false),
            ("Office", 0, true, false),
            ("Other", 163851, false, false),
            ("Office", ERROR_SUCCESS, false, false),
            ("Office", ERROR_SUCCESS, false, true),
        ].into_iter();
        let mut waits = 0;
        let result = wait_for_join("Office", JOIN_TIMEOUT, || {
            let (ssid, reason, disconnected, ready) = observations.next().expect("join should finish");
            notify_join(&context, ssid, reason, disconnected);
            Ok((context.result.load(Ordering::SeqCst), ready))
        }, |_| waits += 1);
        assert!(result.is_ok());
        assert_eq!(waits, 4);
        assert!(observations.next().is_none());
    }

    #[test]
    fn successful_manual_selection_stays_protected_from_later_callbacks() {
        let context = join_notification();
        notify_join(&context, "Office", 163851, false);
        notify_join(&context, "Manual", ERROR_SUCCESS, false);
        notify_join(&context, "Manual", 0, true);
        notify_join(&context, "Office", ERROR_SUCCESS, false);
        let result = wait_for_join("Office", JOIN_TIMEOUT, || {
            Ok((context.result.load(Ordering::SeqCst), true))
        }, |_| panic!("a successful manual selection must stop the join immediately"));
        assert!(matches!(result, Err(PlatformError::Failed(message))
            if message.contains("preserving that selection")));
    }

    #[test]
    fn join_deadline_reports_failure_even_after_disconnect_or_unrelated_failure() {
        let context = join_notification();
        notify_join(&context, "Office", 163851, false);
        notify_join(&context, "Office", 0, true);
        notify_join(&context, "Other", 12345, false);
        assert_eq!(context.result.load(Ordering::SeqCst), 163851);
        let result = wait_for_join("Office", Duration::ZERO, || {
            Ok((context.result.load(Ordering::SeqCst), false))
        }, |_| panic!("the join must not wait past its deadline"));
        assert!(matches!(result, Err(PlatformError::Failed(message))
            if message.contains("within 0 s") && message.contains(&reason_text(163851))));
    }
}
