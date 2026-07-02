//! Menu/account model + the user-facing setting enums. Ported from the small
//! enums in `AppDelegate.swift` and `Settings.swift`. Claude Desktop's `.desktop`
//! and `.both` sources are dropped (no Claude Desktop on Linux), so every
//! account is switchable Claude Code, Codex, or Gemini (Antigravity).

use crate::usage_api::UsageReport;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Claude,
    Codex,
    Gemini,
}

impl Provider {
    pub fn title(&self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::Gemini => "Gemini",
        }
    }

    /// Web usage dashboard for this provider.
    pub fn dashboard_url(&self) -> Option<&'static str> {
        Some(match self {
            Provider::Claude => "https://claude.ai/new#settings/usage",
            Provider::Codex => "https://chatgpt.com/codex/cloud/settings/analytics#usage",
            Provider::Gemini => "https://gemini.google.com/usage",
        })
    }

    pub const ALL: [Provider; 3] = [Provider::Claude, Provider::Codex, Provider::Gemini];
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Code,
    Codex,
    Gemini,
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
    pub fn is_gemini(&self) -> bool {
        self.source == Source::Gemini
    }
    pub fn provider(&self) -> Provider {
        match self.source {
            Source::Codex => Provider::Codex,
            Source::Gemini => Provider::Gemini,
            Source::Code => Provider::Claude,
        }
    }
    /// Storage key for usage/error/backoff maps — namespaced by provider so a
    /// Claude and a Codex account with the same email don't collide.
    pub fn key(&self) -> String {
        match self.source {
            Source::Codex => format!("codex:{}", self.email),
            Source::Gemini => format!("gemini:{}", self.email),
            Source::Code => self.email.clone(),
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
                    || report.seven_day.and_then(|w| w.utilization).is_some()
                    || report.scoped.iter().any(|s| s.window.utilization.is_some());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_url_claude() {
        assert_eq!(
            Provider::Claude.dashboard_url(),
            Some("https://claude.ai/new#settings/usage"),
        );
    }

    #[test]
    fn dashboard_url_codex() {
        assert_eq!(
            Provider::Codex.dashboard_url(),
            Some("https://chatgpt.com/codex/cloud/settings/analytics#usage"),
        );
    }

    #[test]
    fn all_current_providers_have_dashboard_url() {
        for p in Provider::ALL {
            assert!(
                p.dashboard_url().is_some(),
                "{} is missing dashboard_url",
                p.title()
            );
        }
    }

    #[test]
    fn binding_metric_has_data_from_scoped_only() {
        use crate::usage_api::{ScopedWindow, UsageReport, UsageWindow};
        let report = UsageReport {
            five_hour: None,
            seven_day: None,
            scoped: vec![ScopedWindow {
                label: "Fable".into(),
                window: UsageWindow { utilization: Some(42.0), resets_at: None },
            }],
            extra_usage_enabled: false,
            extra_usage_utilization: None,
            fetched_at: chrono::Local::now(),
        };
        assert_eq!(IndicatorMetric::Binding.utilization(&report), Some(42.0));
    }

    #[test]
    fn binding_metric_none_without_any_data() {
        use crate::usage_api::UsageReport;
        let report = UsageReport {
            five_hour: None,
            seven_day: None,
            scoped: vec![],
            extra_usage_enabled: false,
            extra_usage_utilization: None,
            fetched_at: chrono::Local::now(),
        };
        assert_eq!(IndicatorMetric::Binding.utilization(&report), None);
    }

    #[test]
    fn gemini_provider_and_account_key() {
        assert_eq!(Provider::Gemini.title(), "Gemini");
        assert_eq!(Provider::ALL.len(), 3);
        assert_eq!(Provider::Gemini.dashboard_url(), Some("https://gemini.google.com/usage"));
        let a = MenuAccount {
            email: "me@x".into(),
            source: Source::Gemini,
            plan_label: "AI Pro".into(),
            is_active: false,
        };
        assert!(a.is_gemini());
        assert!(!a.is_codex());
        assert_eq!(a.key(), "gemini:me@x");
        assert_eq!(a.provider().title(), "Gemini");
    }
}
