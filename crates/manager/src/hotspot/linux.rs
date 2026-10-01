//! Linux: the hotspot of NetworkManager, through `nmcli`.
//!
//! The argument lists are built here and tested on every OS; only running
//! `nmcli` is Linux-specific. Arguments are passed as argv, never through a
//! shell, so an SSID or passphrase cannot inject anything.

/// Name of the NetworkManager connection the manager creates and reuses.
pub const CONNECTION: &str = "surface-coordinator-hotspot";

/// `nmcli` arguments that create (or update) the hotspot connection and
/// activate it. NetworkManager picks the Wi-Fi device itself.
pub fn nmcli_start_args(ssid: &str, passphrase: &str) -> Vec<String> {
    ["device", "wifi", "hotspot", "con-name", CONNECTION, "ssid", ssid, "password", passphrase]
        .map(str::to_owned)
        .to_vec()
}

/// `nmcli` arguments that deactivate the hotspot connection.
pub fn nmcli_stop_args() -> Vec<String> {
    ["connection", "down", CONNECTION].map(str::to_owned).to_vec()
}

#[cfg(target_os = "linux")]
pub use control::NetworkManager;

#[cfg(target_os = "linux")]
mod control {
    use super::{nmcli_start_args, nmcli_stop_args};
    use crate::hotspot::{BoxFuture, Control, HotspotControl};

    pub struct NetworkManager;

    impl HotspotControl for NetworkManager {
        fn control(&self) -> Control {
            Control::Automatic
        }

        fn start(&self, ssid: &str, passphrase: &str) -> BoxFuture<Result<(), String>> {
            Box::pin(nmcli(nmcli_start_args(ssid, passphrase)))
        }

        fn stop(&self) -> BoxFuture<Result<(), String>> {
            Box::pin(nmcli(nmcli_stop_args()))
        }

        fn instructions(&self, _ssid: &str, _passphrase: &str) -> Vec<String> {
            Vec::new()
        }
    }

    async fn nmcli(args: Vec<String>) -> Result<(), String> {
        let output = tokio::process::Command::new("nmcli")
            .args(&args)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| format!("could not run NetworkManager's nmcli: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(if stderr.is_empty() { format!("nmcli failed ({})", output.status) } else { stderr })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hotspot_is_started_as_a_named_connection_with_the_given_network() {
        assert_eq!(
            nmcli_start_args("SC-0123abcd", "fakepassphrasefakepassph"),
            [
                "device",
                "wifi",
                "hotspot",
                "con-name",
                "surface-coordinator-hotspot",
                "ssid",
                "SC-0123abcd",
                "password",
                "fakepassphrasefakepassph",
            ]
        );
    }

    #[test]
    fn odd_characters_stay_inside_their_own_argument() {
        let args = nmcli_start_args("SC-a b;c", "pass word\"; rm -rf /");
        assert_eq!(args.len(), 9);
        assert_eq!(args[6], "SC-a b;c");
        assert_eq!(args[8], "pass word\"; rm -rf /");
    }

    #[test]
    fn stopping_takes_down_the_same_connection() {
        assert_eq!(nmcli_stop_args(), ["connection", "down", "surface-coordinator-hotspot"]);
    }
}
