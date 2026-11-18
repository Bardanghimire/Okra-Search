use super::{Action, Item, Kind, Provider};
use crate::matcher::{score, Query};

struct SettingEntry {
    name: &'static str,
    detail: &'static str,
    target: SettingTarget,
}

#[derive(Clone, Copy)]
enum SettingTarget {
    Uri(&'static str),
    Run(&'static str, &'static [&'static str]),
}

/// Built-in Windows Settings and administrative shortcuts. No network/plugin discovery.
pub struct SettingsProvider;

const SETTINGS: &[SettingEntry] = &[
    SettingEntry {
        name: "Windows Settings",
        detail: "All settings home",
        target: SettingTarget::Uri("ms-settings:"),
    },
    SettingEntry {
        name: "Display",
        detail: "Screen resolution scaling monitors",
        target: SettingTarget::Uri("ms-settings:display"),
    },
    SettingEntry {
        name: "Night light",
        detail: "Display blue light schedule",
        target: SettingTarget::Uri("ms-settings:nightlight"),
    },
    SettingEntry {
        name: "Sound",
        detail: "Audio output input volume",
        target: SettingTarget::Uri("ms-settings:sound"),
    },
    SettingEntry {
        name: "Volume mixer",
        detail: "Per-app audio volume",
        target: SettingTarget::Uri("ms-settings:apps-volume"),
    },
    SettingEntry {
        name: "Bluetooth",
        detail: "Bluetooth devices pairing",
        target: SettingTarget::Uri("ms-settings:bluetooth"),
    },
    SettingEntry {
        name: "Wi-Fi and network",
        detail: "Network internet status",
        target: SettingTarget::Uri("ms-settings:network-status"),
    },
    SettingEntry {
        name: "VPN",
        detail: "Virtual private network",
        target: SettingTarget::Uri("ms-settings:network-vpn"),
    },
    SettingEntry {
        name: "Personalization",
        detail: "Background lock screen themes",
        target: SettingTarget::Uri("ms-settings:personalization"),
    },
    SettingEntry {
        name: "Colors",
        detail: "Accent color dark light mode",
        target: SettingTarget::Uri("ms-settings:colors"),
    },
    SettingEntry {
        name: "Lock screen",
        detail: "Lock screen background",
        target: SettingTarget::Uri("ms-settings:lockscreen"),
    },
    SettingEntry {
        name: "Installed apps",
        detail: "Uninstall modify applications",
        target: SettingTarget::Uri("ms-settings:appsfeatures"),
    },
    SettingEntry {
        name: "Default apps",
        detail: "File and protocol associations",
        target: SettingTarget::Uri("ms-settings:defaultapps"),
    },
    SettingEntry {
        name: "Startup apps",
        detail: "Applications at sign in",
        target: SettingTarget::Uri("ms-settings:startupapps"),
    },
    SettingEntry {
        name: "Windows Update",
        detail: "Update history pause updates",
        target: SettingTarget::Uri("ms-settings:windowsupdate"),
    },
    SettingEntry {
        name: "Recovery",
        detail: "Reset advanced startup",
        target: SettingTarget::Uri("ms-settings:recovery"),
    },
    SettingEntry {
        name: "Windows Security",
        detail: "Device security protection",
        target: SettingTarget::Uri("windowsdefender:"),
    },
    SettingEntry {
        name: "Privacy",
        detail: "Permissions camera microphone location",
        target: SettingTarget::Uri("ms-settings:privacy"),
    },
    SettingEntry {
        name: "About this PC",
        detail: "Device name processor RAM Windows version",
        target: SettingTarget::Uri("ms-settings:about"),
    },
    SettingEntry {
        name: "Storage",
        detail: "Disk space temporary files",
        target: SettingTarget::Uri("ms-settings:storagesense"),
    },
    SettingEntry {
        name: "Power and sleep",
        detail: "Battery screen sleep",
        target: SettingTarget::Uri("ms-settings:powersleep"),
    },
    SettingEntry {
        name: "Accessibility",
        detail: "Magnifier captions narrator",
        target: SettingTarget::Uri("ms-settings:easeofaccess"),
    },
    SettingEntry {
        name: "Date and time",
        detail: "Clock timezone calendar",
        target: SettingTarget::Uri("ms-settings:dateandtime"),
    },
    SettingEntry {
        name: "Language and region",
        detail: "Display language keyboard region",
        target: SettingTarget::Uri("ms-settings:regionlanguage"),
    },
    SettingEntry {
        name: "Gaming",
        detail: "Game bar captures",
        target: SettingTarget::Uri("ms-settings:gaming"),
    },
    SettingEntry {
        name: "Printers and scanners",
        detail: "Devices print queue",
        target: SettingTarget::Uri("ms-settings:printers"),
    },
    SettingEntry {
        name: "Mouse",
        detail: "Pointer speed buttons",
        target: SettingTarget::Uri("ms-settings:mousetouchpad"),
    },
    SettingEntry {
        name: "Keyboard",
        detail: "Typing input settings",
        target: SettingTarget::Uri("ms-settings:keyboard"),
    },
    SettingEntry {
        name: "Task Manager",
        detail: "Processes performance startup",
        target: SettingTarget::Run("taskmgr.exe", &[]),
    },
    SettingEntry {
        name: "Device Manager",
        detail: "Hardware drivers devices",
        target: SettingTarget::Run("mmc.exe", &["devmgmt.msc"]),
    },
    SettingEntry {
        name: "Disk Management",
        detail: "Partitions volumes disks",
        target: SettingTarget::Run("mmc.exe", &["diskmgmt.msc"]),
    },
    SettingEntry {
        name: "Services",
        detail: "Windows background services",
        target: SettingTarget::Run("mmc.exe", &["services.msc"]),
    },
    SettingEntry {
        name: "Event Viewer",
        detail: "System application logs",
        target: SettingTarget::Run("mmc.exe", &["eventvwr.msc"]),
    },
    SettingEntry {
        name: "Task Scheduler",
        detail: "Scheduled tasks triggers",
        target: SettingTarget::Run("mmc.exe", &["taskschd.msc"]),
    },
    SettingEntry {
        name: "Computer Management",
        detail: "System tools disks shared folders",
        target: SettingTarget::Run("mmc.exe", &["compmgmt.msc"]),
    },
    SettingEntry {
        name: "Registry Editor",
        detail: "Windows registry advanced settings",
        target: SettingTarget::Run("regedit.exe", &[]),
    },
    SettingEntry {
        name: "System Configuration",
        detail: "Boot services startup diagnostic",
        target: SettingTarget::Run("msconfig.exe", &[]),
    },
    SettingEntry {
        name: "System Information",
        detail: "Hardware and system summary",
        target: SettingTarget::Run("msinfo32.exe", &[]),
    },
    SettingEntry {
        name: "Control Panel",
        detail: "Classic Windows settings",
        target: SettingTarget::Run("control.exe", &[]),
    },
    SettingEntry {
        name: "Resource Monitor",
        detail: "CPU memory disk network",
        target: SettingTarget::Run("resmon.exe", &[]),
    },
    SettingEntry {
        name: "Character Map",
        detail: "Special characters Unicode",
        target: SettingTarget::Run("charmap.exe", &[]),
    },
];

impl Provider for SettingsProvider {
    fn name(&self) -> &'static str {
        "Windows Settings"
    }
    fn prefix(&self) -> Option<&'static str> {
        Some("s")
    }
    fn query(&self, q: &str, limit: usize) -> Vec<Item> {
        let query = Query::new(q);
        let mut items: Vec<Item> = SETTINGS
            .iter()
            .filter_map(|entry| {
                let rank = if query.is_empty() {
                    0
                } else {
                    score(&query, entry.name)
                        .map(|s| s + 36)
                        .or_else(|| score(&query, entry.detail).map(|s| s + 8))?
                };
                let action = match entry.target {
                    SettingTarget::Uri(uri) => Action::OpenUri(uri.to_string()),
                    SettingTarget::Run(program, args) => Action::Run {
                        program: program.to_string(),
                        args: args.iter().map(|s| (*s).to_string()).collect(),
                    },
                };
                Some(Item {
                    id: format!("setting:{}", entry.name),
                    title: entry.name.to_string(),
                    subtitle: entry.detail.to_string(),
                    kind: Kind::Setting,
                    action,
                    score: rank,
                })
            })
            .collect();
        items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
        items.truncate(limit);
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_device_manager_by_keyword() {
        let results = SettingsProvider.query("drivers", 10);
        assert!(results.iter().any(|item| item.title == "Device Manager"));
    }
}
