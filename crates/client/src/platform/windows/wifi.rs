//! Wi-Fi through the Native Wi-Fi API (`wlanapi`) and the IP Helper API.
//!
//! `netsh` would be simpler, but its output is localised. Everything here
//! blocks; callers run it on a blocking thread.

use std::ffi::{c_void, CStr};
use std::net::{IpAddr, Ipv4Addr};
use std::ptr::{addr_of, null, null_mut};
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
    dot11_BSS_type_infrastructure, wlan_connection_mode_profile, wlan_interface_state_connected,
    wlan_intf_opcode_current_connection, WlanCloseHandle, WlanConnect, WlanDisconnect,
    WlanEnumInterfaces,
    WlanFreeMemory, WlanGetAvailableNetworkList, WlanOpenHandle, WlanQueryInterface,
    WlanReasonCodeToString, WlanScan, WlanSetProfile, DOT11_SSID, WLAN_API_VERSION_2_0,
    WLAN_AVAILABLE_NETWORK, WLAN_AVAILABLE_NETWORK_LIST, WLAN_CONNECTION_ATTRIBUTES,
    WLAN_CONNECTION_PARAMETERS, WLAN_INTERFACE_INFO, WLAN_INTERFACE_INFO_LIST, WLAN_PROFILE_USER,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN, SOCKET_ADDRESS};

use crate::platform::{PlatformError, WifiConnection, WifiJoinRequest};

/// Windows gives no completion signal without registering for
/// notifications; a scan is done within about four seconds.
const SCAN_WAIT: Duration = Duration::from_secs(4);
/// Association plus DHCP.
const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the old network's address may linger after a disconnect.
const LEAVE_TIMEOUT: Duration = Duration::from_secs(5);
const WITHHELD: &str = "Windows does not reveal Wi-Fi networks to this client \
     (Location services are off)";

/// What Windows says about the network the interface is on.
enum Association {
    Connected(String),
    NotConnected,
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
}

impl Drop for Wlan {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by `self` and closed exactly once.
        unsafe { WlanCloseHandle(self.handle, null()) };
    }
}

impl Wlan {
    /// Opens a handle on the first Wi-Fi interface.
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
        let mut wlan = Wlan { handle, interface: GUID { data1: 0, data2: 0, data3: 0, data4: [0; 8] } };

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
            let first = addr_of!((*list.0).InterfaceInfo).cast::<WLAN_INTERFACE_INFO>();
            wlan.interface = (*first).InterfaceGuid;
        }
        Ok(wlan)
    }

    /// The network the interface is connected to, as far as Windows tells.
    fn association(&self) -> Result<Association, PlatformError> {
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
            // The interface is not connected.
            ERROR_INVALID_STATE => return Ok(Association::NotConnected),
            ERROR_ACCESS_DENIED => return Ok(Association::Withheld),
            code => return Err(failed("WlanQueryInterface", code)),
        }
        if data.0.is_null() || (size as usize) < std::mem::size_of::<WLAN_CONNECTION_ATTRIBUTES>() {
            return Err(PlatformError::Failed("WlanQueryInterface returned too little data".into()));
        }
        // SAFETY: checked above that the buffer holds the structure.
        let attributes = unsafe { &*data.0 };
        if attributes.isState != wlan_interface_state_connected {
            return Ok(Association::NotConnected);
        }
        Ok(Association::Connected(ssid_string(&attributes.wlanAssociationAttributes.dot11Ssid)))
    }
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
        Association::Withheld => None,
    };
    let (ip, gateway) = adapter_addresses(&wlan.interface)?;
    // Without a name, an address is the only sign of a connection.
    if ssid.is_none() && !ip.is_some_and(usable) {
        return Ok(None);
    }
    Ok(Some(WifiConnection { ssid, ip, gateway }))
}

/// An address from a DHCP server rather than the link-local fallback.
fn usable(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(ip) if !ip.is_link_local())
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
    let wlan = Wlan::open()?;
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

    // When Windows withholds the name of the network, being connected can
    // only be told from having an address. Leave the current network first,
    // so that an address seen afterwards is one from the network asked for.
    let blind = matches!(wlan.association()?, Association::Withheld);
    if blind {
        // SAFETY: valid handle and interface.
        unsafe { WlanDisconnect(wlan.handle, &wlan.interface, null()) };
        let deadline = Instant::now() + LEAVE_TIMEOUT;
        while adapter_addresses(&wlan.interface)?.0.is_some_and(usable) && Instant::now() < deadline {
            std::thread::sleep(JOIN_POLL);
        }
    }

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
    // SAFETY: valid handle, interface and parameters; `profile` outlives
    // the call.
    let code = unsafe { WlanConnect(wlan.handle, &wlan.interface, &parameters, null()) };
    if code != ERROR_SUCCESS {
        return Err(PlatformError::Failed(format!("could not connect to {ssid}: error {code}")));
    }

    // `WlanConnect` only starts the connection. Ready means associated with
    // this network and an address from its DHCP server (not a link-local
    // fallback).
    let deadline = Instant::now() + JOIN_TIMEOUT;
    loop {
        let associated = match wlan.association()? {
            Association::Connected(current) => current == ssid,
            Association::NotConnected => false,
            Association::Withheld => true,
        };
        if associated && adapter_addresses(&wlan.interface)?.0.is_some_and(usable) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(PlatformError::Failed(format!(
                "could not connect to {ssid} within {} s (wrong passphrase, out of range, or no address from the network)",
                JOIN_TIMEOUT.as_secs()
            )));
        }
        std::thread::sleep(JOIN_POLL);
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
         <MSM><security>{security}</security></MSM>\
         </WLANProfile>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
