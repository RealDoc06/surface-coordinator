//! Moving devices to the Wi-Fi network this computer is on.
//!
//! The manager knows that network's name and saved password, so the operator
//! does not have to type them. The password never goes to the browser: the
//! manager creates the `wifi.join` job itself, through the embedded
//! coordinator's API.
//!
//! On that network the devices report to the online coordinator the operator
//! uses, if there is one; otherwise to this computer.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;

/// The Wi-Fi network this computer is connected to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiNetwork {
    pub ssid: String,
    /// `None` for an open network.
    pub password: Option<String>,
}

/// Reads this computer's current Wi-Fi network. Blocking.
pub trait WifiSource: Send + Sync + 'static {
    /// `Err` says, for the operator, why the network cannot be passed on.
    fn current(&self) -> Result<WifiNetwork, String>;
}

/// The implementation for the OS the manager runs on.
pub fn native() -> Arc<dyn WifiSource> {
    #[cfg(windows)]
    let source: Arc<dyn WifiSource> = Arc::new(windows::SavedProfile);
    #[cfg(not(windows))]
    let source: Arc<dyn WifiSource> = Arc::new(Unsupported);
    source
}

/// No way to read the current network on this OS.
pub struct Unsupported;

impl WifiSource for Unsupported {
    fn current(&self) -> Result<WifiNetwork, String> {
        Err("the manager cannot read this computer's Wi-Fi network on this operating system".into())
    }
}

/// The address other devices on this computer's network reach it at: the
/// local address of the default route. Nothing is sent.
fn lan_address() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let address = socket.local_addr().ok()?.ip();
    (!address.is_unspecified() && !address.is_loopback()).then_some(address)
}

/// Whether traffic to other devices on this computer's network would leave
/// through another interface, as with a VPN that routes the same subnet.
/// Devices then cannot connect here: the firewall drops replies that go out
/// by a different interface than the request came in on.
fn lan_route_problem(lan: IpAddr) -> Option<String> {
    let IpAddr::V4(address) = lan else { return None };
    // A neighbour: the same address with the last bit flipped.
    let mut octets = address.octets();
    octets[3] ^= 1;
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect((std::net::Ipv4Addr::from(octets), 9)).ok()?;
    let source = socket.local_addr().ok()?.ip();
    (source != lan).then(|| {
        format!(
            "Traffic to this network ({lan}) leaves this computer through another interface \
             ({source}), probably a VPN that routes the same addresses. Devices will not be able \
             to connect to this computer over the Wi-Fi until that is turned off."
        )
    })
}

/// How the manager reaches the embedded coordinator's API.
#[derive(Clone)]
struct Api {
    origin: String,
    token: String,
}

/// The address clients use for the coordinator whose web UI is at `origin`:
/// `https://host[:port]` → `wss://host[:port]`, `http://` → `ws://`. Clients
/// and browsers share the host and port (see `deploy/README.md`).
pub fn client_url(origin: &str) -> Option<String> {
    let mut url = Url::parse(origin).ok()?;
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        _ => return None,
    };
    url.set_scheme(scheme).ok()?;
    url.host_str()?;
    Some(url.origin().ascii_serialization())
}

pub struct WifiService {
    source: Arc<dyn WifiSource>,
    /// Port devices connect to on this computer.
    client_port: AtomicU16,
    api: Mutex<Option<Api>>,
    /// Client address of the online coordinator, when the operator uses one.
    online: Mutex<Option<String>>,
}

impl WifiService {
    pub fn new(source: Arc<dyn WifiSource>) -> WifiService {
        WifiService { source, client_port: AtomicU16::new(0), api: Mutex::new(None), online: Mutex::new(None) }
    }

    /// Called once the embedded coordinator is up.
    pub fn connect(&self, origin: String, token: String, client_port: u16) {
        self.client_port.store(client_port, Ordering::Relaxed);
        *self.api.lock().unwrap_or_else(|e| e.into_inner()) = Some(Api { origin, token });
    }

    /// Sends moved devices to the online coordinator at `origin` (its web
    /// address), or with `None` to this computer.
    pub fn set_online_coordinator(&self, origin: Option<&str>) {
        *self.online.lock().unwrap_or_else(|e| e.into_inner()) = origin.and_then(client_url);
    }

    fn online_url(&self) -> Option<String> {
        self.online.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    async fn network(&self) -> Result<WifiNetwork, String> {
        let source = self.source.clone();
        tokio::task::spawn_blocking(move || source.current())
            .await
            .unwrap_or_else(|error| Err(error.to_string()))
    }

    /// Where moved devices find this computer on that network.
    fn lan_url(&self) -> Option<String> {
        let port = self.client_port.load(Ordering::Relaxed);
        lan_address().map(|address| format!("ws://{}", SocketAddr::new(address, port)))
    }

    /// Where moved devices report: the online coordinator, otherwise this
    /// computer. `"online"` or `"local"`, and the address.
    fn destination(&self) -> (&'static str, Option<String>) {
        match self.online_url() {
            Some(url) => ("online", Some(url)),
            None => ("local", self.lan_url()),
        }
    }
}

fn error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": {"code": code, "message": message.into()}}))).into_response()
}

/// `GET /wifi`: what the button would move devices to. Never the password.
async fn show(State(service): State<Arc<WifiService>>) -> Json<Value> {
    let (coordinator, coordinator_url) = service.destination();
    Json(match service.network().await {
        Ok(network) if coordinator_url.is_some() => json!({
            "available": true,
            "ssid": network.ssid,
            "secured": network.password.is_some(),
            "coordinator": coordinator,
            "coordinator_url": coordinator_url,
            "reason": null,
            // Only devices coming back to this computer need a route to it.
            "warning": if coordinator == "local" { lan_address().and_then(lan_route_problem) } else { None },
        }),
        Ok(_) => json!({
            "available": false, "ssid": null, "secured": false,
            "coordinator": coordinator, "coordinator_url": null,
            "reason": "this computer has no network address devices could reach it at",
        }),
        Err(reason) => json!({
            "available": false, "ssid": null, "secured": false,
            "coordinator": coordinator, "coordinator_url": null,
            "reason": reason,
        }),
    })
}

#[derive(Deserialize)]
struct Move {
    targets: Value,
}

/// `POST /wifi/move {targets}`: creates the `wifi.join` job for this
/// computer's network and answers with the coordinator's own response.
async fn move_devices(State(service): State<Arc<WifiService>>, Json(request): Json<Move>) -> Response {
    let network = match service.network().await {
        Ok(network) => network,
        Err(reason) => return error(StatusCode::CONFLICT, "wifi_unavailable", reason),
    };
    let (_, Some(coordinator_url)) = service.destination() else {
        return error(
            StatusCode::CONFLICT,
            "wifi_unavailable",
            "this computer has no network address devices could reach it at",
        );
    };
    let api = service.api.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(api) = api else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "internal", "the coordinator is not ready");
    };

    let mut payload = json!({ "ssid": network.ssid, "coordinator_url": coordinator_url });
    if let Some(password) = network.password {
        payload["password"] = json!(password);
    }
    let job = json!({ "kind": "wifi.join", "payload": payload, "targets": request.targets });
    let sent = reqwest::Client::new()
        .post(format!("{}/api/v1/jobs", api.origin))
        .bearer_auth(api.token)
        .json(&job)
        .send()
        .await;
    match sent {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let body: Value = response.json().await.unwrap_or(Value::Null);
            (status, Json(body)).into_response()
        }
        Err(failure) => error(StatusCode::BAD_GATEWAY, "internal", failure.to_string()),
    }
}

pub fn router(service: Arc<WifiService>) -> Router {
    Router::new().route("/wifi", get(show)).route("/wifi/move", post(move_devices)).with_state(service)
}

/// What a saved Windows Wi-Fi profile says about the network's security.
#[cfg_attr(not(windows), allow(dead_code))]
fn password_from_profile(xml: &str) -> Result<Option<String>, String> {
    let element = |tag: &str| -> Option<&str> {
        let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
        let end = xml[start..].find(&format!("</{tag}>"))? + start;
        Some(&xml[start..end])
    };
    match element("authentication") {
        Some("open") if element("keyMaterial").is_none() => return Ok(None),
        Some("WPA2PSK" | "WPAPSK" | "WPA3SAE" | "open") => {}
        Some(other) => {
            return Err(format!(
                "this computer's Wi-Fi uses {other} sign-in, which cannot be handed to devices as a password"
            ))
        }
        None => return Err("the saved Wi-Fi profile could not be read".into()),
    }
    if element("protected") != Some("false") {
        return Err(
            "Windows did not release the saved Wi-Fi password; use \"Move to Wi-Fi…\" and type it".into()
        );
    }
    let key = element("keyMaterial").ok_or("the saved Wi-Fi profile has no password")?;
    Ok(Some(
        key.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    ))
}

#[cfg(windows)]
mod windows {
    //! The current connection's saved profile, through the Native Wi-Fi API.

    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    use windows_sys::core::GUID;
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
    use windows_sys::Win32::NetworkManagement::WiFi::{
        wlan_interface_state_connected, wlan_intf_opcode_current_connection, WlanCloseHandle,
        WlanEnumInterfaces, WlanFreeMemory, WlanGetProfile, WlanOpenHandle, WlanQueryInterface,
        WLAN_API_VERSION_2_0, WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO,
        WLAN_INTERFACE_INFO_LIST, WLAN_PROFILE_GET_PLAINTEXT_KEY,
    };

    use super::{password_from_profile, WifiNetwork, WifiSource};

    pub struct SavedProfile;

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: opened by WlanOpenHandle and closed once.
            unsafe { WlanCloseHandle(self.0, null()) };
        }
    }

    /// Memory allocated by the WLAN API.
    struct Memory<T>(*mut T);

    impl<T> Drop for Memory<T> {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: allocated by the WLAN API and freed once.
                unsafe { WlanFreeMemory(self.0.cast::<c_void>()) };
            }
        }
    }

    fn wide_text(units: &[u16]) -> String {
        let length = units.iter().position(|&unit| unit == 0).unwrap_or(units.len());
        String::from_utf16_lossy(&units[..length])
    }

    impl WifiSource for SavedProfile {
        fn current(&self) -> Result<WifiNetwork, String> {
            let not_connected = || "this computer is not connected to a Wi-Fi network".to_owned();
            let mut version = 0u32;
            let mut handle: HANDLE = null_mut();
            // SAFETY: valid out pointers.
            if unsafe { WlanOpenHandle(WLAN_API_VERSION_2_0, null(), &mut version, &mut handle) } != ERROR_SUCCESS {
                return Err("Wi-Fi is not available on this computer".into());
            }
            let handle = Handle(handle);

            let mut list: *mut WLAN_INTERFACE_INFO_LIST = null_mut();
            // SAFETY: valid handle and out pointer; freed by the guard.
            let code = unsafe { WlanEnumInterfaces(handle.0, null(), &mut list) };
            let list = Memory(list);
            if code != ERROR_SUCCESS || list.0.is_null() {
                return Err("Wi-Fi is not available on this computer".into());
            }
            // SAFETY: on success the list holds `dwNumberOfItems` entries.
            let interfaces: Vec<GUID> = unsafe {
                std::slice::from_raw_parts(
                    std::ptr::addr_of!((*list.0).InterfaceInfo).cast::<WLAN_INTERFACE_INFO>(),
                    (*list.0).dwNumberOfItems as usize,
                )
                .iter()
                .map(|interface| interface.InterfaceGuid)
                .collect()
            };

            for interface in interfaces {
                let mut size = 0u32;
                let mut data: *mut c_void = null_mut();
                // SAFETY: valid handle, interface and out pointers.
                let code = unsafe {
                    WlanQueryInterface(
                        handle.0,
                        &interface,
                        wlan_intf_opcode_current_connection,
                        null(),
                        &mut size,
                        &mut data,
                        null_mut(),
                    )
                };
                let data = Memory(data.cast::<WLAN_CONNECTION_ATTRIBUTES>());
                if code != ERROR_SUCCESS
                    || data.0.is_null()
                    || (size as usize) < std::mem::size_of::<WLAN_CONNECTION_ATTRIBUTES>()
                {
                    continue;
                }
                // SAFETY: checked above that the buffer holds the structure.
                let attributes = unsafe { &*data.0 };
                if attributes.isState != wlan_interface_state_connected {
                    continue;
                }
                let ssid = &attributes.wlanAssociationAttributes.dot11Ssid;
                let length = (ssid.uSSIDLength as usize).min(ssid.ucSSID.len());
                let ssid = String::from_utf8_lossy(&ssid.ucSSID[..length]).into_owned();

                let mut xml: *mut u16 = null_mut();
                let mut flags = WLAN_PROFILE_GET_PLAINTEXT_KEY;
                // SAFETY: valid handle and interface; the profile name is a
                // NUL-terminated array inside `attributes`; the XML is freed
                // by the guard.
                let code = unsafe {
                    WlanGetProfile(
                        handle.0,
                        &interface,
                        attributes.strProfileName.as_ptr(),
                        null(),
                        &mut xml,
                        &mut flags,
                        null_mut(),
                    )
                };
                let xml = Memory(xml);
                if code != ERROR_SUCCESS || xml.0.is_null() {
                    return Err(format!("the saved profile of {ssid} could not be read (error {code})"));
                }
                // SAFETY: a NUL-terminated string allocated by the API.
                let text = unsafe {
                    let mut length = 0;
                    while *xml.0.add(length) != 0 {
                        length += 1;
                    }
                    wide_text(std::slice::from_raw_parts(xml.0, length))
                };
                let password = password_from_profile(&text)?;
                return Ok(WifiNetwork { ssid, password });
            }
            Err(not_connected())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(authentication: &str, security: &str) -> String {
        format!(
            "<WLANProfile><name>n</name><MSM><security><authEncryption>\
             <authentication>{authentication}</authentication><encryption>AES</encryption>\
             </authEncryption>{security}</security></MSM></WLANProfile>"
        )
    }

    const KEY: &str = "<sharedKey><keyType>passPhrase</keyType><protected>false</protected>\
                       <keyMaterial>fake &amp; &lt;fixture&gt;</keyMaterial></sharedKey>";

    #[test]
    fn the_password_of_a_personal_network_is_read_and_unescaped() {
        for authentication in ["WPA2PSK", "WPA3SAE"] {
            assert_eq!(
                password_from_profile(&profile(authentication, KEY)),
                Ok(Some("fake & <fixture>".to_owned()))
            );
        }
    }

    #[test]
    fn an_open_network_has_no_password() {
        assert_eq!(password_from_profile(&profile("open", "")), Ok(None));
    }

    #[test]
    fn a_password_windows_keeps_encrypted_is_not_usable() {
        let protected = KEY.replace("<protected>false", "<protected>true");
        assert!(password_from_profile(&profile("WPA2PSK", &protected)).unwrap_err().contains("did not release"));
    }

    #[test]
    fn enterprise_networks_cannot_be_passed_on() {
        assert!(password_from_profile(&profile("WPA2", "")).unwrap_err().contains("WPA2 sign-in"));
    }

    #[test]
    fn clients_reach_the_online_coordinator_over_websockets_at_the_same_host_and_port() {
        let cases = [
            ("https://coordinator.example.org", "wss://coordinator.example.org"),
            ("https://coordinator.example.org:8443", "wss://coordinator.example.org:8443"),
            ("https://coordinator.example.org:443", "wss://coordinator.example.org"),
            ("http://192.168.1.10:8080", "ws://192.168.1.10:8080"),
            ("http://[::1]:8080", "ws://[::1]:8080"),
        ];
        for (origin, expected) in cases {
            assert_eq!(client_url(origin).as_deref(), Some(expected), "{origin}");
        }
        for unusable in ["", "coordinator.example.org", "ftp://coordinator.example.org", "file:///C:/x"] {
            assert_eq!(client_url(unusable), None, "{unusable}");
        }
    }

    #[test]
    fn the_real_adapter_can_be_asked_without_panicking() {
        // Read-only. The password is never printed.
        match native().current() {
            Ok(network) => eprintln!("on {:?}, secured: {}", network.ssid, network.password.is_some()),
            Err(reason) => eprintln!("not available: {reason}"),
        }
        eprintln!("LAN address: {:?}", lan_address());
    }
}
