//! macOS: no supported API turns on Internet Sharing, so the operator does
//! it by hand with the network name and password the manager shows.

use super::{BoxFuture, Control, HotspotControl};

pub struct InternetSharing;

impl HotspotControl for InternetSharing {
    fn control(&self) -> Control {
        Control::Manual
    }

    fn start(&self, _ssid: &str, _passphrase: &str) -> BoxFuture<Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn stop(&self) -> BoxFuture<Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn instructions(&self, ssid: &str, passphrase: &str) -> Vec<String> {
        instructions(ssid, passphrase)
    }
}

/// Steps to share this Mac's connection over Wi-Fi as the hotspot.
pub fn instructions(ssid: &str, passphrase: &str) -> Vec<String> {
    vec![
        "Open System Settings and go to General → Sharing.".to_owned(),
        "Click the info button next to Internet Sharing.".to_owned(),
        "Set \"Share your connection from\" to a connection other than Wi-Fi, such as Ethernet.".to_owned(),
        "Turn on Wi-Fi under \"To devices using\" and click Wi-Fi Options….".to_owned(),
        format!("Set Network Name to {ssid}."),
        "Set Security to WPA2/WPA3 Personal.".to_owned(),
        format!("Set Password to {passphrase}, click OK, then Done."),
        "Turn on Internet Sharing and confirm.".to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_steps_give_the_network_name_and_password() {
        let steps = instructions("SC-0123abcd", "fakepassphrasefakepassph");
        assert!(steps.iter().any(|step| step.contains("SC-0123abcd")), "{steps:?}");
        assert!(steps.iter().any(|step| step.contains("fakepassphrasefakepassph")), "{steps:?}");
        assert!(steps.iter().any(|step| step.contains("Internet Sharing")), "{steps:?}");
    }
}
