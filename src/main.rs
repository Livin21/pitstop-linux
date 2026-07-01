mod app;
mod claude_store;
mod codex;
mod codex_store;
mod credentials;
mod format;
mod icon;
mod model;
mod notify;
#[allow(dead_code)] // wired into the engine in the login task
mod oauth;
mod secret_store;
mod settings;
mod tray;
mod usage_api;
mod util;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--check") {
        check().await;
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| a == "--export-icon") {
        let path = args.get(i + 1).map(String::as_str).unwrap_or("pitstop.png");
        icon::write_app_icon(path)?;
        println!("Wrote {path}");
        return Ok(());
    }
    run_tray().await
}

/// Register the tray icon and hand control to the engine's refresh loop.
async fn run_tray() -> Result<()> {
    use ksni::TrayMethods;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let tray = tray::PitStopTray {
        view: tray::TrayView::loading(),
        tx,
    };
    let handle = tray.spawn().await.map_err(|e| {
        anyhow::anyhow!(
            "Couldn't register the tray icon: {e}. Is a StatusNotifier host running? \
             On Cinnamon you may need `snixembed` or Mint's indicator support enabled."
        )
    })?;
    app::Engine::new(handle).run(rx).await;
    Ok(())
}

/// `pitstop --check` — headless diagnostic mirroring the macOS build: print
/// saved accounts and live usage to stdout without any GUI.
async fn check() {
    let client = reqwest::Client::new();

    let mut store = claude_store::ProfileStore::new();
    if let Err(e) = store.capture_current() {
        println!("capture failed: {e}");
    }
    store.load();
    let active = credentials::active_email().unwrap_or_else(|| "<none>".into());
    println!("active account: {active}");

    for profile in &store.profiles {
        let is_active = profile.email == active;
        println!(
            "\n{} {}  [{}]",
            if is_active { "●" } else { "○" },
            profile.email,
            profile.plan_label()
        );
        match check_claude(&client, &store, &profile.email, is_active).await {
            Ok((five, seven)) => {
                println!("   5-hour  {five}");
                println!("   weekly  {seven}");
            }
            Err(e) => println!("   error: {e}"),
        }
    }

    if codex::is_present() {
        let mut cstore = codex_store::CodexStore::new();
        if let Err(e) = cstore.capture_current() {
            println!("\nCodex capture failed: {e}");
        }
        cstore.load();
        let live = cstore.live_email();
        if cstore.profiles.is_empty() {
            println!("\nCodex: installed but not signed in with a ChatGPT account");
        }
        for profile in &cstore.profiles {
            let is_live = Some(&profile.email) == live.as_ref();
            println!(
                "\n{} {}  [{}]  · Codex",
                if is_live { "▣" } else { "▢" },
                profile.email,
                profile.plan_label
            );
            match check_codex(&client, &cstore, &profile.email, is_live).await {
                Ok(usage) => {
                    if usage.windows.is_empty() {
                        println!("   (no usage windows reported)");
                    }
                    for w in &usage.windows {
                        let label = if w.label.is_empty() { "window" } else { &w.label };
                        println!(
                            "   {label}  {}  {}",
                            format::percent(Some(w.used_percent)),
                            format::reset(w.resets_at)
                        );
                    }
                }
                Err(e) => println!("   error: {e}"),
            }
        }
    }
}

async fn check_claude(
    client: &reqwest::Client,
    store: &claude_store::ProfileStore,
    email: &str,
    is_active: bool,
) -> Result<(String, String), String> {
    let blob = store
        .blob(email, is_active)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no stored credentials".to_string())?;
    let mut creds = credentials::parse_blob(&blob).map_err(|e| e.to_string())?;
    if creds.is_expired() {
        if let Some(rt) = creds.refresh_token.clone() {
            println!("   token expired — refreshing…");
            let fresh = usage_api::refresh(client, &rt)
                .await
                .map_err(|e| e.to_string())?;
            let patched = credentials::patch_blob(
                &blob,
                &fresh.access_token,
                fresh.refresh_token.as_deref(),
                fresh.expires_at_ms,
            )
            .map_err(|e| e.to_string())?;
            store
                .store_refreshed_blob(&patched, email, is_active)
                .map_err(|e| e.to_string())?;
            creds.access_token = fresh.access_token;
        }
    }
    let report = usage_api::fetch_usage(client, &creds.access_token)
        .await
        .map_err(|e| e.to_string())?;
    let five = format!(
        "{}  {}",
        format::percent(report.five_hour.and_then(|w| w.utilization)),
        format::reset(report.five_hour.and_then(|w| w.resets_at))
    );
    let seven = format!(
        "{}  {}",
        format::percent(report.seven_day.and_then(|w| w.utilization)),
        format::reset(report.seven_day.and_then(|w| w.resets_at))
    );
    Ok((five, seven))
}

async fn check_codex(
    client: &reqwest::Client,
    store: &codex_store::CodexStore,
    email: &str,
    is_live: bool,
) -> Result<codex::Usage, String> {
    let blob = store
        .blob(email, is_live)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no usable credentials".to_string())?;
    let creds = codex::credentials(&blob).ok_or_else(|| "no usable credentials".to_string())?;
    match codex::fetch_usage(client, &creds).await {
        Ok(u) => Ok(u),
        Err(usage_api::ApiError::Unauthorized) if !is_live => {
            let rt = creds
                .refresh_token
                .clone()
                .ok_or_else(|| "Codex token expired".to_string())?;
            println!("   token expired — refreshing…");
            let refreshed = codex::refresh(client, &rt).await.map_err(|e| e.to_string())?;
            let patched =
                codex::patching(&blob, &refreshed).ok_or_else(|| "malformed blob".to_string())?;
            let fresh =
                codex::credentials(&patched).ok_or_else(|| "malformed blob".to_string())?;
            store
                .store_refreshed_blob(&patched, email)
                .map_err(|e| e.to_string())?;
            codex::fetch_usage(client, &fresh)
                .await
                .map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}
