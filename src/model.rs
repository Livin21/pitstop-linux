//! Menu/account model + the user-facing setting enums. Ported from the small
//! enums in `AppDelegate.swift` and `Settings.swift`. Claude Desktop's `.desktop`
//! and `.both` sources are dropped (no Claude Desktop on Linux), so every
//! account is either switchable Claude Code or switchable Codex.

use crate::usage_api::UsageReport;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn title(&self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
        }
    }
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Code,
    Codex,
}

/// One row in the menu. Across providers, accounts don't merge: a Claude and a
/// Codex account can share an email yet be different services, so per-account
/// state is keyed by `key()` (provider-namespaced), not bare email.
#[derive(Clone)]
pub struct MenuAccount {
    pub email: String,
    pub source: Source,
    pub plan_label: String,
    pub is_active: bool,
}

impl MenuAccount {
    pub fn is_codex(&self) -> bool {
        self.source == Source::Codex
    }
    pub fn provider(&self) -> Provider {
        if self.is_codex() {
            Provider::Codex
        } else {
            Provider::Claude
        }
    }
    /// Storage key for usage/error/backoff maps — namespaced by provider so a
    /// Claude and a Codex account with the same email don't collide.
    pub fn key(&self) -> String {
        if self.is_codex() {
            format!("codex:{}", self.email)
        } else {
            self.email.clone()
        }
    }
}

// MARK: - Settings enums

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IndicatorStyle {
    IconAndPercent,
    IconOnly,
    PercentOnly,
}

impl IndicatorStyle {
    pub const ALL: [IndicatorStyle; 3] = [
        IndicatorStyle::IconAndPercent,
        IndicatorStyle::IconOnly,
        IndicatorStyle::PercentOnly,
    ];
    pub fn key(&self) -> &'static str {
        match self {
            IndicatorStyle::IconAndPercent => "iconAndPercent",
            IndicatorStyle::IconOnly => "iconOnly",
            IndicatorStyle::PercentOnly => "percentOnly",
        }
    }
    pub fn from_key(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.key() == s)
    }
    pub fn label(&self) -> &'static str {
        match self {
            IndicatorStyle::IconAndPercent => "Icon & Percent",
            IndicatorStyle::IconOnly => "Icon Only",
            IndicatorStyle::PercentOnly => "Percent Only",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IndicatorMetric {
    Binding,
    FiveHour,
    Weekly,
}

impl IndicatorMetric {
    pub const ALL: [IndicatorMetric; 3] = [
        IndicatorMetric::Binding,
        IndicatorMetric::FiveHour,
        IndicatorMetric::Weekly,
    ];
    pub fn key(&self) -> &'static str {
        match self {
            IndicatorMetric::Binding => "binding",
            IndicatorMetric::FiveHour => "fiveHour",
            IndicatorMetric::Weekly => "weekly",
        }
    }
    pub fn from_key(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.key() == s)
    }
    pub fn label(&self) -> &'static str {
        match self {
            IndicatorMetric::Binding => "Highest Limit",
            IndicatorMetric::FiveHour => "5-Hour Limit",
            IndicatorMetric::Weekly => "Weekly Limit",
        }
    }
    /// `None` = the pinned window is absent from the report (shows as "–",
    /// not a misleading 0%).
    pub fn utilization(&self, report: &UsageReport) -> Option<f64> {
        match self {
            IndicatorMetric::Binding => {
                let has = report.five_hour.and_then(|w| w.utilization).is_some()
                    || report.seven_day.and_then(|w| w.utilization).is_some();
                if has {
                    Some(report.max_utilization())
                } else {
                    None
                }
            }
            IndicatorMetric::FiveHour => report.five_hour.and_then(|w| w.utilization),
            IndicatorMetric::Weekly => report.seven_day.and_then(|w| w.utilization),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuBarSource {
    ActiveClaudeCode,
    MostUrgent,
}

impl MenuBarSource {
    pub const ALL: [MenuBarSource; 2] = [MenuBarSource::ActiveClaudeCode, MenuBarSource::MostUrgent];
    pub fn key(&self) -> &'static str {
        match self {
            MenuBarSource::ActiveClaudeCode => "activeClaudeCode",
            MenuBarSource::MostUrgent => "mostUrgent",
        }
    }
    pub fn from_key(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.key() == s)
    }
    pub fn label(&self) -> &'static str {
        match self {
            MenuBarSource::ActiveClaudeCode => "Active Claude Code account",
            MenuBarSource::MostUrgent => "Most-used account (any provider)",
        }
    }
}
