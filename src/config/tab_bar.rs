use serde::{Deserialize, Serialize};

pub(crate) const DEFAULT_TAB_BAR_COMMAND_INTERVAL_SECONDS: u64 = 5;
pub(crate) const DEFAULT_TAB_BAR_COMMAND_TIMEOUT_SECONDS: u64 = 2;
pub(crate) const MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS: u64 = 31_536_000;
pub(crate) const MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS: u64 = 3_600;
pub(crate) const MAX_TAB_BAR_STATUS_ENTRIES: usize = 16;

/// Which end of the desktop tab row a status area is drawn at. Both sides share
/// one entry type, one resolver, and one set of diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TabBarSide {
    Left,
    Right,
}

impl TabBarSide {
    /// Dotted config key this side is configured under, used in diagnostics.
    pub(crate) fn config_key(self) -> &'static str {
        match self {
            Self::Left => "ui.tab_bar_left",
            Self::Right => "ui.tab_bar_right",
        }
    }
}

fn default_datetime_format() -> String {
    "%H:%M".to_string()
}

fn default_command_interval_seconds() -> u64 {
    DEFAULT_TAB_BAR_COMMAND_INTERVAL_SECONDS
}

fn default_command_timeout_seconds() -> u64 {
    DEFAULT_TAB_BAR_COMMAND_TIMEOUT_SECONDS
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TabBarStatusEntryConfig {
    Zoom,
    Hostname,
    Datetime {
        #[serde(default = "default_datetime_format")]
        format: String,
    },
    Text {
        text: String,
        /// Foreground color as `#rrggbb`. Unset keeps the theme color.
        #[serde(default)]
        fg: Option<String>,
        /// Background color as `#rrggbb`. Unset keeps the tab row background.
        #[serde(default)]
        bg: Option<String>,
        #[serde(default)]
        bold: bool,
    },
    Command {
        command: String,
        #[serde(default = "default_command_interval_seconds")]
        interval_seconds: u64,
        #[serde(default = "default_command_timeout_seconds")]
        timeout_seconds: u64,
        /// Interpret SGR sequences in the last output line instead of stripping them.
        #[serde(default)]
        ansi: bool,
    },
}

pub(crate) fn parse_tab_bar_datetime_format(
    value: &str,
) -> Result<time::format_description::OwnedFormatItem, String> {
    if value.is_empty() {
        return Err("datetime format is empty".into());
    }
    let format = time::format_description::parse_strftime_owned(value)
        .map_err(|err| format!("invalid datetime format: {err}"))?;
    time::PrimitiveDateTime::MIN
        .format(&format)
        .map_err(|err| format!("unsupported datetime format: {err}"))?;
    Ok(format)
}

/// Parse a `#rrggbb` status color. Status entry colors stay deliberately
/// narrower than `[theme.custom]` colors so a typo never silently resolves.
pub(crate) fn parse_tab_bar_status_color(value: &str) -> Option<(u8, u8, u8)> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some((
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
}

pub(crate) fn tab_bar_status_diagnostics(
    side: TabBarSide,
    entries: &[TabBarStatusEntryConfig],
) -> Vec<String> {
    let key = side.config_key();
    let mut diagnostics = Vec::new();
    if entries.len() > MAX_TAB_BAR_STATUS_ENTRIES {
        diagnostics.push(format!(
            "{key} may contain at most {MAX_TAB_BAR_STATUS_ENTRIES} entries; ignoring extras"
        ));
    }

    for (index, entry) in entries.iter().enumerate().take(MAX_TAB_BAR_STATUS_ENTRIES) {
        match entry {
            TabBarStatusEntryConfig::Datetime { format } => {
                if format.is_empty() {
                    diagnostics.push(format!(
                        "{key}[{index}] datetime format is empty; hiding entry"
                    ));
                } else if let Err(err) = parse_tab_bar_datetime_format(format) {
                    diagnostics.push(format!("{key}[{index}] has {err}; hiding entry"));
                }
            }
            TabBarStatusEntryConfig::Text { fg, bg, .. } => {
                for (field, value) in [("fg", fg), ("bg", bg)] {
                    if let Some(value) = value {
                        if parse_tab_bar_status_color(value).is_none() {
                            diagnostics.push(format!(
                                "{key}[{index}] {field} = {value:?} is not a #rrggbb color; ignoring it"
                            ));
                        }
                    }
                }
            }
            TabBarStatusEntryConfig::Command {
                command,
                interval_seconds,
                timeout_seconds,
                ansi: _,
            } => {
                if command.trim().is_empty() {
                    diagnostics.push(format!("{key}[{index}] command is empty; hiding entry"));
                }
                if *interval_seconds == 0 {
                    diagnostics.push(format!(
                        "{key}[{index}] interval_seconds must be at least 1; hiding entry"
                    ));
                }
                if *interval_seconds > MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS {
                    diagnostics.push(format!(
                        "{key}[{index}] interval_seconds may be at most {MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS}; hiding entry"
                    ));
                }
                if *timeout_seconds == 0 {
                    diagnostics.push(format!(
                        "{key}[{index}] timeout_seconds must be at least 1; hiding entry"
                    ));
                }
                if *timeout_seconds > MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS {
                    diagnostics.push(format!(
                        "{key}[{index}] timeout_seconds may be at most {MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS}; hiding entry"
                    ));
                }
            }
            TabBarStatusEntryConfig::Zoom | TabBarStatusEntryConfig::Hostname => {}
        }
    }

    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_bar_entries_parse_with_command_defaults() {
        #[derive(Deserialize)]
        struct Wrapper {
            entries: Vec<TabBarStatusEntryConfig>,
        }

        let parsed: Wrapper = toml::from_str(
            r#"
entries = [
  { type = "zoom" },
  { type = "hostname" },
  { type = "datetime", format = "%H:%M" },
  { type = "text", text = "prod" },
  { type = "command", command = "status.sh" },
]
"#,
        )
        .expect("parse tab bar entries");

        assert_eq!(parsed.entries.len(), 5);
        assert!(matches!(
            &parsed.entries[3],
            TabBarStatusEntryConfig::Text {
                fg: None,
                bg: None,
                bold: false,
                ..
            }
        ));
        assert!(matches!(
            &parsed.entries[4],
            TabBarStatusEntryConfig::Command {
                interval_seconds: DEFAULT_TAB_BAR_COMMAND_INTERVAL_SECONDS,
                timeout_seconds: DEFAULT_TAB_BAR_COMMAND_TIMEOUT_SECONDS,
                ansi: false,
                ..
            }
        ));
    }

    #[test]
    fn styled_entry_fields_parse() {
        #[derive(Deserialize)]
        struct Wrapper {
            entries: Vec<TabBarStatusEntryConfig>,
        }

        let parsed: Wrapper = toml::from_str(
            r##"
entries = [
  { type = "text", text = "prod", fg = "#ff0000", bg = "#101010", bold = true },
  { type = "command", command = "status.sh", ansi = true },
]
"##,
        )
        .expect("parse styled tab bar entries");

        assert_eq!(
            parsed.entries[0],
            TabBarStatusEntryConfig::Text {
                text: "prod".into(),
                fg: Some("#ff0000".into()),
                bg: Some("#101010".into()),
                bold: true,
            }
        );
        assert!(matches!(
            &parsed.entries[1],
            TabBarStatusEntryConfig::Command { ansi: true, .. }
        ));
    }

    #[test]
    fn status_colors_accept_only_six_digit_hex() {
        assert_eq!(parse_tab_bar_status_color("#1a2B3c"), Some((26, 43, 60)));
        for value in ["1a2b3c", "#1a2b3", "#1a2b3cc", "#gg0000", "red", ""] {
            assert_eq!(parse_tab_bar_status_color(value), None, "value: {value}");
        }
    }

    #[test]
    fn diagnostics_reject_invalid_datetime_and_command_schedules() {
        let entries = vec![
            TabBarStatusEntryConfig::Datetime {
                format: "%Q".into(),
            },
            TabBarStatusEntryConfig::Datetime {
                format: "%z".into(),
            },
            TabBarStatusEntryConfig::Command {
                command: String::new(),
                interval_seconds: 0,
                timeout_seconds: 0,
                ansi: false,
            },
            TabBarStatusEntryConfig::Command {
                command: "status.sh".into(),
                interval_seconds: MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS + 1,
                timeout_seconds: MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS + 1,
                ansi: true,
            },
        ];

        let diagnostics = tab_bar_status_diagnostics(TabBarSide::Right, &entries).join("\n");
        assert!(diagnostics.contains("invalid datetime format"));
        assert!(diagnostics.contains("unsupported datetime format"));
        assert!(diagnostics.contains("command is empty"));
        assert!(diagnostics.contains("interval_seconds must be at least 1"));
        assert!(diagnostics.contains("interval_seconds may be at most"));
        assert!(diagnostics.contains("timeout_seconds must be at least 1"));
        assert!(diagnostics.contains("timeout_seconds may be at most"));
        assert!(diagnostics.contains("ui.tab_bar_right[0]"));
        assert!(parse_tab_bar_datetime_format("").is_err());
    }

    #[test]
    fn diagnostics_name_the_configured_side_and_bad_entry_colors() {
        let diagnostics = tab_bar_status_diagnostics(
            TabBarSide::Left,
            &[TabBarStatusEntryConfig::Text {
                text: "prod".into(),
                fg: Some("blue".into()),
                bg: Some("#0000".into()),
                bold: false,
            }],
        );

        assert_eq!(
            diagnostics,
            vec![
                "ui.tab_bar_left[0] fg = \"blue\" is not a #rrggbb color; ignoring it".to_string(),
                "ui.tab_bar_left[0] bg = \"#0000\" is not a #rrggbb color; ignoring it".to_string(),
            ]
        );
    }
}
