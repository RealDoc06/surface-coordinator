//! The settings catalogue on Windows: one small PowerShell snippet per
//! entry, run through the script runner.
//!
//! The value reaches a snippet only through the `SC_VALUE` environment
//! variable, which the snippet reads as data. It is never put into the
//! script text, so no value can ever become code.

use std::ffi::OsString;
use std::time::Duration;

use protocol::{RunAs, Shell};
use serde_json::Value;

use crate::platform::{PlatformError, ScriptRequest};

/// Generous: compiling the interop code of the audio and wallpaper snippets
/// takes a few seconds on a slow device.
const SNIPPET_TIMEOUT: Duration = Duration::from_secs(120);

/// Who a snippet runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Context {
    /// The client itself: LocalSystem on a device.
    Machine,
    /// The logged-in user.
    User,
    /// The client itself, on the logged-in user's registry hive, whose SID
    /// it gets in `SC_USER_SID`. For per-user policy keys, which the user
    /// can only read.
    MachineForUser,
}

pub(crate) struct Entry {
    pub id: &'static str,
    pub context: Context,
    pub snippet: &'static str,
}

/// Core Audio's `IAudioEndpointVolume` on the default output, as `ScAudio`.
/// Exits with 3 if the device has no audio output.
macro_rules! audio_interop {
    () => {
        r#"$ErrorActionPreference = 'Stop'
Add-Type -Language CSharp -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

// These methods return HRESULT directly. PreserveSig keeps failure codes
// available to the explicit checks and avoids adding a synthetic retval.
[ComImport, Guid("5CDF2C82-841E-4546-9722-0CF74078229A"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IScAudioEndpointVolume
{
    [PreserveSig] int RegisterControlChangeNotify(IntPtr notify);
    [PreserveSig] int UnregisterControlChangeNotify(IntPtr notify);
    [PreserveSig] int GetChannelCount(out uint count);
    [PreserveSig] int SetMasterVolumeLevel(float levelDb, IntPtr context);
    [PreserveSig] int SetMasterVolumeLevelScalar(float level, IntPtr context);
    [PreserveSig] int GetMasterVolumeLevel(out float levelDb);
    [PreserveSig] int GetMasterVolumeLevelScalar(out float level);
    [PreserveSig] int SetChannelVolumeLevel(uint channel, float levelDb, IntPtr context);
    [PreserveSig] int SetChannelVolumeLevelScalar(uint channel, float level, IntPtr context);
    [PreserveSig] int GetChannelVolumeLevel(uint channel, out float levelDb);
    [PreserveSig] int GetChannelVolumeLevelScalar(uint channel, out float level);
    [PreserveSig] int SetMute([MarshalAs(UnmanagedType.Bool)] bool mute, IntPtr context);
    [PreserveSig] int GetMute([MarshalAs(UnmanagedType.Bool)] out bool mute);
}

[ComImport, Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IScMMDevice
{
    [PreserveSig] int Activate(ref Guid iid, int context, IntPtr parameters, [MarshalAs(UnmanagedType.IUnknown)] out object endpoint);
}

[ComImport, Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IScMMDeviceEnumerator
{
    [PreserveSig] int EnumAudioEndpoints(int dataFlow, int stateMask, out IntPtr devices);
    [PreserveSig] int GetDefaultAudioEndpoint(int dataFlow, int role, out IScMMDevice endpoint);
}

[ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")]
class ScMMDeviceEnumerator
{
}

public static class ScAudio
{
    // HRESULT_FROM_WIN32(ERROR_NOT_FOUND): there is no audio output.
    const int NotFound = unchecked((int)0x80070490);
    const int Render = 0;
    const int Multimedia = 1;
    const int AllContexts = 23;

    static IScMMDevice DefaultOutput()
    {
        var enumerator = (IScMMDeviceEnumerator)new ScMMDeviceEnumerator();
        IScMMDevice device;
        int result = enumerator.GetDefaultAudioEndpoint(Render, Multimedia, out device);
        if (result == NotFound) return null;
        Marshal.ThrowExceptionForHR(result);
        return device;
    }

    public static bool HasOutput()
    {
        return DefaultOutput() != null;
    }

    static IScAudioEndpointVolume Endpoint()
    {
        Guid iid = typeof(IScAudioEndpointVolume).GUID;
        object endpoint;
        Marshal.ThrowExceptionForHR(DefaultOutput().Activate(ref iid, AllContexts, IntPtr.Zero, out endpoint));
        return (IScAudioEndpointVolume)endpoint;
    }

    public static float GetLevel()
    {
        float level;
        Marshal.ThrowExceptionForHR(Endpoint().GetMasterVolumeLevelScalar(out level));
        return level;
    }

    public static void SetLevel(float level)
    {
        Marshal.ThrowExceptionForHR(Endpoint().SetMasterVolumeLevelScalar(level, IntPtr.Zero));
    }

    public static bool GetMute()
    {
        bool mute;
        Marshal.ThrowExceptionForHR(Endpoint().GetMute(out mute));
        return mute;
    }

    public static void SetMute(bool mute)
    {
        Marshal.ThrowExceptionForHR(Endpoint().SetMute(mute, IntPtr.Zero));
    }
}
'@
if (-not [ScAudio]::HasOutput()) {
    [Console]::Error.WriteLine('this device has no audio output')
    exit 3
}
"#
    };
}

/// `powercfg /change <setting> <minutes>` on the active power plan.
macro_rules! powercfg_change {
    ($setting:literal) => {
        concat!(
            r#"$ErrorActionPreference = 'Stop'
$minutes = [int]$env:SC_VALUE
& "$env:SystemRoot\System32\powercfg.exe" /change "#,
            $setting,
            r#" $minutes
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
"#
        )
    };
}

/// Sets (true) or removes (false) a DWORD 1 under the logged-in user's
/// `Policies\Explorer` key, given its SID.
macro_rules! user_explorer_policy {
    ($name:literal) => {
        concat!(
            r#"$ErrorActionPreference = 'Stop'
$lock = [bool]::Parse($env:SC_VALUE)
$sid = $env:SC_USER_SID
if ($sid -notmatch '^S-1-[0-9-]+$') {
    [Console]::Error.WriteLine('not a user SID: ' + $sid)
    exit 2
}
# Explorer reads this policy when the user signs in, so the change shows
# from the next sign-in.
$key = 'Registry::HKEY_USERS\' + $sid + '\Software\Microsoft\Windows\CurrentVersion\Policies\Explorer'
$name = '"#,
            $name,
            r#"'
if ($lock) {
    if (-not (Test-Path -LiteralPath $key)) {
        New-Item -Path $key -Force | Out-Null
    }
    New-ItemProperty -LiteralPath $key -Name $name -PropertyType DWord -Value 1 -Force | Out-Null
} else {
    Remove-ItemProperty -LiteralPath $key -Name $name -ErrorAction SilentlyContinue
}
"#
        )
    };
}

pub(crate) static ENTRIES: [Entry; 12] = [
    Entry {
        id: "audio.volume",
        context: Context::User,
        snippet: concat!(
            audio_interop!(),
            r#"$percent = [int]$env:SC_VALUE
[ScAudio]::SetLevel($percent / 100)
"#
        ),
    },
    Entry {
        id: "audio.mute",
        context: Context::User,
        snippet: concat!(
            audio_interop!(),
            r#"[ScAudio]::SetMute([bool]::Parse($env:SC_VALUE))
"#
        ),
    },
    Entry {
        id: "display.brightness",
        context: Context::Machine,
        snippet: r#"$ErrorActionPreference = 'Stop'
$brightness = [byte][int]$env:SC_VALUE
# Only built-in displays offer this; an external monitor has no instance.
$displays = @(try {
    Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightnessMethods -ErrorAction SilentlyContinue
} catch {
})
if ($displays.Count -eq 0) {
    [Console]::Error.WriteLine('this device has no built-in display whose brightness can be set')
    exit 3
}
foreach ($display in $displays) {
    Invoke-CimMethod -InputObject $display -MethodName WmiSetBrightness -Arguments @{ Timeout = [uint32]1; Brightness = $brightness } | Out-Null
}
"#,
    },
    Entry {
        id: "power.display_timeout_ac",
        context: Context::Machine,
        snippet: powercfg_change!("monitor-timeout-ac"),
    },
    Entry {
        id: "power.display_timeout_battery",
        context: Context::Machine,
        snippet: powercfg_change!("monitor-timeout-dc"),
    },
    Entry {
        id: "power.sleep_timeout_ac",
        context: Context::Machine,
        snippet: powercfg_change!("standby-timeout-ac"),
    },
    Entry {
        id: "power.sleep_timeout_battery",
        context: Context::Machine,
        snippet: powercfg_change!("standby-timeout-dc"),
    },
    Entry {
        id: "desktop.wallpaper",
        context: Context::User,
        snippet: r#"$ErrorActionPreference = 'Stop'
$path = $env:SC_VALUE
$file = Get-Item -LiteralPath $path -ErrorAction SilentlyContinue
if ($null -eq $file -or $file.PSProvider.Name -ne 'FileSystem' -or $file.PSIsContainer) {
    [Console]::Error.WriteLine('there is no image file at ' + $path)
    exit 3
}
Add-Type -Language CSharp -TypeDefinition @'
using System.Runtime.InteropServices;

public static class ScWallpaper
{
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool SystemParametersInfo(uint action, uint parameter, string value, uint update);
}
'@
# SPI_SETDESKWALLPAPER, with SPIF_UPDATEINIFILE | SPIF_SENDCHANGE: kept in
# the profile and shown at once.
if (-not [ScWallpaper]::SystemParametersInfo(20, 0, $file.FullName, 3)) {
    $code = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
    [Console]::Error.WriteLine('setting the wallpaper failed with error ' + $code)
    exit 1
}
"#,
    },
    Entry {
        id: "windows_update.pause_days",
        context: Context::Machine,
        snippet: r#"$ErrorActionPreference = 'Stop'
$days = [int]$env:SC_VALUE
# Where the Settings app keeps a pause, in the format it uses.
$key = 'HKLM:\SOFTWARE\Microsoft\WindowsUpdate\UX\Settings'
$starts = 'PauseUpdatesStartTime', 'PauseFeatureUpdatesStartTime', 'PauseQualityUpdatesStartTime'
$ends = 'PauseUpdatesExpiryTime', 'PauseFeatureUpdatesEndTime', 'PauseQualityUpdatesEndTime'
if ($days -eq 0) {
    foreach ($name in $starts + $ends) {
        Remove-ItemProperty -LiteralPath $key -Name $name -ErrorAction SilentlyContinue
    }
    exit 0
}
if (-not (Test-Path -LiteralPath $key)) {
    New-Item -Path $key -Force | Out-Null
}
$format = "yyyy-MM-dd'T'HH:mm:ss'Z'"
$culture = [Globalization.CultureInfo]::InvariantCulture
$now = [DateTime]::UtcNow
$start = $now.ToString($format, $culture)
$end = $now.AddDays($days).ToString($format, $culture)
foreach ($name in $starts) {
    New-ItemProperty -LiteralPath $key -Name $name -PropertyType String -Value $start -Force | Out-Null
}
foreach ($name in $ends) {
    New-ItemProperty -LiteralPath $key -Name $name -PropertyType String -Value $end -Force | Out-Null
}
"#,
    },
    Entry {
        id: "taskbar.locked",
        context: Context::MachineForUser,
        snippet: user_explorer_policy!("LockTaskbar"),
    },
    Entry {
        id: "start.layout_locked",
        context: Context::MachineForUser,
        snippet: user_explorer_policy!("NoChangeStartMenu"),
    },
    Entry {
        id: "time.zone",
        context: Context::Machine,
        // A cmdlet rather than `tzutil /s`: Windows PowerShell's quoting of
        // arguments to programs mangles some values, a parameter does not.
        snippet: r#"$ErrorActionPreference = 'Stop'
Set-TimeZone -Id $env:SC_VALUE
"#,
    },
];

pub(crate) fn entry(id: &str) -> Option<&'static Entry> {
    ENTRIES.iter().find(|entry| entry.id == id)
}

/// The value as the text a snippet finds in `SC_VALUE`.
fn encode_value(value: &Value) -> Result<String, PlatformError> {
    match value {
        Value::Bool(flag) => Ok(flag.to_string()),
        Value::Number(number) if number.is_i64() => Ok(number.to_string()),
        Value::String(text) => Ok(text.clone()),
        other => Err(PlatformError::Failed(format!("cannot pass {other} to a setting"))),
    }
}

/// Variables set for a snippet on top of the usual ones.
pub(crate) type Environment = Vec<(&'static str, OsString)>;

/// The script that applies `entry`, and the environment that carries the
/// value. `user_sid` is asked only by entries that need it.
pub(crate) fn prepare(
    entry: &Entry,
    value: &Value,
    user_sid: impl FnOnce() -> Result<String, PlatformError>,
) -> Result<(ScriptRequest, Environment), PlatformError> {
    let mut env = vec![("SC_VALUE", OsString::from(encode_value(value)?))];
    let run_as = match entry.context {
        Context::Machine => RunAs::System,
        Context::User => RunAs::User,
        Context::MachineForUser => {
            env.push(("SC_USER_SID", OsString::from(user_sid()?)));
            RunAs::System
        }
    };
    let request = ScriptRequest {
        shell: Shell::PowerShell,
        body: entry.snippet.to_owned(),
        timeout: SNIPPET_TIMEOUT,
        run_as,
    };
    Ok((request, env))
}

#[cfg(test)]
mod tests {
    use protocol::settings::{self, Scope};
    use serde_json::json;

    use super::*;

    #[test]
    fn every_catalogue_entry_is_implemented_in_its_scope() {
        for setting in settings::catalogue() {
            let entry = entry(setting.id).unwrap_or_else(|| panic!("{} has no snippet", setting.id));
            let matches = match setting.scope {
                Scope::Machine => entry.context == Context::Machine,
                Scope::User => matches!(entry.context, Context::User | Context::MachineForUser),
            };
            assert!(matches, "{} is {:?} but runs as {:?}", setting.id, setting.scope, entry.context);
        }
        for entry in &ENTRIES {
            assert!(settings::find(entry.id).is_some(), "{} is not in the catalogue", entry.id);
            assert_eq!(ENTRIES.iter().filter(|e| e.id == entry.id).count(), 1, "{} twice", entry.id);
        }
    }

    #[test]
    fn snippets_take_the_value_only_from_the_environment() {
        let placeholder = |text: &str| {
            text.match_indices('{').any(|(start, _)| {
                let rest = &text[start + 1..];
                let digits = rest.chars().take_while(char::is_ascii_digit).count();
                rest[digits..].starts_with('}')
            })
        };
        for entry in &ENTRIES {
            let snippet = entry.snippet;
            assert!(snippet.contains("$env:SC_VALUE"), "{} ignores its value", entry.id);
            // Every mention of the value is a read of the variable.
            assert_eq!(
                snippet.matches("SC_VALUE").count(),
                snippet.matches("$env:SC_VALUE").count(),
                "{}",
                entry.id
            );
            assert!(!placeholder(snippet), "{} has a format placeholder", entry.id);
            assert!(!snippet.contains("%SC_"), "{} expands a variable as cmd would", entry.id);
            assert!(!snippet.contains("Invoke-Expression") && !snippet.contains("iex "), "{}", entry.id);
            if entry.context == Context::MachineForUser {
                assert!(snippet.contains("$env:SC_USER_SID"), "{}", entry.id);
            }
        }
    }

    #[test]
    fn a_hostile_value_stays_out_of_the_script() {
        let hostile = "'; Remove-Item C:\\ -Recurse; $(calc) `\"";
        for id in ["time.zone", "desktop.wallpaper"] {
            let entry = entry(id).unwrap();
            let (request, env) = prepare(entry, &json!(hostile), || unreachable!()).unwrap();
            assert_eq!(request.body, entry.snippet);
            assert_eq!(env, [("SC_VALUE", OsString::from(hostile))]);
        }
    }

    #[test]
    fn values_are_passed_as_plain_text_in_the_entry_context() {
        let (request, env) = prepare(entry("audio.volume").unwrap(), &json!(35), || unreachable!()).unwrap();
        assert_eq!(request.run_as, RunAs::User);
        assert_eq!(request.shell, Shell::PowerShell);
        assert_eq!(env, [("SC_VALUE", OsString::from("35"))]);

        let (request, env) = prepare(entry("display.brightness").unwrap(), &json!(0), || unreachable!()).unwrap();
        assert_eq!(request.run_as, RunAs::System);
        assert_eq!(env, [("SC_VALUE", OsString::from("0"))]);

        let fake_sid = "S-1-5-21-1111111111-2222222222-3333333333-1001";
        let (request, env) =
            prepare(entry("taskbar.locked").unwrap(), &json!(false), || Ok(fake_sid.into())).unwrap();
        assert_eq!(request.run_as, RunAs::System);
        assert_eq!(env, [("SC_VALUE", OsString::from("false")), ("SC_USER_SID", OsString::from(fake_sid))]);

        let nobody = || Err(PlatformError::NoUserSession("nobody".into()));
        assert!(matches!(
            prepare(entry("start.layout_locked").unwrap(), &json!(true), nobody),
            Err(PlatformError::NoUserSession(_))
        ));
        assert!(prepare(entry("audio.volume").unwrap(), &json!(1.5), || unreachable!()).is_err());
    }
}
