//! The catalogue of device settings an operator can apply with a
//! `setting.apply` job.
//!
//! The catalogue is data: the coordinator validates requests with it, the UI
//! renders forms from it, and the client implements the entries by id.

use serde::Serialize;
use serde_json::Value;

/// Whether a setting applies to the whole device or to the logged-in user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Applies to the device whoever is logged in.
    Machine,
    /// Applies to the user logged in on the device, so needs someone logged in.
    User,
}

/// The kind of value a setting takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ValueType {
    Bool,
    Integer { min: i64, max: i64, unit: &'static str },
    Text { max_len: usize },
}

/// One entry of the catalogue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Setting {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub scope: Scope,
    pub value: ValueType,
}

const PERCENT: ValueType = ValueType::Integer { min: 0, max: 100, unit: "%" };
const MINUTES: ValueType = ValueType::Integer { min: 0, max: 600, unit: "minutes" };

static CATALOGUE: [Setting; 12] = [
    Setting {
        id: "audio.volume",
        label: "Volume",
        description: "The speaker volume of the logged-in user, from 0 (silent) to 100.",
        scope: Scope::User,
        value: PERCENT,
    },
    Setting {
        id: "audio.mute",
        label: "Mute",
        description: "Mutes or unmutes the sound of the logged-in user.",
        scope: Scope::User,
        value: ValueType::Bool,
    },
    Setting {
        id: "display.brightness",
        label: "Display brightness",
        description: "The brightness of the built-in display, from 0 to 100.",
        scope: Scope::Machine,
        value: PERCENT,
    },
    Setting {
        id: "power.display_timeout_ac",
        label: "Turn off display after (plugged in)",
        description: "Minutes of inactivity before the display turns off while plugged in; 0 means never.",
        scope: Scope::Machine,
        value: MINUTES,
    },
    Setting {
        id: "power.display_timeout_battery",
        label: "Turn off display after (on battery)",
        description: "Minutes of inactivity before the display turns off on battery; 0 means never.",
        scope: Scope::Machine,
        value: MINUTES,
    },
    Setting {
        id: "power.sleep_timeout_ac",
        label: "Sleep after (plugged in)",
        description: "Minutes of inactivity before the device goes to sleep while plugged in; 0 means never.",
        scope: Scope::Machine,
        value: MINUTES,
    },
    Setting {
        id: "power.sleep_timeout_battery",
        label: "Sleep after (on battery)",
        description: "Minutes of inactivity before the device goes to sleep on battery; 0 means never.",
        scope: Scope::Machine,
        value: MINUTES,
    },
    Setting {
        id: "desktop.wallpaper",
        label: "Wallpaper",
        description: "The path of an image file on the device to use as the desktop wallpaper.",
        scope: Scope::User,
        value: ValueType::Text { max_len: 260 },
    },
    Setting {
        id: "windows_update.pause_days",
        label: "Pause Windows Update",
        description: "Pauses Windows Update for this many days; 0 resumes updates.",
        scope: Scope::Machine,
        value: ValueType::Integer { min: 0, max: 35, unit: "days" },
    },
    Setting {
        id: "taskbar.locked",
        label: "Lock the taskbar",
        description: "Prevents the logged-in user from changing or rearranging the taskbar.",
        scope: Scope::User,
        value: ValueType::Bool,
    },
    Setting {
        id: "start.layout_locked",
        label: "Lock the Start layout",
        description: "Prevents the logged-in user from changing the pinned items in the Start menu.",
        scope: Scope::User,
        value: ValueType::Bool,
    },
    Setting {
        id: "time.zone",
        label: "Time zone",
        description: "A Windows time zone id such as \"W. Europe Standard Time\".",
        scope: Scope::Machine,
        value: ValueType::Text { max_len: 64 },
    },
];

/// All settings, in the order they are shown to the operator.
pub fn catalogue() -> &'static [Setting] {
    &CATALOGUE
}

/// The catalogue entry with this id.
pub fn find(id: &str) -> Option<&'static Setting> {
    CATALOGUE.iter().find(|setting| setting.id == id)
}

/// Checks that `value` is acceptable for the setting `id`. The error is a
/// human-readable reason.
pub fn validate(id: &str, value: &Value) -> Result<(), String> {
    let setting = find(id).ok_or_else(|| format!("unknown setting {id}"))?;
    match &setting.value {
        ValueType::Bool => {
            if value.is_boolean() {
                Ok(())
            } else {
                Err(format!("{id} must be true or false"))
            }
        }
        ValueType::Integer { min, max, unit } => {
            // `as_i64` rejects floats (even 50.0), strings and out-of-range numbers.
            let number = value
                .as_i64()
                .ok_or_else(|| format!("{id} must be a whole number"))?;
            if (*min..=*max).contains(&number) {
                Ok(())
            } else {
                let unit = if unit.is_empty() { String::new() } else { format!(" {unit}") };
                Err(format!("{id} must be between {min} and {max}{unit}"))
            }
        }
        ValueType::Text { max_len } => {
            let text = value.as_str().ok_or_else(|| format!("{id} must be text"))?;
            if text.is_empty() {
                Err(format!("{id} must not be empty"))
            } else if text.chars().count() > *max_len {
                Err(format!("{id} must be at most {max_len} characters"))
            } else if text.chars().any(char::is_control) {
                Err(format!("{id} must not contain control characters"))
            } else {
                Ok(())
            }
        }
    }
}
