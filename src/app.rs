//! The engine — the Linux port of `AppDelegate.swift`'s logic half. It owns all
//! state, drives the 2-minute refresh loop (with rate-limit backoff), runs
//! auto-switch / projection / threshold notifications, and computes a `TrayView`
//! it pushes into the ksni tray via its `Handle`. Tray clicks arrive back as
//! `Action`s over an mpsc channel — a single-task actor, so no locking.

use crate::claude_store::ProfileStore;
use crate::codex;
use crate::codex_store::CodexStore;
use crate::credentials::{self, OAuthCredentials};
use crate::format;
use crate::icon;
use crate::model::{
    IndicatorMetric, IndicatorStyle, MenuAccount, MenuBarSource, Provider, Source,
};
use crate::notify;
use crate::settings::{self, Settings};
use crate::tray::{GroupView, PitStopTray, RowView, TrayView};
use crate::usage_api::{self, ApiError, UsageReport};
use chrono::{DateTime, Local};
use ksni::Handle;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

const REFRESH_INTERVAL: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub enum Action {
    RefreshNow,
    Summary,
    Switch { key: String },
    Save,
    Remove { key: String },
    SetSetting(SettingChange),
    Quit,
}

#[derive(Clone)]
pub enum SettingChange {
    Style(IndicatorStyle),
    Metric(IndicatorMetric),
    Source(MenuBarSource),
    AutoSwitch(bool),
    Threshold(i64),
    Projection(bool),
    LaunchAtLogin(bool),
}

struct Reading {
    pct: Option<i64>,
    stale: bool,
    title: String,
    body: String,
}

pub struct Engine {
    client: reqwest::Client,
    handle: Handle<PitStopTray>,
    store: ProfileStore,
    codex_store: CodexStore,
    settings: Settings,

    active_email: Option<String>,
    codex_live_email: Option<String>,

    usage: HashMap<String, UsageReport>, // key: email (Claude)
    codex_usage: HashMap<String, codex::Usage>, // key: "codex:<email>"
    fetch_error: HashMap<String, String>,
    needs_action: HashSet<String>,
    next_fetch_allowed: HashMap<String, Instant>,
    failure_count: HashMap<String, u32>,
    usage_history: HashMap<String, Vec<(Instant, f64)>>,
    last_auto_switch: HashMap<Provider, Instant>,
    notified_bucket: HashMap<String, u8>,

    last_refresh: Option<DateTime<Local>>,
    last_top_level_error: Option<String>,
    next_periodic: Instant,
}

impl Engine {
    pub fn new(handle: Handle<PitStopTray>) -> Self {
        Engine {
            client: reqwest::Client::new(),
            handle,
            store: ProfileStore::new(),
            codex_store: CodexStore::new(),
            settings: Settings::load(),
            active_email: None,
            codex_live_email: None,
            usage: HashMap::new(),
            codex_usage: HashMap::new(),
            fetch_error: HashMap::new(),
            needs_action: HashSet::new(),
            next_fetch_allowed: HashMap::new(),
            failure_count: HashMap::new(),
            usage_history: HashMap::new(),
            last_auto_switch: HashMap::new(),
            notified_bucket: HashMap::new(),
            last_refresh: None,
            last_top_level_error: None,
            next_periodic: Instant::now() + REFRESH_INTERVAL,
        }
    }

    pub async fn run(mut self, mut rx: UnboundedReceiver<Action>) {
        self.refresh_all().await;
        self.render().await;
        loop {
            let now = Instant::now();
            let mut wake = self.next_periodic;
            if let Some(b) = self.earliest_backoff() {
                if b < wake {
                    wake = b;
                }
            }
            if wake < now {
                wake = now;
            }
            let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(wake));
            tokio::select! {
                _ = sleep => {
                    self.refresh_all().await;
                    self.render().await;
                }
                maybe = rx.recv() => {
                    match maybe {
                        Some(action) => self.handle_action(action).await,
                        None => break,
                    }
                }
            }
        }
    }

    fn earliest_backoff(&self) -> Option<Instant> {
        let now = Instant::now();
        self.next_fetch_allowed
            .values()
            .filter(|t| **t > now)
            .min()
            .copied()
    }

    async fn handle_action(&mut self, action: Action) {
        match action {
            Action::RefreshNow => {
                self.next_fetch_allowed.clear();
                self.refresh_all().await;
                self.render().await;
            }
            Action::Summary => {
                notify::post("PitStop — current usage", &self.summary_text());
                self.refresh_all().await;
                self.render().await;
            }
            Action::Switch { key } => {
                if let Some(email) = key.strip_prefix("codex:") {
                    self.perform_codex_switch(email, false, None).await;
                } else {
                    self.perform_switch(&key.clone(), false, None).await;
                }
                self.refresh_all().await;
                self.render().await;
            }
            Action::Save => {
                match self.store.capture_current() {
                    Ok(Some(email)) => notify::post(
                        &format!("Saved {email}"),
                        "This account can now be switched to from PitStop.",
                    ),
                    Ok(None) => notify::post(
                        "Nothing to save",
                        "No Claude Code login found. Run `claude` and log in first.",
                    ),
                    Err(e) => notify::post("Couldn't save account", &e.to_string()),
                }
                self.refresh_all().await;
                self.render().await;
            }
            Action::Remove { key } => {
                if let Some(email) = key.strip_prefix("codex:") {
                    let _ = self.codex_store.remove(email);
                    self.codex_usage.remove(&key);
                } else {
                    let _ = self.store.remove(&key);
                    self.usage.remove(&key);
                }
                self.fetch_error.remove(&key);
                self.next_fetch_allowed.remove(&key);
                self.failure_count.remove(&key);
                self.render().await;
            }
            Action::SetSetting(change) => {
                self.apply_setting(change);
                self.render().await;
            }
            Action::Quit => std::process::exit(0),
        }
    }

    fn apply_setting(&mut self, change: SettingChange) {
        match change {
            SettingChange::Style(x) => self.settings.indicator_style = x,
            SettingChange::Metric(x) => self.settings.indicator_metric = x,
            SettingChange::Source(x) => self.settings.menu_bar_source = x,
            SettingChange::AutoSwitch(b) => self.settings.auto_switch_enabled = b,
            SettingChange::Threshold(t) => self.settings.auto_switch_threshold = t,
            SettingChange::Projection(b) => self.settings.show_projection = b,
            SettingChange::LaunchAtLogin(b) => {
                if let Err(e) = settings::set_launch_at_login(b) {
                    self.last_top_level_error = Some(e.to_string());
                }
            }
        }
        let _ = self.settings.save();
    }

    // MARK: - Refresh

    async fn refresh_all(&mut self) {
        self.fetch_pass().await;
        self.record_usage_samples();
        self.check_thresholds();
        if self.evaluate_auto_switch().await {
            self.fetch_pass().await;
            self.record_usage_samples();
        }
        self.next_periodic = Instant::now() + REFRESH_INTERVAL;
    }

    async fn fetch_pass(&mut self) {
        self.last_top_level_error = None;
        if let Err(e) = self.store.capture_current() {
            self.last_top_level_error = Some(e.to_string());
        }
        self.store.load();
        self.active_email = credentials::active_email();

        let emails: Vec<String> = self.store.profiles.iter().map(|p| p.email.clone()).collect();
        for email in emails {
            if !self.passed_backoff_gate(&email) {
                continue;
            }
            let is_active = Some(&email) == self.active_email.as_ref();
            match self.fresh_credentials(&email, is_active).await {
                Ok(creds) => match usage_api::fetch_usage(&self.client, &creds.access_token).await {
                    Ok(report) => {
                        self.usage.insert(email.clone(), report);
                        self.clear_fetch_error(&email);
                    }
                    Err(e) => self.record_fetch_error(e, &email),
                },
                Err(e) => self.record_fetch_error(e, &email),
            }
        }

        self.refresh_codex().await;
        self.last_refresh = Some(Local::now());
    }

    fn passed_backoff_gate(&mut self, key: &str) -> bool {
        if let Some(t) = self.next_fetch_allowed.get(key).copied() {
            if Instant::now() < t {
                return false;
            }
            self.next_fetch_allowed.remove(key);
        }
        true
    }

    fn clear_fetch_error(&mut self, key: &str) {
        self.fetch_error.remove(key);
        self.failure_count.insert(key.to_string(), 0);
        self.next_fetch_allowed.remove(key);
        self.needs_action.remove(key);
    }

    fn record_fetch_error(&mut self, e: ApiError, key: &str) {
        let fails = self.failure_count.get(key).copied().unwrap_or(0) + 1;
        self.failure_count.insert(key.to_string(), fails);
        match e {
            ApiError::RateLimited(retry) => {
                let delay = retry.unwrap_or_else(|| (120.0 * 2f64.powi((fails - 1) as i32)).min(900.0));
                self.next_fetch_allowed
                    .insert(key.to_string(), Instant::now() + Duration::from_secs_f64(delay));
                self.fetch_error.insert(key.to_string(), "Rate limited".into());
                self.needs_action.remove(key);
            }
            ApiError::Unauthorized => {
                self.next_fetch_allowed
                    .insert(key.to_string(), Instant::now() + Duration::from_secs(3600));
                self.fetch_error.insert(key.to_string(), e.to_string());
                self.needs_action.insert(key.to_string());
            }
            other => {
                self.fetch_error.insert(key.to_string(), other.to_string());
                self.needs_action.remove(key);
            }
        }
    }

    /// Non-expired credentials for a profile, refreshing via the OAuth refresh
    /// grant (and persisting the result) when the stored token has aged out.
    async fn fresh_credentials(
        &self,
        email: &str,
        is_active: bool,
    ) -> Result<OAuthCredentials, ApiError> {
        let blob = self
            .store
            .blob(email, is_active)
            .map_err(|e| ApiError::Network(e.to_string()))?
            .ok_or_else(|| ApiError::Network("No stored credentials".into()))?;
        let mut creds = credentials::parse_blob(&blob).map_err(|_| ApiError::Malformed)?;
        if !creds.is_expired() {
            return Ok(creds);
        }
        let Some(rt) = creds.refresh_token.clone() else {
            return Err(ApiError::Unauthorized);
        };
        let fresh = usage_api::refresh(&self.client, &rt).await?;
        let patched = credentials::patch_blob(
            &blob,
            &fresh.access_token,
            fresh.refresh_token.as_deref(),
            fresh.expires_at_ms,
        )
        .map_err(|_| ApiError::Malformed)?;
        self.store
            .store_refreshed_blob(&patched, email, is_active)
            .map_err(|e| ApiError::Network(e.to_string()))?;
        creds.access_token = fresh.access_token;
        creds.refresh_token = fresh.refresh_token.or(creds.refresh_token);
        creds.expires_at_ms = fresh.expires_at_ms;
        Ok(creds)
    }

    async fn refresh_codex(&mut self) {
        if !codex::is_present() {
            return;
        }
        if let Err(e) = self.codex_store.capture_current() {
            self.last_top_level_error = Some(e.to_string());
        }
        self.codex_store.load();
        self.codex_live_email = self.codex_store.live_email();

        let emails: Vec<String> = self.codex_store.profiles.iter().map(|p| p.email.clone()).collect();
        for email in emails {
            let key = format!("codex:{email}");
            if !self.passed_backoff_gate(&key) {
                continue;
            }
            let is_live = Some(&email) == self.codex_live_email.as_ref();
            match self.fetch_codex_usage(&email, is_live).await {
                Ok(usage) => {
                    self.codex_usage.insert(key.clone(), usage);
                    self.clear_fetch_error(&key);
                }
                Err(e) => {
                    let unauthorized = matches!(e, ApiError::Unauthorized);
                    self.record_fetch_error(e, &key);
                    if unauthorized && !is_live {
                        self.fetch_error
                            .insert(key.clone(), "Codex session ended — sign in to Codex again".into());
                    }
                }
            }
        }
    }

    async fn fetch_codex_usage(&self, email: &str, is_active: bool) -> Result<codex::Usage, ApiError> {
        let blob = self
            .codex_store
            .blob(email, is_active)
            .map_err(|e| ApiError::Network(e.to_string()))?
            .ok_or(ApiError::Unauthorized)?;
        let creds = codex::credentials(&blob).ok_or(ApiError::Unauthorized)?;
        match codex::fetch_usage(&self.client, &creds).await {
            Ok(u) => Ok(u),
            Err(ApiError::Unauthorized) if !is_active => {
                let rt = creds.refresh_token.clone().ok_or(ApiError::Unauthorized)?;
                let refreshed = codex::refresh(&self.client, &rt)
                    .await
                    .map_err(|_| ApiError::Unauthorized)?;
                let patched = codex::patching(&blob, &refreshed).ok_or(ApiError::Malformed)?;
                let fresh = codex::credentials(&patched).ok_or(ApiError::Malformed)?;
                self.codex_store
                    .store_refreshed_blob(&patched, email)
                    .map_err(|e| ApiError::Network(e.to_string()))?;
                codex::fetch_usage(&self.client, &fresh).await
            }
            Err(e) => Err(e),
        }
    }

    // MARK: - Projection / thresholds / auto-switch

    fn record_usage_samples(&mut self) {
        let now = Instant::now();
        let mut samples: Vec<(String, f64)> = Vec::new();
        for (k, r) in &self.usage {
            if !self.fetch_error.contains_key(k) {
                samples.push((k.clone(), r.max_utilization()));
            }
        }
        for (k, cu) in &self.codex_usage {
            if !self.fetch_error.contains_key(k) {
                samples.push((k.clone(), cu.max_utilization()));
            }
        }
        for (key, util) in samples {
            let entry = self.usage_history.entry(key).or_default();
            if let Some(last) = entry.last() {
                if util < last.1 - 1.0 {
                    entry.clear(); // window reset
                }
            }
            entry.push((now, util));
            entry.retain(|(t, _)| now.duration_since(*t).as_secs_f64() <= 1800.0);
        }
    }

    fn projected_full(&self, key: &str, current: f64, resets_at: Option<DateTime<chrono::Utc>>) -> Option<DateTime<Local>> {
        if !self.settings.show_projection || current >= 100.0 {
            return None;
        }
        let samples = self.usage_history.get(key)?;
        if samples.len() < 3 {
            return None;
        }
        let first = samples.first()?;
        let last = samples.last()?;
        let dt = last.0.duration_since(first.0).as_secs_f64();
        if dt < 300.0 {
            return None;
        }
        let rate = (last.1 - first.1) / dt;
        if rate <= 0.0005 {
            return None;
        }
        let secs_to_full = (100.0 - current) / rate;
        if let Some(reset) = resets_at {
            let secs_to_reset = (reset.with_timezone(&Local) - Local::now()).num_seconds() as f64;
            if secs_to_full >= secs_to_reset {
                return None;
            }
        }
        Some(Local::now() + chrono::Duration::seconds(secs_to_full as i64))
    }

    fn check_thresholds(&mut self) {
        let Some(email) = self.active_email.clone() else {
            return;
        };
        let Some(report) = self.usage.get(&email) else {
            return;
        };
        if self.fetch_error.contains_key(&email) {
            return;
        }
        let pct = report.max_utilization();
        let bucket: u8 = if pct >= 95.0 {
            2
        } else if pct >= 80.0 {
            1
        } else {
            0
        };
        let last = *self.notified_bucket.get(&email).unwrap_or(&0);
        if bucket > last {
            let reset = report
                .binding_window()
                .and_then(|w| w.resets_at)
                .map(|d| format::reset(Some(d)))
                .unwrap_or_default();
            let best = self
                .store
                .profiles
                .iter()
                .filter(|p| p.email != email)
                .filter_map(|p| {
                    if self.fetch_error.contains_key(&p.email) {
                        None
                    } else {
                        self.usage.get(&p.email).map(|r| (p.email.clone(), r.max_utilization()))
                    }
                })
                .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            let hint = match &best {
                Some((e, u)) if *u < 80.0 => {
                    format!("Best pit: {e} ({}% used) — switch from the menu.", u.round() as i64)
                }
                Some(_) => "All saved accounts are running hot — check the menu.".into(),
                None => "Add a second account in PitStop to keep working.".into(),
            };
            notify::post(
                &format!("Claude Code usage at {}%", pct.round() as i64),
                &format!("{email} — {reset}. {hint}"),
            );
        }
        self.notified_bucket.insert(email, bucket);
    }

    async fn evaluate_auto_switch(&mut self) -> bool {
        if !self.settings.auto_switch_enabled {
            return false;
        }
        let threshold = self.settings.auto_switch_threshold as f64;
        let mut switched = false;

        let claude_utils: Vec<(String, Option<f64>)> = self
            .store
            .profiles
            .iter()
            .map(|p| {
                let u = if self.fetch_error.contains_key(&p.email) {
                    None
                } else {
                    self.usage.get(&p.email).map(|r| r.max_utilization())
                };
                (p.email.clone(), u)
            })
            .collect();
        if let Some((target, reason)) = pick_auto_switch(
            self.active_email.as_deref(),
            threshold,
            self.last_auto_switch.get(&Provider::Claude).copied(),
            &claude_utils,
        ) {
            self.last_auto_switch.insert(Provider::Claude, Instant::now());
            self.perform_switch(&target, true, Some(reason)).await;
            switched = true;
        }

        let codex_utils: Vec<(String, Option<f64>)> = self
            .codex_store
            .profiles
            .iter()
            .map(|p| {
                let key = format!("codex:{}", p.email);
                let u = if self.fetch_error.contains_key(&key) {
                    None
                } else {
                    self.codex_usage.get(&key).map(|r| r.max_utilization())
                };
                (p.email.clone(), u)
            })
            .collect();
        if let Some((target, reason)) = pick_auto_switch(
            self.codex_live_email.as_deref(),
            threshold,
            self.last_auto_switch.get(&Provider::Codex).copied(),
            &codex_utils,
        ) {
            self.last_auto_switch.insert(Provider::Codex, Instant::now());
            self.perform_codex_switch(&target, true, Some(reason)).await;
            switched = true;
        }

        switched
    }

    async fn perform_switch(&mut self, email: &str, auto: bool, reason: Option<String>) {
        match self.store.switch_to(email) {
            Ok(()) => {
                self.active_email = Some(email.to_string());
                self.notified_bucket.remove(email);
                let title = if auto {
                    format!("Auto-switched to {email}")
                } else {
                    format!("Switched to {email}")
                };
                let body = reason.unwrap_or_else(|| {
                    "New Claude Code sessions use this account. Running sessions pick it up on their next token refresh.".into()
                });
                notify::post(&title, &body);
            }
            Err(e) => {
                self.last_top_level_error = Some(format!("Couldn't switch account: {e}"));
                notify::post("Couldn't switch account", &e.to_string());
            }
        }
    }

    async fn perform_codex_switch(&mut self, email: &str, auto: bool, reason: Option<String>) {
        match self.codex_store.switch_to(email) {
            Ok(()) => {
                self.codex_live_email = Some(email.to_string());
                let title = if auto {
                    format!("Auto-switched Codex to {email}")
                } else {
                    format!("Switched Codex to {email}")
                };
                let body = reason.unwrap_or_else(|| {
                    "New `codex` sessions use this account. Quit and reopen the Codex app to pick it up.".into()
                });
                notify::post(&title, &body);
            }
            Err(e) => {
                self.last_top_level_error = Some(format!("Couldn't switch Codex account: {e}"));
                notify::post("Couldn't switch Codex account", &e.to_string());
            }
        }
    }

    // MARK: - View

    async fn render(&self) {
        let view = self.build_view();
        self.handle
            .update(move |t: &mut PitStopTray| {
                t.view = view;
            })
            .await;
    }

    fn build_view(&self) -> TrayView {
        let reading = self.menu_bar_reading();
        let icon = icon::render(reading.pct, reading.stale, self.settings.indicator_style);
        let groups = self.grouped_view();

        let mut removable: Vec<(String, String)> = Vec::new();
        for p in &self.store.profiles {
            if Some(&p.email) != self.active_email.as_ref() {
                removable.push((p.email.clone(), p.email.clone()));
            }
        }
        for p in &self.codex_store.profiles {
            if Some(&p.email) != self.codex_live_email.as_ref() {
                removable.push((format!("{} · Codex", p.email), format!("codex:{}", p.email)));
            }
        }

        TrayView {
            icon,
            tooltip_title: reading.title,
            tooltip_body: reading.body,
            groups,
            removable,
            updated_line: self
                .last_refresh
                .map(|d| format!("Updated {} · refreshes every 2 min", format::updated(d))),
            error_line: self.last_top_level_error.clone(),
            settings: self.settings.clone(),
            launch_at_login: settings::launch_at_login_enabled(),
        }
    }

    fn accounts_for_menu(&self) -> Vec<MenuAccount> {
        let mut rows: Vec<MenuAccount> = self
            .store
            .profiles
            .iter()
            .map(|p| MenuAccount {
                email: p.email.clone(),
                source: Source::Code,
                plan_label: p.plan_label(),
                is_active: Some(&p.email) == self.active_email.as_ref(),
            })
            .collect();
        for c in &self.codex_store.profiles {
            rows.push(MenuAccount {
                email: c.email.clone(),
                source: Source::Codex,
                plan_label: c.plan_label.clone(),
                is_active: Some(&c.email) == self.codex_live_email.as_ref(),
            });
        }
        rows
    }

    fn headroom(&self, a: &MenuAccount) -> f64 {
        if a.is_codex() {
            self.codex_usage.get(&a.key()).map(|u| u.max_utilization()).unwrap_or(999.0)
        } else {
            self.usage.get(&a.email).map(|r| r.max_utilization()).unwrap_or(999.0)
        }
    }

    fn grouped_view(&self) -> Vec<GroupView> {
        let all = self.accounts_for_menu();
        let mut groups = Vec::new();
        for provider in Provider::ALL {
            let mut accounts: Vec<MenuAccount> =
                all.iter().filter(|a| a.provider() == provider).cloned().collect();
            accounts.sort_by(|a, b| {
                if a.is_active != b.is_active {
                    return b.is_active.cmp(&a.is_active);
                }
                self.headroom(a)
                    .partial_cmp(&self.headroom(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            if accounts.is_empty() {
                continue;
            }
            let rows = accounts.iter().map(|a| self.build_row(a)).collect();
            groups.push(GroupView {
                title: provider.title().into(),
                rows,
            });
        }
        groups
    }

    fn build_row(&self, account: &MenuAccount) -> RowView {
        let key = account.key();
        let mut detail: Vec<String> = Vec::new();
        let mut binding_util: Option<f64> = None;
        let mut binding_reset: Option<DateTime<chrono::Utc>> = None;
        let mut data_date: Option<DateTime<Local>> = None;

        if account.is_codex() {
            if let Some(cu) = self.codex_usage.get(&key) {
                data_date = Some(cu.fetched_at);
                for w in &cu.windows {
                    let label = if w.label.is_empty() { "·" } else { &w.label };
                    detail.push(window_line(label, Some(w.used_percent), w.resets_at));
                }
                if let Some(top) = cu
                    .windows
                    .iter()
                    .max_by(|a, b| a.used_percent.partial_cmp(&b.used_percent).unwrap_or(std::cmp::Ordering::Equal))
                {
                    binding_util = Some(top.used_percent);
                    binding_reset = top.resets_at;
                }
            }
        } else if let Some(report) = self.usage.get(&key) {
            data_date = Some(report.fetched_at);
            binding_util = Some(report.max_utilization());
            binding_reset = report.binding_window().and_then(|w| w.resets_at);
            let f5 = report.five_hour.and_then(|w| w.utilization);
            detail.push(window_line("5h", f5, report.five_hour.and_then(|w| w.resets_at)));
            let f7 = report.seven_day.and_then(|w| w.utilization);
            detail.push(window_line("7d", f7, report.seven_day.and_then(|w| w.resets_at)));

            let mut extras: Vec<String> = Vec::new();
            if let Some(v) = report.seven_day_opus.and_then(|w| w.utilization) {
                if v > 0.0 {
                    extras.push(format!("Opus wk {}", format::percent(Some(v))));
                }
            }
            if let Some(v) = report.seven_day_sonnet.and_then(|w| w.utilization) {
                if v > 0.0 {
                    extras.push(format!("Sonnet wk {}", format::percent(Some(v))));
                }
            }
            if report.extra_usage_enabled {
                extras.push(format!("Extra {}", format::percent(report.extra_usage_utilization)));
            }
            if !extras.is_empty() {
                detail.push(format!("       {}", extras.join(" · ")));
            }
        }

        if let Some(util) = binding_util {
            if !self.fetch_error.contains_key(&key) {
                if let Some(full) = self.projected_full(&key, util, binding_reset) {
                    detail.push(format!("       ↗ on pace to hit limit ~{}", format::updated(full)));
                }
            }
        }

        if let Some(status) = self.row_status(account, &key, data_date) {
            detail.push(format!("       {status}"));
        }

        RowView {
            marker: if account.is_active { '●' } else { '○' },
            email: account.email.clone(),
            plan_label: account.plan_label.clone(),
            switchable: !account.is_active,
            switch_key: key,
            detail_lines: detail,
        }
    }

    fn row_status(&self, account: &MenuAccount, key: &str, data_date: Option<DateTime<Local>>) -> Option<String> {
        if account.is_codex() && account.is_active && self.needs_action.contains(key) {
            return Some(match data_date {
                Some(d) => format!(
                    "Usage updates when Codex next saves its token · last seen {}",
                    format::updated(d)
                ),
                None => "Usage updates when Codex next saves its token".into(),
            });
        }
        if let Some(err) = self.fetch_error.get(key) {
            let mut text = err.clone();
            if let Some(until) = self.next_fetch_allowed.get(key) {
                if !self.needs_action.contains(key) {
                    let remaining = until.saturating_duration_since(Instant::now()).as_secs_f64();
                    text += &if remaining > 1.0 {
                        format!(" — retrying {}", format::relative(remaining))
                    } else {
                        " — retrying on next refresh".into()
                    };
                }
            }
            return Some(match data_date {
                Some(d) => format!("⚠︎ {text} · showing {} data", format::updated(d)),
                None => format!("⚠︎ {text}"),
            });
        }
        if data_date.is_none() {
            return Some("Loading…".into());
        }
        None
    }

    fn menu_bar_reading(&self) -> Reading {
        match self.settings.menu_bar_source {
            MenuBarSource::ActiveClaudeCode => {
                let Some(email) = self.active_email.clone() else {
                    return Reading {
                        pct: None,
                        stale: false,
                        title: "PitStop".into(),
                        body: "No usage data yet".into(),
                    };
                };
                let Some(report) = self.usage.get(&email) else {
                    let body = self
                        .fetch_error
                        .get(&email)
                        .cloned()
                        .unwrap_or_else(|| "No usage data yet".into());
                    return Reading {
                        pct: None,
                        stale: false,
                        title: "PitStop".into(),
                        body,
                    };
                };
                let util = self.settings.indicator_metric.utilization(report);
                Reading {
                    pct: util.map(|u| u.round() as i64),
                    stale: self.fetch_error.contains_key(&email),
                    title: email.clone(),
                    body: self.status_tip(&email, report),
                }
            }
            MenuBarSource::MostUrgent => {
                let mut best: Option<(String, f64, bool)> = None;
                let mut consider = |name: String, util: f64, stale: bool| {
                    if best.as_ref().is_none_or(|b| util > b.1) {
                        best = Some((name, util, stale));
                    }
                };
                for (key, report) in &self.usage {
                    consider(key.clone(), report.max_utilization(), self.fetch_error.contains_key(key));
                }
                for (key, cu) in &self.codex_usage {
                    let email = key.strip_prefix("codex:").unwrap_or(key);
                    consider(format!("{email} (Codex)"), cu.max_utilization(), self.fetch_error.contains_key(key));
                }
                match best {
                    Some((name, util, stale)) => {
                        let pct = util.round() as i64;
                        Reading {
                            pct: Some(pct),
                            stale,
                            title: "Most used".into(),
                            body: format!("{name} — {pct}%"),
                        }
                    }
                    None => Reading {
                        pct: None,
                        stale: false,
                        title: "PitStop".into(),
                        body: "No usage data yet".into(),
                    },
                }
            }
        }
    }

    fn status_tip(&self, email: &str, report: &UsageReport) -> String {
        let mut tip = format!(
            "5-hour {} · weekly {}",
            format::percent(report.five_hour.and_then(|w| w.utilization)),
            format::percent(report.seven_day.and_then(|w| w.utilization))
        );
        if let Some(err) = self.fetch_error.get(email) {
            tip += &format!("\n⚠ {err} — showing data from {}", format::updated(report.fetched_at));
        }
        tip
    }

    /// One line per account, for the left-click summary notification.
    fn summary_text(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        for acct in self.accounts_for_menu() {
            let marker = if acct.is_active { "●" } else { "○" };
            let key = acct.key();
            let detail = if acct.is_codex() {
                self.codex_usage
                    .get(&key)
                    .map(|cu| {
                        if cu.windows.is_empty() {
                            "—".to_string()
                        } else {
                            cu.windows
                                .iter()
                                .map(|w| format!("{} {}", w.label, format::percent(Some(w.used_percent))))
                                .collect::<Vec<_>>()
                                .join(" · ")
                        }
                    })
                    .or_else(|| self.fetch_error.get(&key).cloned())
                    .unwrap_or_else(|| "…".into())
            } else {
                self.usage
                    .get(&key)
                    .map(|r| {
                        format!(
                            "5h {} · 7d {}",
                            format::percent(r.five_hour.and_then(|w| w.utilization)),
                            format::percent(r.seven_day.and_then(|w| w.utilization))
                        )
                    })
                    .or_else(|| self.fetch_error.get(&key).cloned())
                    .unwrap_or_else(|| "…".into())
            };
            let provider = if acct.is_codex() { " (Codex)" } else { "" };
            lines.push(format!("{marker} {}{provider} — {detail}", acct.email));
        }
        if lines.is_empty() {
            "No accounts yet — log in with `claude`.".to_string()
        } else {
            lines.join("\n")
        }
    }
}

/// Least-squares slope (utilisation % per second) over time-stamped samples.
/// Returns `None` when all samples share the same timestamp (degenerate).
/// Matches the Swift `slopePerSecond` in AppDelegate.swift (v0.3.1 diff).
#[allow(dead_code)] // wired up in a later task
fn slope_per_second(samples: &[(Instant, f64)]) -> Option<f64> {
    if samples.len() < 2 { return None; }
    let t0 = samples[0].0;
    let xs: Vec<f64> = samples
        .iter()
        .map(|(t, _)| t.duration_since(t0).as_secs_f64())
        .collect();
    let ys: Vec<f64> = samples.iter().map(|(_, u)| *u).collect();
    let n = samples.len() as f64;
    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = ys.iter().sum::<f64>() / n;
    let mut num = 0.0_f64;
    let mut den = 0.0_f64;
    for i in 0..samples.len() {
        let dx = xs[i] - mean_x;
        num += dx * (ys[i] - mean_y);
        den += dx * dx;
    }
    if den > 0.0 { Some(num / den) } else { None }
}

/// Human-readable name for a rate-limit window label used in the projection
/// line: "5-hour", "weekly", "monthly", or the raw label for anything else.
#[allow(dead_code)] // wired up in a later task
fn window_name(label: &str) -> String {
    match label {
        "5h"  => "5-hour".into(),
        "7d"  => "weekly".into(),
        "30d" => "monthly".into(),
        other => other.to_string(),
    }
}

fn window_line(label: &str, pct: Option<f64>, resets_at: Option<DateTime<chrono::Utc>>) -> String {
    format!(
        "     {:<3} {} {}  {:>4}   {}",
        label,
        dot(pct),
        bar(pct),
        format::percent(pct),
        format::compact_reset(resets_at)
    )
}

fn bar(pct: Option<f64>) -> String {
    const CELLS: usize = 10;
    match pct {
        Some(p) => {
            let filled = ((p / 100.0 * CELLS as f64).round() as usize).min(CELLS);
            format!("{}{}", "█".repeat(filled), "░".repeat(CELLS - filled))
        }
        None => "░".repeat(CELLS),
    }
}

fn dot(pct: Option<f64>) -> &'static str {
    match pct {
        Some(p) if p >= 90.0 => "🔴",
        Some(p) if p >= 70.0 => "🟠",
        Some(_) => "🟢",
        None => "▫",
    }
}

fn pick_auto_switch(
    live: Option<&str>,
    threshold: f64,
    last: Option<Instant>,
    utils: &[(String, Option<f64>)],
) -> Option<(String, String)> {
    let live = live?;
    let live_util = utils.iter().find(|(e, _)| e == live).and_then(|(_, u)| *u)?;
    if live_util < threshold {
        return None;
    }
    if let Some(last) = last {
        if last.elapsed().as_secs() < 180 {
            return None;
        }
    }
    let target = utils
        .iter()
        .filter(|(e, _)| e != live)
        .filter_map(|(e, u)| u.map(|v| (e.clone(), v)))
        .filter(|(_, v)| *v < threshold)
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;
    let reason = format!(
        "{live} hit {}% — moved to {} ({}% used).",
        live_util.round() as i64,
        target.0,
        target.1.round() as i64
    );
    Some((target.0, reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // ── slope_per_second ───────────────────────────────────────────────────

    #[test]
    fn slope_empty_or_single_sample_is_none() {
        assert_eq!(slope_per_second(&[]), None);
        let t = std::time::Instant::now();
        assert_eq!(slope_per_second(&[(t, 5.0)]), None);
    }

    #[test]
    fn slope_perfect_linear_series() {
        // xs = [0, 200, 400, 600], ys = [10, 30, 50, 70]
        // meanX=300, meanY=40
        // num = (-300)(-30)+(-100)(-10)+(100)(10)+(300)(30) = 9000+1000+1000+9000 = 20000
        // den = 90000+10000+10000+90000 = 200000  =>  slope = 0.1 /s
        let t0 = Instant::now();
        let samples = vec![
            (t0, 10.0_f64),
            (t0 + Duration::from_secs(200), 30.0),
            (t0 + Duration::from_secs(400), 50.0),
            (t0 + Duration::from_secs(600), 70.0),
        ];
        let s = slope_per_second(&samples).expect("should return Some for a rising series");
        assert!((s - 0.1).abs() < 1e-9, "expected 0.1 /s, got {s}");
    }

    #[test]
    fn slope_degenerate_all_same_instant() {
        let t0 = Instant::now();
        let samples = vec![
            (t0, 10.0_f64),
            (t0, 20.0),
            (t0, 30.0),
            (t0, 40.0),
        ];
        assert!(slope_per_second(&samples).is_none(), "all same instant => den==0 => None");
    }

    #[test]
    fn slope_flat_series_is_zero() {
        let t0 = Instant::now();
        let samples = vec![
            (t0, 42.0_f64),
            (t0 + Duration::from_secs(200), 42.0),
            (t0 + Duration::from_secs(400), 42.0),
            (t0 + Duration::from_secs(600), 42.0),
        ];
        // num = 0 (all yi == meanY) ; den > 0  =>  slope == 0.0
        let s = slope_per_second(&samples).expect("den > 0 for different timestamps");
        assert!(s.abs() < 1e-12, "flat series => slope == 0");
    }

    // ── window_name ────────────────────────────────────────────────────────

    #[test]
    fn window_name_known_labels() {
        assert_eq!(window_name("5h"),  "5-hour");
        assert_eq!(window_name("7d"),  "weekly");
        assert_eq!(window_name("30d"), "monthly");
    }

    #[test]
    fn window_name_unknown_label_passthrough() {
        assert_eq!(window_name("14d"), "14d");
        assert_eq!(window_name("1h"),  "1h");
        assert_eq!(window_name(""),    "");
    }
}
