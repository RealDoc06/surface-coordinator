use protocol::settings::{catalogue, find, validate, Scope, ValueType};
use serde_json::{json, Value};
use std::collections::HashSet;

#[test]
fn ids_are_unique() {
    let mut seen = HashSet::new();
    for setting in catalogue() {
        assert!(seen.insert(setting.id), "duplicate id {}", setting.id);
    }
    assert_eq!(catalogue().len(), 12);
}

#[test]
fn every_entry_has_a_label_and_a_description() {
    for setting in catalogue() {
        assert!(!setting.id.is_empty());
        assert!(!setting.label.trim().is_empty(), "{} has no label", setting.id);
        assert!(!setting.description.trim().is_empty(), "{} has no description", setting.id);
    }
}

#[test]
fn catalogue_entries_have_the_documented_types_and_scopes() {
    let expect = |id: &str, scope: Scope, value: ValueType| {
        let setting = find(id).unwrap_or_else(|| panic!("{id} missing"));
        assert_eq!(setting.scope, scope, "scope of {id}");
        assert_eq!(setting.value, value, "value of {id}");
    };
    let percent = ValueType::Integer { min: 0, max: 100, unit: "%" };
    let minutes = ValueType::Integer { min: 0, max: 600, unit: "minutes" };
    expect("audio.volume", Scope::User, percent.clone());
    expect("audio.mute", Scope::User, ValueType::Bool);
    expect("display.brightness", Scope::Machine, percent);
    expect("power.display_timeout_ac", Scope::Machine, minutes.clone());
    expect("power.display_timeout_battery", Scope::Machine, minutes.clone());
    expect("power.sleep_timeout_ac", Scope::Machine, minutes.clone());
    expect("power.sleep_timeout_battery", Scope::Machine, minutes);
    expect("desktop.wallpaper", Scope::User, ValueType::Text { max_len: 260 });
    expect(
        "windows_update.pause_days",
        Scope::Machine,
        ValueType::Integer { min: 0, max: 35, unit: "days" },
    );
    expect("taskbar.locked", Scope::User, ValueType::Bool);
    expect("start.layout_locked", Scope::User, ValueType::Bool);
    expect("time.zone", Scope::Machine, ValueType::Text { max_len: 64 });
}

/// The entry as JSON without its description, which is prose that may be
/// reworded; its presence is tested separately.
fn entry_json(id: &str) -> Value {
    let mut value = serde_json::to_value(find(id).unwrap()).unwrap();
    let description = value.as_object_mut().unwrap().remove("description").unwrap();
    assert!(description.as_str().is_some_and(|d| !d.is_empty()));
    value
}

#[test]
fn bool_entry_wire_shape() {
    assert_eq!(
        entry_json("audio.mute"),
        json!({"id": "audio.mute", "label": "Mute", "scope": "user", "value": {"type": "bool"}})
    );
}

#[test]
fn integer_entry_wire_shape() {
    assert_eq!(
        entry_json("display.brightness"),
        json!({
            "id": "display.brightness",
            "label": "Display brightness",
            "scope": "machine",
            "value": {"type": "integer", "min": 0, "max": 100, "unit": "%"},
        })
    );
}

#[test]
fn text_entry_wire_shape() {
    assert_eq!(
        entry_json("time.zone"),
        json!({"id": "time.zone", "label": "Time zone", "scope": "machine", "value": {"type": "text", "max_len": 64}})
    );
}

#[test]
fn catalogue_serialises_as_a_list_of_settings() {
    let value = serde_json::to_value(catalogue()).unwrap();
    let list = value.as_array().unwrap();
    assert_eq!(list.len(), catalogue().len());
    for entry in list {
        let mut keys: Vec<&str> = entry.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["description", "id", "label", "scope", "value"]);
    }
}

#[test]
fn find_returns_the_entry_or_none() {
    assert_eq!(find("audio.volume").unwrap().label, "Volume");
    assert_eq!(find("time.zone").unwrap().id, "time.zone");
    assert!(find("audio.nothing").is_none());
    assert!(find("").is_none());
    assert!(find("Audio.Volume").is_none());
}

#[test]
fn integer_values_are_validated() {
    for value in [json!(0), json!(1), json!(50), json!(99), json!(100)] {
        assert!(validate("audio.volume", &value).is_ok(), "{value} should be valid");
    }
    let invalid = [
        json!(101),
        json!(-1),
        json!(50.5),
        json!(50.0),
        json!("50"),
        json!(true),
        json!(null),
        json!([50]),
        json!({"value": 50}),
        json!(u64::MAX),
        json!(i64::MIN),
    ];
    for value in invalid {
        let reason = validate("audio.volume", &value).expect_err(&format!("{value} should be invalid"));
        assert!(!reason.is_empty());
    }
}

#[test]
fn integer_bounds_come_from_the_entry() {
    assert!(validate("power.sleep_timeout_ac", &json!(600)).is_ok());
    assert!(validate("power.sleep_timeout_ac", &json!(601)).is_err());
    assert!(validate("windows_update.pause_days", &json!(0)).is_ok());
    assert!(validate("windows_update.pause_days", &json!(35)).is_ok());
    assert!(validate("windows_update.pause_days", &json!(36)).is_err());
}

#[test]
fn bool_values_are_validated() {
    assert!(validate("audio.mute", &json!(true)).is_ok());
    assert!(validate("audio.mute", &json!(false)).is_ok());
    for value in [json!(1), json!(0), json!("true"), json!("false"), json!(null), json!([true])] {
        assert!(validate("audio.mute", &value).is_err(), "{value} should be invalid");
    }
}

#[test]
fn text_values_are_validated() {
    assert!(validate("time.zone", &json!("W. Europe Standard Time")).is_ok());
    assert!(validate("time.zone", &json!("x")).is_ok());
    assert!(validate("time.zone", &json!("a".repeat(64))).is_ok());
    assert!(validate("desktop.wallpaper", &json!("a".repeat(260))).is_ok());
    // Counted in characters, not bytes.
    assert!(validate("time.zone", &json!("ä".repeat(64))).is_ok());

    let invalid = [
        json!(""),
        json!("a".repeat(65)),
        json!("ä".repeat(65)),
        json!("two\nlines"),
        json!("carriage\rreturn"),
        json!("nul\0byte"),
        json!("tab\tbed"),
        json!("escape\u{1b}[0m"),
        json!(12),
        json!(true),
        json!(null),
    ];
    for value in invalid {
        assert!(validate("time.zone", &value).is_err(), "{value} should be invalid");
    }
    assert!(validate("desktop.wallpaper", &json!("a".repeat(261))).is_err());
}

#[test]
fn unknown_ids_are_rejected_with_a_reason() {
    let reason = validate("audio.nothing", &json!(1)).unwrap_err();
    assert!(reason.contains("audio.nothing"), "reason was {reason:?}");
}
