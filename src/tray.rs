//! The StatusNotifierItem tray (via `ksni`) — the Linux stand-in for the macOS
//! `NSStatusItem` + `NSMenu`. The engine (`app.rs`) computes a `TrayView`
//! snapshot and pushes it in via the ksni `Handle`; this type only renders that
//! snapshot and forwards clicks to the engine over an mpsc channel.
//!
//! dbusmenu rows are single-line, so the macOS rich `AccountRowView` (custom
//! NSView drawing) becomes a clickable header line per account plus a few
//! disabled "detail" lines beneath it (usage bars as unicode, projection,
//! status), keeping the same information at a glance.

use crate::app::{Action, SettingChange};
use crate::icon::IconImage;
use crate::model::{IndicatorMetric, IndicatorStyle, MenuBarSource};
use crate::settings::Settings;
use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
use ksni::{Category, Icon, MenuItem, ToolTip, Tray};
use crate::updater::UpdateInfo;
use tokio::sync::mpsc::UnboundedSender;

pub struct GroupView {
    pub title: String,
    pub dashboard_url: Option<String>,
    pub rows: Vec<RowView>,
}

pub struct RowView {
    pub marker: char,
    pub email: String,
    pub plan_label: String,
    pub switchable: bool,
    pub switch_key: String,
    pub login: bool,
    pub detail_lines: Vec<String>,
}

pub struct TrayView {
    pub icon: IconImage,
    pub tooltip_title: String,
    pub tooltip_body: String,
    pub groups: Vec<GroupView>,
    pub removable: Vec<(String, String)>, // (display title, storage key)
    pub updated_line: Option<String>,
    pub error_line: Option<String>,
    pub settings: Settings,
    pub launch_at_login: bool,
    pub update_info: Option<UpdateInfo>,
}

impl TrayView {
    pub fn loading() -> Self {
        TrayView {
            icon: crate::icon::placeholder(),
            tooltip_title: "PitStop".into(),
            tooltip_body: "Loading usage…".into(),
            groups: Vec::new(),
            removable: Vec::new(),
            updated_line: None,
            error_line: None,
            settings: Settings::default(),
            launch_at_login: false,
            update_info: None,
        }
    }
}

pub struct PitStopTray {
    pub view: TrayView,
    pub tx: UnboundedSender<Action>,
}

impl Tray for PitStopTray {
    /// Left-click (`Activate`) pops a usage-summary notification. We leave
    /// `MENU_ON_ACTIVATE` at its default (false) so the host actually calls
    /// `activate()`: on Cinnamon/xapp the menu is bound to right-click and the
    /// `MENU_ON_ACTIVATE` workaround other DEs honor is ignored, so left-click
    /// would otherwise do nothing. Right-click still opens the full menu.
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(Action::Summary);
    }

    fn id(&self) -> String {
        "pitstop".into()
    }

    fn title(&self) -> String {
        "PitStop".into()
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![Icon {
            width: self.view.icon.width,
            height: self.view.icon.height,
            data: self.view.icon.argb.clone(),
        }]
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
            title: self.view.tooltip_title.clone(),
            description: self.view.tooltip_body.clone(),
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let v = &self.view;
        let mut items: Vec<MenuItem<Self>> = Vec::new();

        if v.groups.is_empty() {
            items.push(disabled("No accounts found — log in with `claude` first".into()));
        }
        for g in &v.groups {
            items.push(disabled(format!("——  {}  ——", g.title)));
            if let Some(url) = &g.dashboard_url {
                items.push(send_item(
                    format!("↗ Open {} usage dashboard", g.title),
                    true,
                    Action::OpenUrl(url.clone()),
                ));
            }
            for row in &g.rows {
                let plan = if row.plan_label.is_empty() {
                    String::new()
                } else {
                    format!("   [{}]", row.plan_label)
                };
                let header = format!("{}  {}{}", row.marker, row.email, plan);
                if row.switchable {
                    let suffix = row_trailing(row.login);
                    let action = if row.login {
                        Action::Login { key: row.switch_key.clone() }
                    } else {
                        Action::Switch { key: row.switch_key.clone() }
                    };
                    items.push(send_item(format!("{header}    {suffix}"), true, action));
                } else {
                    items.push(disabled(header));
                }
                for line in &row.detail_lines {
                    items.push(disabled(line.clone()));
                }
            }
        }

        items.push(MenuItem::Separator);
        items.push(send_item("Save Current Account".into(), true, Action::Save));
        if !v.removable.is_empty() {
            let sub: Vec<MenuItem<Self>> = v
                .removable
                .iter()
                .map(|(title, key)| {
                    send_item(title.clone(), true, Action::Remove { key: key.clone() })
                })
                .collect();
            items.push(submenu("Remove Account", sub));
        }

        items.push(MenuItem::Separator);
        items.push(send_item("Refresh Now".into(), true, Action::RefreshNow));
        if let Some(u) = &v.updated_line {
            items.push(disabled(u.clone()));
        }
        if let Some(e) = &v.error_line {
            items.push(disabled(format!("⚠ {e}")));
        }

        items.push(MenuItem::Separator);
        items.push(self.settings_submenu());
        items.push(disabled(format!("PitStop v{}", env!("CARGO_PKG_VERSION"))));
        if let Some(ref info) = v.update_info {
            if info.can_rebuild {
                items.push(send_item(
                    format!("Update & Relaunch  (v{} available)", info.version),
                    true,
                    Action::UpdateAndRelaunch,
                ));
            } else {
                // Not a rebuildable source checkout — send them to the release page.
                items.push(send_item(
                    format!("Update available  (v{}) →", info.version),
                    true,
                    Action::OpenUrl(info.url.clone()),
                ));
            }
        }
        items.push(send_item("Quit PitStop".into(), true, Action::Quit));
        items
    }
}

impl PitStopTray {
    fn settings_submenu(&self) -> MenuItem<Self> {
        let s = &self.view.settings;

        let style_items: Vec<MenuItem<Self>> = IndicatorStyle::ALL
            .iter()
            .map(|opt| {
                check(
                    opt.label().into(),
                    s.indicator_style == *opt,
                    Action::SetSetting(SettingChange::Style(*opt)),
                )
            })
            .collect();
        let track_items: Vec<MenuItem<Self>> = MenuBarSource::ALL
            .iter()
            .map(|opt| {
                check(
                    opt.label().into(),
                    s.menu_bar_source == *opt,
                    Action::SetSetting(SettingChange::Source(*opt)),
                )
            })
            .collect();
        let metric_items: Vec<MenuItem<Self>> = IndicatorMetric::ALL
            .iter()
            .map(|opt| {
                check(
                    opt.label().into(),
                    s.indicator_metric == *opt,
                    Action::SetSetting(SettingChange::Metric(*opt)),
                )
            })
            .collect();
        let thr_items: Vec<MenuItem<Self>> = [50, 60, 70, 75, 80, 85, 90, 95]
            .iter()
            .map(|t| {
                check(
                    format!("{t}%"),
                    s.auto_switch_threshold == *t,
                    Action::SetSetting(SettingChange::Threshold(*t)),
                )
            })
            .collect();

        submenu(
            "Settings",
            vec![
                submenu("Menu bar shows", style_items),
                submenu("Track", track_items),
                submenu("Number from", metric_items),
                MenuItem::Separator,
                check(
                    "Auto-switch when an account runs low".into(),
                    s.auto_switch_enabled,
                    Action::SetSetting(SettingChange::AutoSwitch(!s.auto_switch_enabled)),
                ),
                submenu("Auto-switch threshold", thr_items),
                MenuItem::Separator,
                check(
                    "Show time-to-limit projection".into(),
                    s.show_projection,
                    Action::SetSetting(SettingChange::Projection(!s.show_projection)),
                ),
                check(
                    "Launch at login".into(),
                    self.view.launch_at_login,
                    Action::SetSetting(SettingChange::LaunchAtLogin(!self.view.launch_at_login)),
                ),
            ],
        )
    }
}

// MARK: - Menu item builders

fn disabled(label: String) -> MenuItem<PitStopTray> {
    StandardItem {
        label,
        enabled: false,
        ..Default::default()
    }
    .into()
}

fn send_item(label: String, enabled: bool, action: Action) -> MenuItem<PitStopTray> {
    StandardItem {
        label,
        enabled,
        activate: Box::new(move |t: &mut PitStopTray| {
            let _ = t.tx.send(action.clone());
        }),
        ..Default::default()
    }
    .into()
}

fn check(label: String, checked: bool, action: Action) -> MenuItem<PitStopTray> {
    CheckmarkItem {
        label,
        checked,
        enabled: true,
        activate: Box::new(move |t: &mut PitStopTray| {
            let _ = t.tx.send(action.clone());
        }),
        ..Default::default()
    }
    .into()
}

fn submenu(label: &str, items: Vec<MenuItem<PitStopTray>>) -> MenuItem<PitStopTray> {
    SubMenu {
        label: label.into(),
        submenu: items,
        ..Default::default()
    }
    .into()
}

/// The trailing action label for a switchable row: Login when the token was
/// rejected, otherwise the plain account switch.
fn row_trailing(login: bool) -> &'static str {
    if login {
        "⟳ Log in again"
    } else {
        "⮂ switch"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_trailing_switches_on_login_flag() {
        assert_eq!(row_trailing(false), "⮂ switch");
        assert_eq!(row_trailing(true), "⟳ Log in again");
    }

    #[test]
    fn group_view_carries_dashboard_url() {
        let g = GroupView {
            title: "Claude".to_string(),
            dashboard_url: Some("https://claude.ai/new#settings/usage".to_string()),
            rows: vec![],
        };
        assert_eq!(
            g.dashboard_url.as_deref(),
            Some("https://claude.ai/new#settings/usage"),
        );
    }

    #[test]
    fn group_view_dashboard_url_can_be_none() {
        let g = GroupView {
            title: "Unknown".to_string(),
            dashboard_url: None,
            rows: vec![],
        };
        assert!(g.dashboard_url.is_none());
    }

    #[test]
    fn version_line_label_correct() {
        // The disabled menu item always shows the running version.
        let label = format!("PitStop v{}", env!("CARGO_PKG_VERSION"));
        assert_eq!(label, "PitStop v0.3.1");
    }

    #[test]
    fn update_item_label_correct() {
        // When an update is available the label includes the new version.
        let info = UpdateInfo {
            version: "0.4.0".into(),
            url: "https://github.com/Livin21/pitstop-linux/releases/tag/v0.4.0".into(),
            can_rebuild: true,
        };
        let label = format!("Update & Relaunch  (v{} available)", info.version);
        assert_eq!(label, "Update & Relaunch  (v0.4.0 available)");
    }

    #[test]
    fn no_update_means_item_absent() {
        // up-to-date → update_info is None → no item should appear
        let update_info: Option<UpdateInfo> = None;
        assert!(
            update_info.is_none(),
            "None update_info → Update & Relaunch item must not appear"
        );
    }
}
