//! User preferences, persisted to `~/.config/pitstop/settings.json` (the Linux
//! stand-in for macOS `UserDefaults`), plus launch-at-login via an XDG autostart
//! `.desktop` file (the stand-in for `SMAppService`).

use crate::model::{IndicatorMetric, IndicatorStyle, MenuBarSource};
use crate::util::{config_dir, home, write_atomic};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Clone)]
pub struct Settings {
    pub indicator_style: IndicatorStyle,
    pub indicator_metric: IndicatorMetric,
    pub menu_bar_source: MenuBarSource,
    pub auto_switch_enabled: bool,
    pub auto_switch_threshold: i64,
    pub show_projection: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            indicator_style: IndicatorStyle::IconAndPercent,
            indicator_metric: IndicatorMetric::Binding,
            menu_bar_source: MenuBarSource::ActiveClaudeCode,
            auto_switch_enabled: false,
            auto_switch_threshold: 90,
            show_projection: true,
        }
    }
}

impl Settings {
    fn file() -> PathBuf {
        config_dir().join("settings.json")
    }

    pub fn load() -> Self {
        let mut s = Settings::default();
        let Ok(data) = std::fs::read(Self::file()) else {
            return s;
        };
        let Ok(root) = serde_json::from_slice::<Value>(&data) else {
            return s;
        };
        if let Some(v) = root.get("indicatorStyle").and_then(Value::as_str) {
            if let Some(x) = IndicatorStyle::from_key(v) {
                s.indicator_style = x;
            }
        }
        if let Some(v) = root.get("indicatorMetric").and_then(Value::as_str) {
            if let Some(x) = IndicatorMetric::from_key(v) {
                s.indicator_metric = x;
            }
        }
        if let Some(v) = root.get("menuBarSource").and_then(Value::as_str) {
            if let Some(x) = MenuBarSource::from_key(v) {
                s.menu_bar_source = x;
            }
        }
        if let Some(v) = root.get("autoSwitchEnabled").and_then(Value::as_bool) {
            s.auto_switch_enabled = v;
        }
        if let Some(v) = root.get("autoSwitchThreshold").and_then(Value::as_i64) {
            if v != 0 {
                s.auto_switch_threshold = v;
            }
        }
        if let Some(v) = root.get("showProjection").and_then(Value::as_bool) {
            s.show_projection = v;
        }
        s
    }

    pub fn save(&self) -> Result<()> {
        let root = json!({
            "indicatorStyle": self.indicator_style.key(),
            "indicatorMetric": self.indicator_metric.key(),
            "menuBarSource": self.menu_bar_source.key(),
            "autoSwitchEnabled": self.auto_switch_enabled,
            "autoSwitchThreshold": self.auto_switch_threshold,
            "showProjection": self.show_projection,
        });
        write_atomic(&Self::file(), &serde_json::to_vec_pretty(&root)?, None)
    }
}

// MARK: - Launch at login (XDG autostart)

fn autostart_path() -> PathBuf {
    home().join(".config/autostart/pitstop.desktop")
}

pub fn launch_at_login_enabled() -> bool {
    autostart_path().exists()
}

pub fn set_launch_at_login(enabled: bool) -> Result<()> {
    let path = autostart_path();
    if enabled {
        let exe = std::env::current_exe()?;
        let content = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=PitStop\n\
             Comment=Track AI coding usage limits and switch accounts\n\
             Exec={}\n\
             Icon=pitstop\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n",
            exe.display()
        );
        write_atomic(&path, content.as_bytes(), None)?;
    } else if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}
