use protocol::{AppLaunchPayload, PowerAction, PowerPayload, SettingApplyPayload};
use serde_json::json;

#[test]
fn app_launch_payload_wire_shape() {
    let payload = AppLaunchPayload {
        path: r"C:\Program Files\App\app.exe".into(),
        arguments: vec!["--fullscreen".into(), "a b".into()],
    };
    let value = serde_json::to_value(&payload).unwrap();
    assert_eq!(
        value,
        json!({"path": r"C:\Program Files\App\app.exe", "arguments": ["--fullscreen", "a b"]})
    );
    assert_eq!(serde_json::from_value::<AppLaunchPayload>(value).unwrap(), payload);
}

#[test]
fn app_launch_arguments_default_to_empty() {
    let payload: AppLaunchPayload = serde_json::from_value(json!({"path": "notepad.exe"})).unwrap();
    assert_eq!(payload, AppLaunchPayload { path: "notepad.exe".into(), arguments: vec![] });
}

#[test]
fn app_launch_requires_a_path() {
    assert!(serde_json::from_value::<AppLaunchPayload>(json!({"arguments": ["x"]})).is_err());
}

#[test]
fn power_payload_wire_shape() {
    for (action, name) in [
        (PowerAction::Restart, "restart"),
        (PowerAction::Shutdown, "shutdown"),
        (PowerAction::Sleep, "sleep"),
    ] {
        let payload = PowerPayload { action };
        let value = serde_json::to_value(payload).unwrap();
        assert_eq!(value, json!({"action": name}));
        assert_eq!(serde_json::from_value::<PowerPayload>(value).unwrap(), payload);
    }
}

#[test]
fn unknown_power_action_is_an_error() {
    assert!(serde_json::from_value::<PowerPayload>(json!({"action": "hibernate"})).is_err());
    assert!(serde_json::from_value::<PowerPayload>(json!({"action": "Restart"})).is_err());
    assert!(serde_json::from_value::<PowerPayload>(json!({})).is_err());
}

#[test]
fn setting_apply_payload_keeps_the_json_value_as_is() {
    for value in [
        json!(true),
        json!(42),
        json!(-1),
        json!(50.5),
        json!("W. Europe Standard Time"),
        json!("50"),
        json!(null),
    ] {
        let wire = json!({"setting": "some.setting", "value": value});
        let payload: SettingApplyPayload = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(payload.setting, "some.setting");
        assert_eq!(payload.value, value);
        assert_eq!(serde_json::to_value(&payload).unwrap(), wire);
    }
}

#[test]
fn setting_apply_requires_setting_and_value() {
    assert!(serde_json::from_value::<SettingApplyPayload>(json!({"setting": "audio.mute"})).is_err());
    assert!(serde_json::from_value::<SettingApplyPayload>(json!({"value": true})).is_err());
}
