//! The engine — the Linux port of `AppDelegate.swift`'s logic half. It owns all
//! state, drives the 2-minute refresh loop (with rate-limit backoff), runs
//! auto-switch / projection / threshold notifications, and computes a `TrayView`
//! it pushes into the ksni tray via its `Handle`. Tray clicks arrive back as
//! `Action`s over an mpsc channel — a single-task actor, so no locking.

use crate::claude_store::ProfileStore;
use crate::codex;
use crate::codex_store::CodexStore;
use crate::credentials::{self, OAuthCredentials};
use crate::gemini;
use crate::gemini_store::GeminiStore;
use crate::format;
use crate::icon;
use crate::model::{
    IndicatorMetric, IndicatorStyle, MenuAccount, MenuBarSource, Provider, Source,
};
use crate::notify;
use crate::oauth::{self, LoginAdapter};
use crate::settings::{self, Settings};
use crate::tray::{GroupView, PitStopTray, RowView, TrayView};
use crate::updater::{self, UpdateInfo};
use crate::usage_api::{self, ApiError, UsageReport};
use chrono::{DateTime, Local};
use ksni::Handle;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

const REFRESH_INTERVAL: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub enum Action {
    RefreshNow,
    Summary,
    Switch { key: String },
    Save,
    Remove { key: String },
    OpenUrl(String),
    SetSetting(SettingChange),
    Login { key: String },
    LoginFinished { key: String, result: Result<(), String> },
    UpdateAndRelaunch,
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
    gemini_store: GeminiStore,
    settings: Settings,

    active_email: Option<String>,
    codex_live_email: Option<String>,
    gemini_live_email: Option<String>,

    usage: HashMap<String, UsageReport>, // key: email (Claude)
    codex_usage: HashMap<String, codex::Usage>, // key: "codex:<email>"
    gemini_usage: HashMap<String, gemini::Usage>, // key: "gemini:<email>"
    gemini_plan: HashMap<String, String>,        // key: email -> plan chip
    gemini_email_cache: HashMap<String, String>, // access_token -> email
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
    update_info: Option<UpdateInfo>,

    action_tx: UnboundedSender<Action>,
    login_in_flight: bool,
}

impl Engine {
    pub fn new(handle: Handle<PitStopTray>, action_tx: UnboundedSender<Action>) -> Self {
        Engine {
            client: reqwest::Client::new(),
            handle,
            store: ProfileStore::new(),
            codex_store: CodexStore::new(),
            gemini_store: GeminiStore::new(),
            settings: Settings::load(),
            active_email: None,
            codex_live_email: None,
            gemini_live_email: None,
            usage: HashMap::new(),
            codex_usage: HashMap::new(),
            gemini_usage: HashMap::new(),
            gemini_plan: HashMap::new(),
            gemini_email_cache: HashMap::new(),
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
            update_info: None,
            action_tx,
            login_in_flight: false,
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
                if let Some(email) = key.strip_prefix("gemini:") {
                    self.perform_gemini_switch(email, false, None).await;
                } else if let Some(email) = key.strip_prefix("codex:") {
                    self.perform_codex_switch(email, false, None).await;
                } else {
                    self.perform_switch(&key, false, None).await;
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
                if let Some(email) = key.strip_prefix("gemini:") {
                    let _ = self.gemini_store.remove(email);
                    self.gemini_usage.remove(&key);
                } else if let Some(email) = key.strip_prefix("codex:") {
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
            Action::OpenUrl(url) => {
                open_url(&url);
            }
            Action::SetSetting(change) => {
                self.apply_setting(change);
                self.render().await;
            }
            Action::Login { key } => {
                self.perform_login(key).await;
            }
            Action::LoginFinished { key, result } => {
                self.login_in_flight = false;
                match result {
                    Ok(()) => {
                        clear_after_login(
                            &mut self.next_fetch_allowed,
                            &mut self.failure_count,
                            &mut self.needs_action,
                            &key,
                        );
                        notify::post("Signed in", "Re-authenticated — refreshing usage…");
                        self.refresh_all().await;
                        self.render().await;
                    }
                    Err(e) => {
                        notify::post("Sign-in failed", &e);
                        self.render().await;
                    }
                }
            }
            Action::UpdateAndRelaunch => {
                if let Some(ref info) = self.update_info.clone() {
                    match updater::rebuild_and_relaunch(info).await {
                        Ok(()) => {} // unreachable: exec replaced this process
                        Err(e) => {
                            notify::post(
                                "Update failed — opening release page",
                                &e.to_string(),
                            );
                            open_url(&info.url);
                        }
                    }
                }
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
        // Silent daily check for a new GitHub release (best-effort; never blocks refresh).
        if let Some(result) = updater::check_if_due(&self.client).await {
            self.update_info = result;
        }
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
        self.refresh_gemini().await;
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

    async fn refresh_gemini(&mut self) {
        // Resolve + snapshot the live Antigravity account (keyring).
        let live_blob = match GeminiStore::live_blob().await {
            Ok(b) => b,
            Err(e) => {
                // Gemini not in use (no saved profiles): don't let a keyring-connect
                // failure clobber real Claude/Codex errors — short-circuit quietly.
                if self.gemini_store.profiles.is_empty() {
                    self.gemini_live_email = None;
                    return;
                }
                self.last_top_level_error = Some(e.to_string());
                None
            }
        };
        self.gemini_live_email = None;
        if let Some(blob) = &live_blob {
            if let Some(creds) = gemini::antigravity_creds(blob) {
                if let Ok(email) = self.gemini_email(&creds).await {
                    self.gemini_live_email = Some(email.clone());
                    let plan = self.gemini_plan.get(&email).cloned().unwrap_or_default();
                    if let Err(e) = self.gemini_store.snapshot(&email, blob, &plan) {
                        // Same guard: only surface a keyring error if Gemini is in use.
                        if !self.gemini_store.profiles.is_empty() {
                            self.last_top_level_error = Some(e.to_string());
                        }
                    }
                }
            }
        }
        self.gemini_store.load();

        let emails: Vec<String> = self
            .gemini_store
            .profiles
            .iter()
            .map(|p| p.email.clone())
            .collect();
        for email in emails {
            let key = format!("gemini:{email}");
            if !self.passed_backoff_gate(&key) {
                continue;
            }
            let is_live = Some(&email) == self.gemini_live_email.as_ref();
            match self.fetch_gemini_usage(&email, is_live).await {
                Ok((usage, plan)) => {
                    self.gemini_usage.insert(key.clone(), usage);
                    if !plan.is_empty() {
                        self.gemini_plan.insert(email.clone(), plan);
                    }
                    self.clear_fetch_error(&key);
                }
                Err(e) => self.record_fetch_error(e, &key),
            }
        }
    }

    /// Resolve the email for a credential blob, caching by access token. Refreshes
    /// in memory first if the token has aged out (never persists the live token).
    async fn gemini_email(&mut self, creds: &gemini::Creds) -> Result<String, ApiError> {
        if let Some(e) = self.gemini_email_cache.get(&creds.access_token) {
            return Ok(e.clone());
        }
        let token = if creds.is_expired() {
            match &creds.refresh_token {
                Some(rt) => gemini::refresh(&self.client, rt).await?.access_token,
                None => creds.access_token.clone(),
            }
        } else {
            creds.access_token.clone()
        };
        let email = gemini::fetch_email(&self.client, &token).await?;
        self.gemini_email_cache
            .insert(creds.access_token.clone(), email.clone());
        Ok(email)
    }

    /// Usage + plan chip for one account. Refreshes an expired token in memory;
    /// persists the rotated token to the snapshot ONLY for inactive accounts.
    async fn fetch_gemini_usage(
        &self,
        email: &str,
        is_live: bool,
    ) -> Result<(gemini::Usage, String), ApiError> {
        let blob = if is_live {
            GeminiStore::live_blob()
                .await
                .map_err(|e| ApiError::Network(e.to_string()))?
        } else {
            self.gemini_store
                .saved_blob(email)
                .map_err(|e| ApiError::Network(e.to_string()))?
        }
        .ok_or(ApiError::Unauthorized)?;
        let creds = gemini::antigravity_creds(&blob).ok_or(ApiError::Unauthorized)?;
        let access = if creds.is_expired() {
            let rt = creds.refresh_token.clone().ok_or(ApiError::Unauthorized)?;
            let refreshed = gemini::refresh(&self.client, &rt).await?;
            if !is_live {
                if let Some(patched) = gemini::patch_antigravity_blob(
                    &blob,
                    &refreshed.access_token,
                    refreshed.id_token.as_deref(),
                    &gemini::expiry_iso(refreshed.expires_at_ms),
                ) {
                    self.gemini_store
                        .store_refreshed_blob(&patched, email)
                        .map_err(|e| ApiError::Network(e.to_string()))?;
                }
            }
            refreshed.access_token
        } else {
            creds.access_token.clone()
        };
        let (project, plan) = gemini::load_project(&self.client, &access).await?;
        let Some(project) = project else {
            // Signed in, no Code Assist project → presence-only row (no bar).
            return Ok((
                gemini::Usage {
                    windows: vec![],
                    fetched_at: chrono::Local::now(),
                },
                plan,
            ));
        };
        let usage = gemini::fetch_usage(&self.client, &access, &project).await?;
        Ok((usage, plan))
    }

    // MARK: - Projection / thresholds / auto-switch

    fn record_usage_samples(&mut self) {
        let now = Instant::now();
        // Collect (account_key, window_label, utilisation) for every window we have
        // fresh data for. We explicitly avoid `self` borrows inside the loop.
        let mut windows: Vec<(String, String, f64)> = Vec::new();
        for (key, report) in &self.usage {
            if self.fetch_error.contains_key(key) {
                continue;
            }
            if let Some(u) = report.five_hour.and_then(|w| w.utilization) {
                windows.push((key.clone(), "5h".to_string(), u));
            }
            if let Some(u) = report.seven_day.and_then(|w| w.utilization) {
                windows.push((key.clone(), "7d".to_string(), u));
            }
            for s in &report.scoped {
                if let Some(u) = s.window.utilization {
                    windows.push((key.clone(), s.label.clone(), u));
                }
            }
        }
        for (key, cu) in &self.codex_usage {
            if self.fetch_error.contains_key(key) {
                continue;
            }
            for w in &cu.windows {
                windows.push((key.clone(), w.label.clone(), w.used_percent));
            }
        }
        for (key, gu) in &self.gemini_usage {
            if self.fetch_error.contains_key(key) {
                continue;
            }
            for w in &gu.windows {
                windows.push((key.clone(), w.label.clone(), w.used_percent));
            }
        }
        for (account_key, label, util) in windows {
            let wkey = format!("{account_key}#{label}");
            record_window_sample(&mut self.usage_history, &wkey, util, now);
        }
    }

    /// The windows PitStop may project toward their limit for `key`.
    /// For Claude: the 5-hour and weekly windows (when utilisation is present).
    /// For Codex: every rate-limit window the API reported.
    /// Returns `(label, current_util, resets_at)` tuples, matching the
    /// per-window sample keys `"{key}#{label}"` in `usage_history`.
    ///
    /// **Precedence:** Codex usage (`codex_usage`) is checked before Claude usage
    /// (`usage`). A key present in both maps (which shouldn't occur in practice,
    /// since Codex keys are prefixed `"codex:"`) would be served from codex_usage.
    /// A direct unit test of this precedence is not feasible without constructing
    /// a full `Engine`, which requires live `reqwest::Client` and `Handle<PitStopTray>`
    /// — types that cannot be instantiated in `#[cfg(test)]` without a running
    /// ksni tray. The precedence is instead verified by reading the source: the
    /// `if let Some(cu) = self.codex_usage.get(key)` early-return on line below
    /// executes before the `self.usage.get(key)` branch.
    fn projectable_windows(
        &self,
        key: &str,
    ) -> Vec<(String, f64, Option<DateTime<chrono::Utc>>)> {
        if let Some(cu) = self.codex_usage.get(key) {
            return cu
                .windows
                .iter()
                .map(|w| (w.label.clone(), w.used_percent, w.resets_at))
                .collect();
        }
        if let Some(gu) = self.gemini_usage.get(key) {
            return gu
                .windows
                .iter()
                .map(|w| (w.label.clone(), w.used_percent, w.resets_at))
                .collect();
        }
        if let Some(report) = self.usage.get(key) {
            let mut v: Vec<(String, f64, Option<DateTime<chrono::Utc>>)> = Vec::new();
            if let Some(u) = report.five_hour.and_then(|w| w.utilization) {
                v.push(("5h".to_string(), u, report.five_hour.and_then(|w| w.resets_at)));
            }
            if let Some(u) = report.seven_day.and_then(|w| w.utilization) {
                v.push(("7d".to_string(), u, report.seven_day.and_then(|w| w.resets_at)));
            }
            for s in &report.scoped {
                if let Some(u) = s.window.utilization {
                    v.push((s.label.clone(), u, s.window.resets_at));
                }
            }
            return v;
        }
        vec![]
    }

    /// The soonest "on pace to hit <window> limit" across an account's windows,
    /// or `None` when no window is trending toward its limit before it resets.
    /// Respects `settings.show_projection`. Matches Swift `projectionText(forKey:)`.
    fn projection_text(&self, key: &str) -> Option<String> {
        if !self.settings.show_projection {
            return None;
        }
        let mut soonest: Option<(String, DateTime<Local>)> = None;
        for (label, util, resets_at) in self.projectable_windows(key) {
            let wkey = format!("{key}#{label}");
            let samples = match self.usage_history.get(&wkey) {
                Some(s) => s,
                None => continue,
            };
            if let Some(date) = projected_full_from_samples(samples, util, resets_at) {
                let is_sooner = soonest.as_ref().is_none_or(|(_, prev)| date < *prev);
                if is_sooner {
                    soonest = Some((label, date));
                }
            }
        }
        let (label, date) = soonest?;
        Some(format!(
            "↗ on pace to hit {} limit ~{}",
            window_name(&label),
            format::short_clock(date)
        ))
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

        let gemini_utils: Vec<(String, Option<f64>)> = self
            .gemini_store
            .profiles
            .iter()
            .map(|p| {
                let key = format!("gemini:{}", p.email);
                let u = if self.fetch_error.contains_key(&key) {
                    None
                } else {
                    self.gemini_usage.get(&key).map(|r| r.max_utilization())
                };
                (p.email.clone(), u)
            })
            .collect();
        if let Some((target, reason)) = pick_auto_switch(
            self.gemini_live_email.as_deref(),
            threshold,
            self.last_auto_switch.get(&Provider::Gemini).copied(),
            &gemini_utils,
        ) {
            self.last_auto_switch.insert(Provider::Gemini, Instant::now());
            self.perform_gemini_switch(&target, true, Some(reason)).await;
            switched = true;
        }

        switched
    }

    /// Kick off a native OAuth re-login for `key`'s account. Runs the browser
    /// flow inside a detached `tokio::spawn` so the 90–180 s wait never blocks
    /// the select loop; the outcome comes back as `Action::LoginFinished`. A
    /// single `login_in_flight` guard rejects a second concurrent sign-in.
    async fn perform_login(&mut self, key: String) {
        if self.login_in_flight {
            notify::post(
                "Sign-in already in progress",
                "Finish or cancel the current sign-in before starting another.",
            );
            return;
        }
        self.login_in_flight = true;
        let (email, provider) = login_key_provider(&key);
        let adapter: Box<dyn LoginAdapter> = match provider {
            Provider::Codex => Box::new(oauth::CodexLoginAdapter),
            Provider::Claude => Box::new(oauth::ClaudeLoginAdapter),
            Provider::Gemini => Box::new(oauth::GeminiLoginAdapter),
        };
        let http = self.client.clone();
        let tx = self.action_tx.clone();
        tokio::spawn(async move {
            let result = oauth::run_login(&http, adapter.as_ref(), &email)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Action::LoginFinished { key, result });
        });
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

    /// Make `email` the live Antigravity account. Unlike Claude/Codex, the Gemini
    /// blob carries no email, so `gemini_store::switch_to` can't snapshot the
    /// outgoing account itself — we do it here FIRST (capture the current live
    /// keyring blob under the tracked live email) so the departing account's
    /// refresh token is never stranded. `switch_to` then form-matches the live
    /// keyring before writing the target's saved blob.
    async fn perform_gemini_switch(&mut self, email: &str, auto: bool, reason: Option<String>) {
        // Snapshot the outgoing live account first so its refresh token isn't stranded.
        if let Some(live) = self.gemini_live_email.clone() {
            if let Ok(Some(blob)) = GeminiStore::live_blob().await {
                let plan = self.gemini_plan.get(&live).cloned().unwrap_or_default();
                let _ = self.gemini_store.snapshot(&live, &blob, &plan);
            }
        }
        match self.gemini_store.switch_to(email).await {
            Ok(()) => {
                self.gemini_live_email = Some(email.to_string());
                let title = if auto {
                    format!("Auto-switched Gemini to {email}")
                } else {
                    format!("Switched Gemini to {email}")
                };
                notify::post(&title, &gemini_switch_body(reason));
            }
            Err(e) => {
                self.last_top_level_error = Some(format!("Couldn't switch Gemini account: {e}"));
                notify::post("Couldn't switch Gemini account", &e.to_string());
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
        for p in &self.gemini_store.profiles {
            if Some(&p.email) != self.gemini_live_email.as_ref() {
                removable.push((format!("{} · Gemini", p.email), format!("gemini:{}", p.email)));
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
            update_info: self.update_info.clone(),
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
        for c in &self.gemini_store.profiles {
            let plan = self.gemini_plan.get(&c.email).cloned().unwrap_or_else(|| c.plan_label.clone());
            let plan_label = if plan.is_empty() {
                gemini::SURFACE_TAG.to_string()
            } else {
                format!("{plan} · {}", gemini::SURFACE_TAG)
            };
            rows.push(MenuAccount {
                email: c.email.clone(),
                source: Source::Gemini,
                plan_label,
                is_active: Some(&c.email) == self.gemini_live_email.as_ref(),
            });
        }
        rows
    }

    fn headroom(&self, a: &MenuAccount) -> f64 {
        if a.is_codex() {
            self.codex_usage.get(&a.key()).map(|u| u.max_utilization()).unwrap_or(999.0)
        } else if a.is_gemini() {
            self.gemini_usage.get(&a.key()).map(|u| u.max_utilization()).unwrap_or(999.0)
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
                dashboard_url: provider.dashboard_url().map(str::to_string),
                rows,
            });
        }
        groups
    }

    fn build_row(&self, account: &MenuAccount) -> RowView {
        let key = account.key();
        let mut detail: Vec<String> = Vec::new();
        let mut data_date: Option<DateTime<Local>> = None;

        if account.is_codex() {
            if let Some(cu) = self.codex_usage.get(&key) {
                data_date = Some(cu.fetched_at);
                for w in &cu.windows {
                    let label = if w.label.is_empty() { "·" } else { &w.label };
                    detail.push(window_line(label, Some(w.used_percent), w.resets_at));
                }
            }
        } else if account.is_gemini() {
            if let Some(gu) = self.gemini_usage.get(&key) {
                data_date = Some(gu.fetched_at);
                for line in gemini_detail_lines(gu) {
                    detail.push(line);
                }
            }
        } else if let Some(report) = self.usage.get(&key) {
            data_date = Some(report.fetched_at);
            let f5 = report.five_hour.and_then(|w| w.utilization);
            detail.push(window_line("5h", f5, report.five_hour.and_then(|w| w.resets_at)));
            let f7 = report.seven_day.and_then(|w| w.utilization);
            detail.push(window_line("7d", f7, report.seven_day.and_then(|w| w.resets_at)));

            for line in scoped_window_lines(report) {
                detail.push(line);
            }

            let mut extras: Vec<String> = Vec::new();
            if report.extra_usage_enabled {
                extras.push(format!("Extra {}", format::percent(report.extra_usage_utilization)));
            }
            if !extras.is_empty() {
                detail.push(format!("       {}", extras.join(" · ")));
            }
        }

        if !self.fetch_error.contains_key(&key) {
            if let Some(text) = self.projection_text(&key) {
                detail.push(format!("       {text}"));
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
            login: login_eligible(self.needs_action.contains(&key), account.is_active),
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
                    consider(menu_label(key), report.max_utilization(), self.fetch_error.contains_key(key));
                }
                for (key, cu) in &self.codex_usage {
                    consider(menu_label(key), cu.max_utilization(), self.fetch_error.contains_key(key));
                }
                for (key, gu) in &self.gemini_usage {
                    consider(menu_label(key), gu.max_utilization(), self.fetch_error.contains_key(key));
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
        for s in &report.scoped {
            tip += &format!(" · {} {}", s.label, format::percent(s.window.utilization));
        }
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
            let detail = if acct.is_gemini() {
                self.gemini_usage
                    .get(&key)
                    .map(|gu| {
                        if gu.windows.is_empty() {
                            "—".to_string()
                        } else {
                            gu.windows
                                .iter()
                                .map(|w| format!("{} {}", w.label, format::percent(Some(w.used_percent))))
                                .collect::<Vec<_>>()
                                .join(" · ")
                        }
                    })
                    .or_else(|| self.fetch_error.get(&key).cloned())
                    .unwrap_or_else(|| "…".into())
            } else if acct.is_codex() {
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
            let provider = if acct.is_codex() {
                " (Codex)"
            } else if acct.is_gemini() {
                " (Gemini)"
            } else {
                ""
            };
            lines.push(format!("{marker} {}{provider} — {detail}", acct.email));
        }
        if lines.is_empty() {
            "No accounts yet — log in with `claude`.".to_string()
        } else {
            lines.join("\n")
        }
    }
}

/// Projected local time when one rate-limit window reaches 100 % at its current
/// least-squares pace. Returns `None` when any gate fails:
///   - current ≥ 100 %
///   - fewer than 4 samples
///   - span of samples < 600 s
///   - least-squares slope ≤ 0.0005 %/s  (flat or not rising meaningfully)
///   - secs_to_full ≤ 0  (already at or past 100 %)
///   - projected time is at or after `resets_at` (window resets first)
///
/// Matches the Swift `projectedFull(samples:current:resetsAt:)` gate set
/// introduced in PitStop macOS v0.3.1.
fn projected_full_from_samples(
    samples: &[(Instant, f64)],
    current: f64,
    resets_at: Option<DateTime<chrono::Utc>>,
) -> Option<DateTime<Local>> {
    if current >= 100.0 || samples.len() < 4 {
        return None;
    }
    let span = samples
        .last()?
        .0
        .duration_since(samples.first()?.0)
        .as_secs_f64();
    if span < 600.0 {
        return None;
    }
    let rate = slope_per_second(samples)?;
    if rate <= 0.0005 {
        return None;
    }
    let secs_to_full = (100.0 - current) / rate;
    if secs_to_full <= 0.0 {
        return None;
    }
    // A barely-used window projecting far into the future is noise, not a
    // warning — only surface once the window is meaningfully used (>= 25 %) or
    // the limit is genuinely close (ETA <= 3 h). Matches macOS d062687.
    if current < 25.0 && secs_to_full > 10800.0 {
        return None;
    }
    let projected = Local::now() + chrono::Duration::seconds(secs_to_full as i64);
    if let Some(reset) = resets_at {
        if projected >= reset.with_timezone(&Local) {
            return None;
        }
    }
    Some(projected)
}

/// Append one (timestamp, utilisation) sample to the per-window history,
/// pruning entries older than 30 min and clearing all entries when a window
/// appears to have reset (utilisation dropped by more than 2.0 points).
/// The `now` parameter is threaded from the caller so all windows in a
/// single refresh share the same timestamp.
fn record_window_sample(
    history: &mut HashMap<String, Vec<(Instant, f64)>>,
    key: &str,
    util: f64,
    now: Instant,
) {
    let entry = history.entry(key.to_string()).or_default();
    if let Some(last) = entry.last() {
        if util < last.1 - 2.0 {
            entry.clear(); // window reset
        }
    }
    entry.push((now, util));
    entry.retain(|(t, _)| now.duration_since(*t).as_secs_f64() <= 1800.0);
}

/// Least-squares slope (utilisation % per second) over time-stamped samples.
/// Returns `None` when all samples share the same timestamp (degenerate).
/// Matches the Swift `slopePerSecond` in AppDelegate.swift (v0.3.1 diff).
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
fn window_name(label: &str) -> String {
    match label {
        "5h"  => "5-hour".into(),
        "7d"  => "weekly".into(),
        "30d" => "monthly".into(),
        other => other.to_string(),
    }
}

/// Menu-bar label for an account key, namespacing non-Claude providers. A
/// `"codex:<email>"` key renders as `"<email> (Codex)"`, `"gemini:<email>"` as
/// `"<email> (Gemini)"`, and a bare Claude email is used verbatim.
fn menu_label(key: &str) -> String {
    if let Some(e) = key.strip_prefix("codex:") {
        format!("{e} (Codex)")
    } else if let Some(e) = key.strip_prefix("gemini:") {
        format!("{e} (Gemini)")
    } else {
        key.to_string()
    }
}

/// Detail lines for a Gemini row: one `window_line` per model quota window,
/// followed by an `extras_line` summarising the next-most-used models (mirrors
/// the Codex/Claude detail layout). Never logs the tokens the usage came from.
fn gemini_detail_lines(usage: &gemini::Usage) -> Vec<String> {
    let mut lines: Vec<String> = usage
        .windows
        .iter()
        .map(|w| window_line(&w.label, Some(w.used_percent), w.resets_at))
        .collect();
    if let Some(extras) = usage.extras_line() {
        lines.push(format!("       {extras}"));
    }
    lines
}

/// One detail bar line per scoped weekly limit (Fable, …), in report order.
fn scoped_window_lines(report: &UsageReport) -> Vec<String> {
    report
        .scoped
        .iter()
        .map(|s| window_line(&s.label, s.window.utilization, s.window.resets_at))
        .collect()
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

/// A row shows the coral Login action instead of Switch when its key is flagged
/// `needs_action` (token rejected) AND the row is inactive (switchable).
pub fn login_eligible(in_needs_action: bool, is_active: bool) -> bool {
    in_needs_action && !is_active
}

/// Split a menu key into `(email, provider)` for re-login. A `"codex:<email>"`
/// key targets Codex with the bare email; a bare email is a Claude key used
/// verbatim. Mirrors the `Action::Switch` key convention so `perform_login`
/// can pick the matching `LoginAdapter`.
fn login_key_provider(key: &str) -> (String, Provider) {
    if let Some(email) = key.strip_prefix("codex:") {
        (email.to_string(), Provider::Codex)
    } else if let Some(email) = key.strip_prefix("gemini:") {
        (email.to_string(), Provider::Gemini)
    } else {
        (key.to_string(), Provider::Claude)
    }
}

/// The success half of `Action::LoginFinished`: drop the Unauthorized backoff,
/// reset the failure counter, and clear the `needs_action` flag so the row
/// heals on the next `refresh_all`. Free function (mirroring
/// `record_window_sample`) so the state transition is unit-testable without a
/// live `Engine`.
fn clear_after_login(
    next_fetch_allowed: &mut HashMap<String, Instant>,
    failure_count: &mut HashMap<String, u32>,
    needs_action: &mut HashSet<String>,
    key: &str,
) {
    next_fetch_allowed.remove(key);
    failure_count.insert(key.to_string(), 0);
    needs_action.remove(key);
}

const GEMINI_TOS_CAVEAT: &str =
    "Note: Antigravity's terms discourage rotating this token — switch sparingly.";

/// Body text for a Gemini switch notification. Uses the auto-switch `reason` when
/// present, otherwise the manual-switch guidance, and ALWAYS appends the ToS
/// caveat (rotating the Antigravity token is discouraged by Google's terms).
fn gemini_switch_body(reason: Option<String>) -> String {
    let base = reason.unwrap_or_else(|| {
        "New Antigravity sessions use this account. Quit and reopen Antigravity to pick it up.".into()
    });
    format!("{base}\n{GEMINI_TOS_CAVEAT}")
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

/// Shells out to `xdg-open` to open `url` in the default browser.
/// Fire-and-forget: if `xdg-open` is absent the error goes to stderr only.
pub(crate) fn open_url(url: &str) {
    use std::process::{Command, Stdio};
    let result = Command::new("xdg-open")
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if result.is_err() {
        eprintln!("PitStop: xdg-open unavailable — visit {url}");
    }
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

    use std::collections::HashMap;

    // helper alias to avoid repeating the long type
    type History = HashMap<String, Vec<(Instant, f64)>>;

    // ── record_window_sample ───────────────────────────────────────────────

    #[test]
    fn per_window_key_format() {
        // Verify the key schema consumed everywhere downstream.
        assert_eq!(format!("{acc}#{lbl}", acc = "me@x.com", lbl = "7d"), "me@x.com#7d");
        assert_eq!(
            format!("{acc}#{lbl}", acc = "codex:me@x.com", lbl = "5h"),
            "codex:me@x.com#5h"
        );
    }

    #[test]
    fn reset_detection_clears_on_drop_over_2() {
        let mut h: History = HashMap::new();
        let now = Instant::now();
        record_window_sample(&mut h, "k#5h", 55.0, now);
        record_window_sample(&mut h, "k#5h", 50.0, now); // drop 5.0 > 2.0 → clear
        assert_eq!(h["k#5h"].len(), 1, "should have cleared on > 2.0 drop");
    }

    #[test]
    fn reset_detection_keeps_on_drop_of_exactly_2() {
        let mut h: History = HashMap::new();
        let now = Instant::now();
        record_window_sample(&mut h, "k#7d", 60.0, now);
        // 60.0 - 58.0 = 2.0, which is NOT > 2.0, so no clear
        record_window_sample(&mut h, "k#7d", 58.0, now);
        assert_eq!(h["k#7d"].len(), 2, "drop of exactly 2.0 must not reset");
    }

    #[test]
    fn reset_detection_keeps_on_rise() {
        let mut h: History = HashMap::new();
        let now = Instant::now();
        for util in [10.0, 20.0, 30.0, 40.0] {
            record_window_sample(&mut h, "k#5h", util, now);
        }
        assert_eq!(h["k#5h"].len(), 4, "rising series should accumulate");
    }

    #[test]
    fn samples_older_than_30_min_are_pruned() {
        let mut h: History = HashMap::new();
        let old = Instant::now() - Duration::from_secs(1801); // just over 30 min ago
        let now = Instant::now();
        // Manually seed an old entry
        h.entry("k#5h".to_string()).or_default().push((old, 10.0));
        record_window_sample(&mut h, "k#5h", 20.0, now);
        // The old entry is 1801 s before `now`; retain condition: now.duration_since(t) <= 1800
        assert_eq!(h["k#5h"].len(), 1, "old sample should have been pruned");
    }

    /// Extra hardening (Task 3): verify that `record_window_sample` stores the
    /// sample under the exact composite key produced by the production keying
    /// path (`"{account_key}#{label}"`), not just that the format! macro
    /// produces that string.
    #[test]
    fn record_window_sample_produces_exact_key() {
        let mut h: History = HashMap::new();
        let now = Instant::now();
        record_window_sample(&mut h, "codex:me@x#7d", 42.0, now);
        assert!(
            h.contains_key("codex:me@x#7d"),
            "expected key \"codex:me@x#7d\" in history map, got: {:?}",
            h.keys().collect::<Vec<_>>()
        );
    }

    use chrono::Utc;

    // ── projected_full_from_samples (gate checks) ──────────────────────────

    /// Helper: 4 evenly-spaced samples with a 0.1 /s slope and ≥600 s span.
    fn rising_samples(t0: Instant) -> Vec<(Instant, f64)> {
        // xs=[0,200,400,600] ys=[10,30,50,70]  slope=0.1/s  span=600s
        vec![
            (t0,                            10.0),
            (t0 + Duration::from_secs(200), 30.0),
            (t0 + Duration::from_secs(400), 50.0),
            (t0 + Duration::from_secs(600), 70.0),
        ]
    }

    #[test]
    fn gate_fewer_than_4_samples_returns_none() {
        let t0 = Instant::now();
        let three = vec![
            (t0,                            10.0),
            (t0 + Duration::from_secs(300), 25.0),
            (t0 + Duration::from_secs(700), 40.0),
        ];
        assert!(projected_full_from_samples(&three, 40.0, None).is_none());
    }

    #[test]
    fn gate_span_under_600s_returns_none() {
        let t0 = Instant::now();
        // 4 samples but only 599 s span
        let samples = vec![
            (t0,                            10.0),
            (t0 + Duration::from_secs(199), 30.0),
            (t0 + Duration::from_secs(399), 50.0),
            (t0 + Duration::from_secs(599), 70.0),
        ];
        assert!(projected_full_from_samples(&samples, 70.0, None).is_none());
    }

    #[test]
    fn gate_current_at_100_returns_none() {
        let t0 = Instant::now();
        let samples = rising_samples(t0);
        assert!(projected_full_from_samples(&samples, 100.0, None).is_none());
    }

    #[test]
    fn gate_slope_below_threshold_returns_none() {
        // slope = 0.006 / 600 = 0.00001 /s  (well below 0.0005 gate)
        let t0 = Instant::now();
        let samples = vec![
            (t0,                            10.000),
            (t0 + Duration::from_secs(200), 10.002),
            (t0 + Duration::from_secs(400), 10.004),
            (t0 + Duration::from_secs(600), 10.006),
        ];
        assert!(projected_full_from_samples(&samples, 10.006, None).is_none());
    }

    #[test]
    fn gate_resets_before_full_returns_none() {
        let t0 = Instant::now();
        let samples = rising_samples(t0);
        // slope≈0.1/s  current=70  secs_to_full≈300s
        // resets in 10 s → window resets before it fills → None
        let resets_soon = Some(Utc::now() + chrono::Duration::seconds(10));
        assert!(projected_full_from_samples(&samples, 70.0, resets_soon).is_none());
    }

    #[test]
    fn all_gates_pass_returns_some() {
        let t0 = Instant::now();
        let samples = rising_samples(t0);
        // slope=0.1/s, current=70, span=600s, 4 samples, no resets_at → Some
        let result = projected_full_from_samples(&samples, 70.0, None);
        assert!(result.is_some(), "all gates passed — expected Some, got None");
    }

    #[test]
    fn all_gates_pass_resets_after_full_returns_some() {
        let t0 = Instant::now();
        let samples = rising_samples(t0);
        // secs_to_full ≈ 300s; reset in 3600s (well after full) → Some
        let resets_later = Some(Utc::now() + chrono::Duration::seconds(3600));
        let result = projected_full_from_samples(&samples, 70.0, resets_later);
        assert!(result.is_some(), "resets after full → should project");
    }

    // ── Action::OpenUrl ────────────────────────────────────────────────────

    #[test]
    fn action_open_url_variant_round_trips() {
        let action = Action::OpenUrl("https://claude.ai/new#settings/usage".to_string());
        match &action {
            Action::OpenUrl(url) => {
                assert_eq!(url, "https://claude.ai/new#settings/usage");
            }
            _ => panic!("Expected Action::OpenUrl"),
        }
    }

    // ── in-app re-login (Task 8) ───────────────────────────────────────────

    #[test]
    fn login_eligible_requires_needs_action_and_inactive() {
        assert!(login_eligible(true, false)); // flagged + inactive -> Login
        assert!(!login_eligible(true, true)); // active row -> never
        assert!(!login_eligible(false, false)); // not flagged -> Switch
    }

    #[test]
    fn login_key_provider_splits_codex_prefix() {
        // A "codex:" key resolves to Codex with the bare email; anything else
        // is a Claude key used verbatim. This is the resolution `perform_login`
        // uses to pick the adapter. (`Provider` has no `Debug`, so we compare
        // with `==` rather than `assert_eq!`.)
        let (email, provider) = login_key_provider("codex:me@x.com");
        assert_eq!(email, "me@x.com");
        assert!(provider == Provider::Codex);

        let (email, provider) = login_key_provider("me@x.com");
        assert_eq!(email, "me@x.com");
        assert!(provider == Provider::Claude);

        // A bare "codex:" prefix with empty email still classifies as Codex.
        let (email, provider) = login_key_provider("codex:");
        assert!(email.is_empty());
        assert!(provider == Provider::Codex);

        // A "gemini:" key resolves to Gemini with the bare email.
        let (email, provider) = login_key_provider("gemini:me@x.com");
        assert_eq!(email, "me@x.com");
        assert!(provider == Provider::Gemini);
    }

    #[test]
    fn menu_label_namespaces_providers() {
        assert_eq!(menu_label("me@x"), "me@x"); // Claude (bare email)
        assert_eq!(menu_label("codex:me@x"), "me@x (Codex)");
        assert_eq!(menu_label("gemini:me@x"), "me@x (Gemini)");
    }

    #[test]
    fn gemini_detail_lines_include_windows_and_extras() {
        let u = gemini::Usage {
            windows: vec![
                gemini::UsageWindow { label: "3-pro".into(), used_percent: 22.0, resets_at: None },
                gemini::UsageWindow { label: "2.5-flash".into(), used_percent: 5.0, resets_at: None },
            ],
            fetched_at: chrono::Local::now(),
        };
        let lines = gemini_detail_lines(&u);
        // one line per window + one extras line
        assert_eq!(lines.len(), 3);
        assert!(lines.last().unwrap().contains("2.5-flash 5%"));
    }

    #[test]
    fn gemini_switch_body_carries_tos_caveat() {
        let body = gemini_switch_body(None);
        assert!(body.contains("Antigravity"));
        assert!(body.to_lowercase().contains("terms") || body.to_lowercase().contains("discourage"));
        let custom = gemini_switch_body(Some("me@a hit 92% — moved to me@b (10% used).".into()));
        assert!(custom.contains("moved to me@b"));
        assert!(custom.to_lowercase().contains("discourage")); // caveat still appended
    }

    // ── pick_auto_switch (auto-switch eligibility, shared by all providers) ──

    #[test]
    fn pick_auto_switch_moves_to_coolest_saved_account_above_threshold() {
        // Gemini-shaped inputs: the live account is over threshold; the coolest
        // OTHER saved account below threshold wins, and the reason names it.
        let utils = vec![
            ("live@x".to_string(), Some(92.0)),
            ("hot@x".to_string(), Some(80.0)),
            ("cool@x".to_string(), Some(10.0)),
            ("mid@x".to_string(), Some(40.0)),
        ];
        let (target, reason) =
            pick_auto_switch(Some("live@x"), 75.0, None, &utils).expect("should switch");
        assert_eq!(target, "cool@x", "picks the coolest account under threshold");
        assert!(reason.contains("live@x hit 92%"));
        assert!(reason.contains("moved to cool@x"));
    }

    #[test]
    fn pick_auto_switch_respects_cooldown() {
        // Same over-threshold live account, but a switch happened <180s ago.
        let utils = vec![
            ("live@x".to_string(), Some(92.0)),
            ("cool@x".to_string(), Some(10.0)),
        ];
        let recent = Instant::now() - Duration::from_secs(30);
        assert!(
            pick_auto_switch(Some("live@x"), 75.0, Some(recent), &utils).is_none(),
            "within the 180s cooldown, no switch"
        );
        // Past the cooldown, it switches again.
        let old = Instant::now() - Duration::from_secs(200);
        assert!(pick_auto_switch(Some("live@x"), 75.0, Some(old), &utils).is_some());
    }

    #[test]
    fn pick_auto_switch_skips_when_live_below_threshold() {
        let utils = vec![
            ("live@x".to_string(), Some(50.0)),
            ("cool@x".to_string(), Some(10.0)),
        ];
        assert!(
            pick_auto_switch(Some("live@x"), 75.0, None, &utils).is_none(),
            "live under threshold => no switch"
        );
    }

    #[test]
    fn pick_auto_switch_none_when_no_cooler_alternative() {
        // Live is hot, but every other saved account is also over threshold.
        let utils = vec![
            ("live@x".to_string(), Some(92.0)),
            ("also_hot@x".to_string(), Some(88.0)),
        ];
        assert!(pick_auto_switch(Some("live@x"), 75.0, None, &utils).is_none());
    }

    // ── Action::UpdateAndRelaunch (Task 6) ────────────────────────────────

    #[test]
    fn update_and_relaunch_action_is_clone() {
        // Verifies that Action::UpdateAndRelaunch variant exists and is Clone.
        let a = Action::UpdateAndRelaunch;
        let _b = a.clone();
    }

    #[test]
    fn clear_after_login_heals_row_state() {
        // The success half of LoginFinished: drop the 1-hour Unauthorized backoff,
        // reset the failure counter, and clear the needs_action flag so the row
        // heals on the next refresh.
        let mut nfa: HashMap<String, Instant> = HashMap::new();
        let mut fc: HashMap<String, u32> = HashMap::new();
        let mut na: std::collections::HashSet<String> = std::collections::HashSet::new();
        let key = "me@x.com";
        nfa.insert(key.to_string(), Instant::now() + Duration::from_secs(3600));
        fc.insert(key.to_string(), 5);
        na.insert(key.to_string());

        clear_after_login(&mut nfa, &mut fc, &mut na, key);

        assert!(!nfa.contains_key(key), "backoff should be cleared");
        assert_eq!(fc[key], 0, "failure count reset to zero");
        assert!(!na.contains(key), "needs_action flag cleared");
    }

    #[test]
    fn scoped_window_lines_one_per_scoped() {
        use crate::usage_api::{ScopedWindow, UsageReport, UsageWindow};
        let report = UsageReport {
            five_hour: None,
            seven_day: None,
            scoped: vec![
                ScopedWindow {
                    label: "Fable".into(),
                    window: UsageWindow { utilization: Some(13.0), resets_at: None },
                },
                ScopedWindow {
                    label: "Opus".into(),
                    window: UsageWindow { utilization: Some(40.0), resets_at: None },
                },
            ],
            extra_usage_enabled: false,
            extra_usage_utilization: None,
            fetched_at: Local::now(),
        };
        let lines = scoped_window_lines(&report);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Fable"), "line 0 should name the model: {}", lines[0]);
        assert!(lines[0].contains("13%"), "line 0 should show the percent: {}", lines[0]);
        assert!(lines[1].contains("Opus"));
        assert!(lines[1].contains("40%"));
    }

    // ── projected_full_from_samples — projection floor ─────────────────────

    /// Five evenly-spaced samples rising at `rate_per_hour` %/h over the past
    /// `minutes`, ending at `ending_at`. Least-squares slope over a perfectly
    /// linear series equals the true slope, so gate math is exact.
    fn rising_at(rate_per_hour: f64, minutes: f64, ending_at: f64) -> Vec<(Instant, f64)> {
        let now = Instant::now();
        (0..=4)
            .map(|i| {
                let m = -minutes + (i as f64) * (minutes / 4.0); // -minutes ..= 0
                let secs_ago = (-m * 60.0).round() as u64;
                let t = now - Duration::from_secs(secs_ago);
                (t, ending_at + rate_per_hour * (m / 60.0))
            })
            .collect()
    }

    #[test]
    fn floor_low_use_far_eta_returns_none() {
        // 2% used, ~11%/h → ETA ~9h out: below 25% and beyond 3h → suppressed.
        let samples = rising_at(11.0, 20.0, 2.0);
        assert!(projected_full_from_samples(&samples, 2.0, None).is_none());
    }

    #[test]
    fn floor_hot_window_projects() {
        // Same slope but already 26% used → above the floor → projects.
        let samples = rising_at(11.0, 20.0, 26.0);
        assert!(projected_full_from_samples(&samples, 26.0, None).is_some());
    }

    #[test]
    fn floor_low_use_imminent_eta_projects() {
        // 2% used but ~49%/h → ETA ~2h: close enough to warn even below 25%.
        let samples = rising_at(49.0, 20.0, 2.0);
        assert!(projected_full_from_samples(&samples, 2.0, None).is_some());
    }
}
