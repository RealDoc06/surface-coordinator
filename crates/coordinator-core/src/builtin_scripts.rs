//! The scripts every coordinator ships in its library.
//!
//! They only read: nothing here changes the device. Ids are fixed so that a
//! newer coordinator can refresh the name and body of the stored copy.

pub struct BuiltInScript {
    pub id: &'static str,
    pub name: &'static str,
    pub shell: &'static str,
    pub body: &'static str,
}

pub static BUILT_IN_SCRIPTS: [BuiltInScript; 5] = [
    BuiltInScript {
        id: "builtin-system-summary",
        name: "System summary",
        shell: "powershell",
        body: r#"$os = Get-CimInstance Win32_OperatingSystem
$system = Get-CimInstance Win32_ComputerSystem
$uptime = (Get-Date) - $os.LastBootUpTime
[pscustomobject]@{
    Hostname = $env:COMPUTERNAME
    OS       = "$($os.Caption) $($os.Version) (build $($os.BuildNumber))"
    Model    = "$($system.Manufacturer) $($system.Model)"
    Serial   = (Get-CimInstance Win32_BIOS).SerialNumber
    Uptime   = '{0} days {1} hours {2} minutes' -f $uptime.Days, $uptime.Hours, $uptime.Minutes
} | Format-List | Out-String -Width 200
"#,
    },
    BuiltInScript {
        id: "builtin-disk-space",
        name: "Disk space",
        shell: "powershell",
        body: r#"Get-CimInstance Win32_LogicalDisk -Filter 'DriveType = 3' |
    Select-Object DeviceID, VolumeName,
        @{ Name = 'SizeGB'; Expression = { [math]::Round($_.Size / 1GB, 1) } },
        @{ Name = 'FreeGB'; Expression = { [math]::Round($_.FreeSpace / 1GB, 1) } },
        @{ Name = 'Free%';  Expression = { if ($_.Size) { [math]::Round(100 * $_.FreeSpace / $_.Size) } } } |
    Format-Table -AutoSize | Out-String -Width 200
"#,
    },
    BuiltInScript {
        id: "builtin-network-configuration",
        name: "Network configuration",
        shell: "powershell",
        body: r#"Get-NetIPConfiguration | Where-Object { $_.IPv4Address } | ForEach-Object {
    '{0}: {1}' -f $_.InterfaceAlias, ($_.IPv4Address.IPAddress -join ', ')
}
$ssid = netsh wlan show interfaces | Select-String '^\s*SSID\s*:\s*(.+)$'
if ($ssid) { 'Wi-Fi SSID: ' + $ssid.Matches[0].Groups[1].Value.Trim() } else { 'Wi-Fi SSID: (not connected)' }
"#,
    },
    BuiltInScript {
        id: "builtin-installed-updates",
        name: "Installed updates (last 20)",
        shell: "powershell",
        body: r#"Get-HotFix |
    Sort-Object InstalledOn -Descending |
    Select-Object -First 20 HotFixID, Description, InstalledOn, InstalledBy |
    Format-Table -AutoSize | Out-String -Width 200
"#,
    },
    BuiltInScript {
        id: "builtin-user-and-battery",
        name: "Logged-in user and battery",
        shell: "powershell",
        body: r#"$user = (Get-CimInstance Win32_ComputerSystem).UserName
'Logged-in user: ' + $(if ($user) { $user } else { '(nobody)' })
$batteries = @(Get-CimInstance Win32_Battery)
if ($batteries.Count -eq 0) { 'Battery: none' }
foreach ($battery in $batteries) {
    $state = switch ($battery.BatteryStatus) {
        1 { 'discharging' }
        2 { 'on AC power' }
        3 { 'fully charged' }
        6 { 'charging' }
        default { "status $_" }
    }
    'Battery: {0}% ({1})' -f $battery.EstimatedChargeRemaining, $state
}
"#,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_ins_have_unique_ids_and_names_and_pass_the_rules_for_user_scripts() {
        let mut ids: Vec<&str> = BUILT_IN_SCRIPTS.iter().map(|s| s.id).collect();
        let mut names: Vec<String> = BUILT_IN_SCRIPTS.iter().map(|s| s.name.to_lowercase()).collect();
        ids.sort_unstable();
        ids.dedup();
        names.sort_unstable();
        names.dedup();
        assert_eq!(ids.len(), BUILT_IN_SCRIPTS.len());
        assert_eq!(names.len(), BUILT_IN_SCRIPTS.len());
        for script in &BUILT_IN_SCRIPTS {
            assert!(script.id.starts_with("builtin-"));
            assert!(crate::scripts::valid_name(script.name), "{}", script.name);
            assert!(crate::scripts::valid_body(script.body), "{}", script.id);
            assert!(matches!(script.shell, "powershell" | "cmd"));
        }
    }
}
