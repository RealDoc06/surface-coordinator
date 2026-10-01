//! The configuration file, `config.json` in the state directory.
//!
//! `client.exe install` writes it; the service reads nothing else. In a
//! console, command-line flags and environment variables override it or
//! stand in for it entirely, as they did before there was a file.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "config.json";

/// What `config.json` holds. Every field may be missing from the file, so
/// that flags can fill the gaps; what is still missing after merging is an
/// error (see [`merge`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// The enrollment secret, which is why `install` restricts the state
    /// directory to SYSTEM and Administrators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_secret: Option<String>,
    /// Without one the client looks for the manager's hotspot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator_url: Option<String>,
}

impl ConfigFile {
    /// The file's content, as `install` writes it.
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).expect("the configuration always serialises");
        text.push('\n');
        text
    }
}

/// Values given on the command line or in the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub coordinator_url: Option<String>,
    pub key_id: Option<String>,
    pub key_secret: Option<String>,
}

/// The configuration the client runs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub coordinator_url: Option<String>,
    pub key_id: String,
    pub key_secret: String,
    pub device_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{path} is not a valid configuration file: {source}")]
    Parse { path: PathBuf, source: serde_json::Error },
    #[error("cannot read or create the device id in {path}: {source}")]
    DeviceId { path: PathBuf, source: io::Error },
    #[error("no {what}: give {flag} (or set {env}), or set \"{field}\" in {path}")]
    Missing { what: &'static str, flag: &'static str, env: &'static str, field: &'static str, path: PathBuf },
}

/// Reads `config.json` from `state_dir`; `None` if there is none.
pub fn read(state_dir: &Path) -> Result<Option<ConfigFile>, ConfigError> {
    let path = state_dir.join(CONFIG_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|source| ConfigError::Parse { path, source }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read { path, source }),
    }
}

/// Settings from the file and the overrides, an override winning over the
/// file: coordinator URL, key id, key secret, and the device id if the file
/// has one ([`load`] looks further).
fn merge(
    file: Option<ConfigFile>,
    overrides: Overrides,
    file_path: &Path,
) -> Result<(Option<String>, String, String, Option<String>), ConfigError> {
    let file = file.unwrap_or_default();
    // An empty value (such as `SC_KEY_ID=`) counts as not given.
    let pick = |given: Option<String>, stored: Option<String>| {
        given.filter(|value| !value.is_empty()).or(stored.filter(|value| !value.is_empty()))
    };
    let missing = |what, flag, env, field| ConfigError::Missing { what, flag, env, field, path: file_path.to_owned() };
    let key_id = pick(overrides.key_id, file.key_id)
        .ok_or_else(|| missing("enrollment key id", "--key-id", "SC_KEY_ID", "key_id"))?;
    let key_secret = pick(overrides.key_secret, file.key_secret)
        .ok_or_else(|| missing("enrollment key secret", "--key-secret", "SC_KEY_SECRET", "key_secret"))?;
    let coordinator_url = pick(overrides.coordinator_url, file.coordinator_url);
    let device_id = file.device_id.filter(|id| !id.trim().is_empty()).map(|id| id.trim().to_owned());
    Ok((coordinator_url, key_id, key_secret, device_id))
}

/// The settings for a client with state in `state_dir`. The device id comes
/// from `config.json`, else from the `device_id` file of earlier versions,
/// else it is generated (and stored in that file).
pub fn load(state_dir: &Path, overrides: Overrides) -> Result<Settings, ConfigError> {
    let path = state_dir.join(CONFIG_FILE);
    let (coordinator_url, key_id, key_secret, device_id) = merge(read(state_dir)?, overrides, &path)?;
    let device_id = match device_id {
        Some(id) => id,
        None => crate::load_or_create_device_id(state_dir)
            .map_err(|source| ConfigError::DeviceId { path: state_dir.join("device_id"), source })?,
    };
    Ok(Settings { coordinator_url, key_id, key_secret, device_id })
}

/// The device id already given to this device in `state_dir`, if any: from
/// `config.json`, else from the `device_id` file. `install` keeps it, so a
/// reinstall does not make the device a new one.
pub fn existing_device_id(state_dir: &Path) -> Option<String> {
    let from_config = read(state_dir).ok().flatten().and_then(|file| file.device_id);
    let from_file = || std::fs::read_to_string(state_dir.join("device_id")).ok();
    from_config.or_else(from_file).map(|id| id.trim().to_owned()).filter(|id| !id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> ConfigFile {
        ConfigFile {
            device_id: Some("fake-device-from-file".into()),
            key_id: Some("fake-key-from-file".into()),
            key_secret: Some("fake-secret-from-file".into()),
            coordinator_url: Some("wss://file.example".into()),
        }
    }

    fn flags() -> Overrides {
        Overrides {
            coordinator_url: Some("ws://127.0.0.1:18080".into()),
            key_id: Some("fake-key-from-flag".into()),
            key_secret: Some("fake-secret-from-env".into()),
        }
    }

    fn write(dir: &Path, text: &str) {
        std::fs::write(dir.join(CONFIG_FILE), text).unwrap();
    }

    #[test]
    fn the_file_alone_is_enough() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &file().to_json());
        let settings = load(dir.path(), Overrides::default()).unwrap();
        assert_eq!(
            settings,
            Settings {
                coordinator_url: Some("wss://file.example".into()),
                key_id: "fake-key-from-file".into(),
                key_secret: "fake-secret-from-file".into(),
                device_id: "fake-device-from-file".into(),
            }
        );
        // The device id file of earlier versions is not consulted or created.
        assert!(!dir.path().join("device_id").exists());
    }

    #[test]
    fn flags_alone_are_enough_and_keep_the_device_id_file() {
        let dir = tempfile::tempdir().unwrap();
        let settings = load(dir.path(), flags()).unwrap();
        assert_eq!(settings.key_id, "fake-key-from-flag");
        assert_eq!(settings.key_secret, "fake-secret-from-env");
        assert_eq!(settings.coordinator_url.as_deref(), Some("ws://127.0.0.1:18080"));
        // Generated and stored, then the same on the next start.
        assert_eq!(std::fs::read_to_string(dir.path().join("device_id")).unwrap(), settings.device_id);
        assert_eq!(load(dir.path(), flags()).unwrap().device_id, settings.device_id);
    }

    #[test]
    fn flags_win_over_the_file_field_by_field() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &file().to_json());
        let only_url = Overrides { coordinator_url: Some("ws://127.0.0.1:1".into()), ..Overrides::default() };
        let settings = load(dir.path(), only_url).unwrap();
        assert_eq!(settings.coordinator_url.as_deref(), Some("ws://127.0.0.1:1"));
        assert_eq!(settings.key_id, "fake-key-from-file");
        assert_eq!(settings.key_secret, "fake-secret-from-file");

        let settings = load(dir.path(), flags()).unwrap();
        assert_eq!(settings.key_id, "fake-key-from-flag");
        assert_eq!(settings.key_secret, "fake-secret-from-env");
        // The device id only ever comes from the state directory.
        assert_eq!(settings.device_id, "fake-device-from-file");
    }

    #[test]
    fn a_device_id_file_from_an_earlier_version_is_used_when_the_file_has_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device_id"), "fake-old-device\n").unwrap();
        write(dir.path(), r#"{"key_id": "k", "key_secret": "s"}"#);
        assert_eq!(load(dir.path(), Overrides::default()).unwrap().device_id, "fake-old-device");
        assert_eq!(existing_device_id(dir.path()).as_deref(), Some("fake-old-device"));
    }

    #[test]
    fn a_missing_key_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = load(dir.path(), Overrides::default()).unwrap_err();
        let text = error.to_string();
        assert!(text.starts_with("no enrollment key id: give --key-id (or set SC_KEY_ID)"), "{text}");
        assert!(text.contains("config.json"), "{text}");

        write(dir.path(), r#"{"key_id": "fake-key", "key_secret": ""}"#);
        let text = load(dir.path(), Overrides::default()).unwrap_err().to_string();
        assert!(text.starts_with("no enrollment key secret"), "{text}");
        assert!(text.contains("SC_KEY_SECRET"), "{text}");
        // Nothing was generated for a client that cannot run.
        assert!(!dir.path().join("device_id").exists());
    }

    #[test]
    fn a_broken_file_is_an_error_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "{ not json");
        assert!(matches!(load(dir.path(), flags()), Err(ConfigError::Parse { .. })));
    }

    #[test]
    fn the_written_file_reads_back_and_leaves_out_what_is_not_set() {
        let mut config = file();
        config.coordinator_url = None;
        let text = config.to_json();
        assert!(!text.contains("coordinator_url"), "{text}");
        assert!(text.ends_with("}\n"));
        assert_eq!(serde_json::from_str::<ConfigFile>(&text).unwrap(), config);
    }
}
