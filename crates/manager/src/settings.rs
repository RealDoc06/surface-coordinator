//! What the manager remembers between launches.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "manager.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Origin of the remote coordinator last connected to.
    pub remote_url: Option<String>,
}

impl Settings {
    fn path(dir: &Path) -> PathBuf {
        dir.join(FILE_NAME)
    }

    /// Loads the settings; a missing or unreadable file gives the defaults.
    pub fn load(dir: &Path) -> Settings {
        match std::fs::read(Self::path(dir)) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, "ignoring unreadable manager settings");
                Settings::default()
            }),
            Err(_) => Settings::default(),
        }
    }

    pub fn save(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let temporary = dir.join(format!("{FILE_NAME}.tmp"));
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(temporary, Self::path(dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_tolerates_missing_or_broken_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Settings::load(dir.path()), Settings::default());

        let settings = Settings { remote_url: Some("https://coordinator.example.org".into()) };
        settings.save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path()), settings);

        std::fs::write(dir.path().join(FILE_NAME), b"{ broken").unwrap();
        assert_eq!(Settings::load(dir.path()), Settings::default());

        // Fields added by a newer version are ignored; missing ones default.
        std::fs::write(dir.path().join(FILE_NAME), br#"{"something_new": 1}"#).unwrap();
        assert_eq!(Settings::load(dir.path()), Settings::default());
    }
}
