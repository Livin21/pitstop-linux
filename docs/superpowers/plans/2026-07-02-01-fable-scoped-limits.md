# Fable Scoped Weekly Limits Implementation Plan
> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (- [ ]) syntax for tracking.

**Goal:** Parse Claude's new `limits[]` array and render per-model scoped weekly limits ("Fable") as their own labelled bar rows, counting them fully toward the binding utilization so the menu bar, most-urgent pick, auto-switch, and threshold notifications all react.
**Architecture:** Additive data-model + parser change in `usage_api.rs` first (new `ScopedWindow`, `UsageReport.scoped`, `limit_window_entry`/`limit_window_by_kind` helpers, legacy fallback), then the binding math (`max_utilization`/`binding_window` span scoped) plus the `model.rs` has-data guard, then the `app.rs` display/projection wiring (bars, sampling, projection windows, tooltip, `--check`), then the v0.4.1 projection floor. The permanently-`null` `seven_day_opus`/`seven_day_sonnet` fields and their dead "Opus wk/Sonnet wk" extras lines are removed as part of the model change (Task 1) so every task ends green.
**Tech Stack:** Rust 2021; existing crates only — `serde_json`, `chrono`, `std::collections::HashMap`, `std::time::Instant`. No new dependencies.
**Depends on:** none. (Consumes the per-window projection infrastructure `record_window_sample` / `projectable_windows` / `projected_full_from_samples` already merged from the v0.3.1 projection plan.)

## Global Constraints
- Rust 2021; single tokio task (Engine::run select loop); ksni tray; no new threads/locks in the render path.
- Secrets only in 0600 files or the GNOME keyring; never logged; secret-bearing structs must not derive Debug.
- reqwest async; serde/serde_json; chrono; anyhow. Reuse existing ApiError.
- Each task ends green: cargo build clean, cargo test passes, cargo clippy --all-targets -- -D warnings clean, one commit.
---

## Background: current baseline

- `src/usage_api.rs:38-53` — `UsageWindow { utilization, resets_at }` (derives `Clone, Copy`) and `UsageReport { five_hour, seven_day, seven_day_opus, seven_day_sonnet, extra_usage_enabled, extra_usage_utilization, fetched_at }` (derives `Clone`).
- `src/usage_api.rs:57-72` — `max_utilization()` = `max(five_hour, seven_day)`; `binding_window()` picks 5h-vs-7d.
- `src/usage_api.rs:120-153` — `parse()` builds the report from top-level `five_hour`/`seven_day`/`seven_day_opus`/`seven_day_sonnet` via the private `window(any: Option<&Value>) -> Option<UsageWindow>` helper. `parse_iso8601` (line 156) parses RFC-3339 with or without fractional seconds.
- `src/model.rs:145-159` — `IndicatorMetric::utilization(&report)`; the `Binding` arm's has-data guard checks only `five_hour`/`seven_day`.
- `src/app.rs:1108-1124` — `build_row` Claude branch: two extras arms push "Opus wk"/"Sonnet wk" from `seven_day_opus`/`seven_day_sonnet`, then an "Extra" arm.
- `src/app.rs:605-641` — `record_usage_samples` Claude loop pushes only `"5h"`/`"7d"`.
- `src/app.rs:676-686` — `projectable_windows` Claude (`report`) branch returns only `"5h"`/`"7d"`.
- `src/app.rs:1250-1260` — `status_tip` builds `"5-hour X% · weekly Y%"`.
- `src/app.rs:1341-1372` — free fn `projected_full_from_samples(samples, current, resets_at)`; the `secs_to_full <= 0.0` guard is at line 1362.
- `src/main.rs:259-303` — `check_claude` returns `Ok((five, seven))`; `src/main.rs:83-89` prints only those two lines. `check_claude` does **not** enumerate windows generically today, so scoped bars must be added explicitly.

The only construction site of `UsageReport` is `parse()` (verified: `grep -rn "UsageReport {" src/` → `usage_api.rs:131` only). The only references to `seven_day_opus`/`seven_day_sonnet` are `usage_api.rs:48,49,134,135` and `app.rs:1109,1114` (verified: `grep -rn "seven_day_opus\|seven_day_sonnet" src/`). **Removing those fields therefore requires deleting the two `app.rs` extras arms in the same commit** — done in Task 1 so the build never breaks between tasks.

---

### Task 1: `usage_api.rs` data model + parse (ScopedWindow, scoped, limits fallback; remove opus/sonnet)

**Files:** Modify: `src/usage_api.rs` (add `ScopedWindow`; `UsageReport.scoped`; remove `seven_day_opus`/`seven_day_sonnet` fields + parse lines; add `limit_window_entry`/`limit_window_by_kind`; rewrite `parse` body; add `#[cfg(test)] mod tests`) / Modify: `src/app.rs:1108-1118` (delete the two dead Opus/Sonnet extras arms — required for a green build since the fields are gone)
**Interfaces:** Consumes: nothing | Produces: `pub struct ScopedWindow { pub label: String, pub window: UsageWindow }`; `UsageReport.scoped: Vec<ScopedWindow>`; `fn limit_window_entry(&Value) -> Option<UsageWindow>`

- [ ] **Step 1: Write the failing tests**

Append this block at the end of `src/usage_api.rs` (after the final `}` of `refresh`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scoped_weekly_limit() {
        let data = br#"{
          "five_hour": {"utilization": 64.0, "resets_at": "2026-07-02T00:50:00.818202+00:00"},
          "seven_day": {"utilization": 7.0, "resets_at": "2026-07-05T00:00:00+00:00"},
          "limits": [
            {"kind": "session", "group": "session", "percent": 64, "resets_at": "2026-07-02T00:50:00.818202+00:00"},
            {"kind": "weekly_all", "group": "weekly", "percent": 7, "resets_at": "2026-07-05T00:00:00+00:00"},
            {"kind": "weekly_scoped", "group": "weekly", "percent": 13,
             "resets_at": "2026-07-05T00:00:00+00:00",
             "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}}
          ]
        }"#;
        let report = parse(data).expect("valid payload");
        assert_eq!(report.scoped.len(), 1);
        assert_eq!(report.scoped[0].label, "Fable");
        assert_eq!(report.scoped[0].window.utilization, Some(13.0));
        assert!(report.scoped[0].window.resets_at.is_some());
        // Legacy top-level fields are preferred over the limits[] fallback.
        assert_eq!(report.five_hour.and_then(|w| w.utilization), Some(64.0));
        // 6-digit fractional-second reset parses.
        assert!(report.five_hour.and_then(|w| w.resets_at).is_some());
    }

    #[test]
    fn scoped_label_falls_back_to_scoped() {
        let data = br#"{"limits": [{"kind": "weekly_scoped", "percent": 5}]}"#;
        let report = parse(data).expect("valid payload");
        assert_eq!(report.scoped.len(), 1);
        assert_eq!(report.scoped[0].label, "Scoped");
        assert_eq!(report.scoped[0].window.utilization, Some(5.0));
    }

    #[test]
    fn falls_back_to_limits_for_main_windows() {
        let data = br#"{"limits": [
          {"kind": "session", "percent": 42, "resets_at": "2026-07-02T00:50:00+00:00"},
          {"kind": "weekly_all", "percent": 24}
        ]}"#;
        let report = parse(data).expect("valid payload");
        assert_eq!(report.five_hour.and_then(|w| w.utilization), Some(42.0));
        assert!(report.five_hour.and_then(|w| w.resets_at).is_some());
        assert_eq!(report.seven_day.and_then(|w| w.utilization), Some(24.0));
    }

    #[test]
    fn unknown_limit_kinds_ignored() {
        let data = br#"{"limits": [{"kind": "hourly_lunar", "percent": 99}], "five_hour": {"utilization": 1}}"#;
        let report = parse(data).expect("valid payload");
        assert!(report.scoped.is_empty());
        assert_eq!(report.max_utilization(), 1.0);
    }
}
```

- [ ] **Step 2: Run tests, verify they fail**

```
cargo test --lib usage_api::tests
```

Expected: `FAIL` — `error[E0609]: no field \`scoped\` on type \`usage_api::UsageReport\`` (the `scoped` field does not exist yet).

- [ ] **Step 3: Minimal implementation**

**3a.** In `src/usage_api.rs`, add the `ScopedWindow` struct immediately after the `UsageWindow` struct (after line 42):

```rust
/// A per-model weekly limit ("Fable", …) from the `limits[]` array's
/// `weekly_scoped` entries. An independent cap: hitting it blocks only that
/// model, but per user preference it still counts toward the binding number.
#[derive(Clone)]
pub struct ScopedWindow {
    pub label: String,
    pub window: UsageWindow,
}
```

**3b.** In the `UsageReport` struct (lines 44-53), remove the `seven_day_opus` and `seven_day_sonnet` fields and add `scoped` after `seven_day`, so the struct reads:

```rust
#[derive(Clone)]
pub struct UsageReport {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
    pub scoped: Vec<ScopedWindow>,
    pub extra_usage_enabled: bool,
    pub extra_usage_utilization: Option<f64>,
    pub fetched_at: DateTime<Local>,
}
```

**3c.** Rewrite the `parse` body (lines 120-140) so that, after the existing `extra_usage` block, it computes the limits fallback + scoped windows and constructs the new shape. Replace the `Ok(UsageReport { … })` literal (and add the limits logic just above it):

```rust
    let empty: Vec<Value> = Vec::new();
    let limits = root
        .get("limits")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    // 5h/7d keep coming from the legacy top-level fields (more precision);
    // fall back to the limits[] session / weekly_all entries when absent.
    let mut five_hour = window(root.get("five_hour"));
    if five_hour.is_none() {
        five_hour = limit_window_by_kind(limits, "session");
    }
    let mut seven_day = window(root.get("seven_day"));
    if seven_day.is_none() {
        seven_day = limit_window_by_kind(limits, "weekly_all");
    }
    let scoped: Vec<ScopedWindow> = limits
        .iter()
        .filter(|e| e.get("kind").and_then(Value::as_str) == Some("weekly_scoped"))
        .filter_map(|e| {
            let window = limit_window_entry(e)?;
            let label = e
                .get("scope")
                .and_then(|s| s.get("model"))
                .and_then(|m| m.get("display_name"))
                .and_then(Value::as_str)
                .unwrap_or("Scoped")
                .to_string();
            Some(ScopedWindow { label, window })
        })
        .collect();
    Ok(UsageReport {
        five_hour,
        seven_day,
        scoped,
        extra_usage_enabled: extra_enabled,
        extra_usage_utilization: extra_util,
        fetched_at: Local::now(),
    })
```

**3d.** Add the two `limits[]` helpers immediately after the existing `fn window(any: Option<&Value>) -> Option<UsageWindow>` (after line 153):

```rust
/// A `UsageWindow` from a `limits[]` entry: reads `percent` (NOT `utilization`)
/// plus `resets_at`. Returns `None` when `percent` is absent.
fn limit_window_entry(entry: &Value) -> Option<UsageWindow> {
    let percent = entry.get("percent").and_then(Value::as_f64)?;
    let resets_at = entry
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(parse_iso8601);
    Some(UsageWindow {
        utilization: Some(percent),
        resets_at,
    })
}

/// The first `limits[]` entry whose `kind` matches, as a `UsageWindow`.
fn limit_window_by_kind(limits: &[Value], kind: &str) -> Option<UsageWindow> {
    limits
        .iter()
        .find(|e| e.get("kind").and_then(Value::as_str) == Some(kind))
        .and_then(limit_window_entry)
}
```

**3e.** In `src/app.rs`, delete the two dead extras arms in `build_row` (lines 1109-1118) — the block from `if let Some(v) = report.seven_day_opus…` through the closing `}` of the `seven_day_sonnet` arm — leaving the `let mut extras: Vec<String> = Vec::new();` line and the `if report.extra_usage_enabled { … }` arm intact. After the edit the extras block reads:

```rust
            let mut extras: Vec<String> = Vec::new();
            if report.extra_usage_enabled {
                extras.push(format!("Extra {}", format::percent(report.extra_usage_utilization)));
            }
            if !extras.is_empty() {
                detail.push(format!("       {}", extras.join(" · ")));
            }
```

- [ ] **Step 4: Run tests, verify they pass**

```
cargo test --lib usage_api::tests
```

Expected: all 4 tests `PASS`. Then:

```
cargo build && cargo test && cargo clippy --all-targets -- -D warnings
```

Expected: clean build, full suite green, zero clippy warnings. Verify the fields are gone: `grep -rn "seven_day_opus\|seven_day_sonnet" src/` returns nothing.

- [ ] **Step 5: Commit**

```
git add src/usage_api.rs src/app.rs && git commit -m "$(cat <<'EOF'
feat(fable): parse scoped weekly limits from the usage limits[] array

Add ScopedWindow + UsageReport.scoped, populated from limits[] entries
with kind == "weekly_scoped" (label = scope.model.display_name, fallback
"Scoped", utilization = percent). session/weekly_all entries back-fill the
legacy 5h/7d fields when absent; unknown kinds are ignored. Remove the
permanently-null seven_day_opus/seven_day_sonnet fields and their dead
"Opus wk"/"Sonnet wk" extras lines.
EOF
)"
```

---

### Task 2: Binding math spans scoped + `IndicatorMetric` has-data guard

**Files:** Modify: `src/usage_api.rs:57-72` (`max_utilization` + `binding_window`) and its `#[cfg(test)] mod tests` (add 1 test) / Modify: `src/model.rs:147-155` (`IndicatorMetric::Binding` has-data guard) and its `#[cfg(test)] mod tests` (add 2 tests)
**Interfaces:** Consumes: `UsageReport.scoped` (Task 1) | Produces: `max_utilization`/`binding_window` covering `[5h, 7d] + scoped`; `IndicatorMetric::Binding` reacting to scoped-only reports

- [ ] **Step 1: Write the failing tests**

Append to the `#[cfg(test)] mod tests` block in `src/usage_api.rs`:

```rust
    #[test]
    fn binding_includes_scoped() {
        let data = br#"{"five_hour": {"utilization": 10}, "seven_day": {"utilization": 20},
         "limits": [{"kind": "weekly_scoped", "percent": 95,
                     "resets_at": "2026-07-05T00:00:00+00:00",
                     "scope": {"model": {"display_name": "Fable"}}}]}"#;
        let report = parse(data).expect("valid payload");
        assert_eq!(report.max_utilization(), 95.0);
        // Fable's reset stamp drives threshold notifications when it is binding.
        assert!(report.binding_window().and_then(|w| w.resets_at).is_some());
    }
```

Append to the `#[cfg(test)] mod tests` block in `src/model.rs`:

```rust
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
```

- [ ] **Step 2: Run tests, verify they fail**

```
cargo test --lib binding_includes_scoped binding_metric_has_data_from_scoped_only binding_metric_none_without_any_data
```

Expected: `FAIL` — `binding_includes_scoped` asserts `95.0` but current `max_utilization` returns `20.0` (`assertion `left == right` failed: left: 20.0, right: 95.0`); `binding_metric_has_data_from_scoped_only` asserts `Some(42.0)` but the current guard ignores scoped and returns `None`.

- [ ] **Step 3: Minimal implementation**

**3a.** In `src/usage_api.rs`, replace the `max_utilization` and `binding_window` methods (lines 57-72) with:

```rust
    /// The binding constraint — whichever window is closest to its limit,
    /// now including per-model scoped weekly limits (Fable).
    pub fn max_utilization(&self) -> f64 {
        self.binding_window()
            .and_then(|w| w.utilization)
            .unwrap_or(0.0)
    }

    /// The window driving `max_utilization`, for reset-time display.
    /// First-wins on ties, so 5h beats 7d beats scoped at equal utilization.
    pub fn binding_window(&self) -> Option<UsageWindow> {
        let mut best: Option<UsageWindow> = None;
        let candidates = [self.five_hour, self.seven_day]
            .into_iter()
            .flatten()
            .chain(self.scoped.iter().map(|s| s.window));
        for w in candidates {
            let is_better = match best {
                None => true,
                Some(b) => w.utilization.unwrap_or(0.0) > b.utilization.unwrap_or(0.0),
            };
            if is_better {
                best = Some(w);
            }
        }
        best
    }
```

**3b.** In `src/model.rs`, the `IndicatorMetric::Binding` arm (lines 147-155) — extend the `has` check to include scoped:

```rust
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
```

- [ ] **Step 4: Run tests, verify they pass**

```
cargo test --lib binding_includes_scoped binding_metric_has_data_from_scoped_only binding_metric_none_without_any_data
```

Expected: all 3 `PASS`. Then:

```
cargo build && cargo test && cargo clippy --all-targets -- -D warnings
```

Expected: clean build, full suite green, zero clippy warnings.

- [ ] **Step 5: Commit**

```
git add src/usage_api.rs src/model.rs && git commit -m "$(cat <<'EOF'
feat(fable): count scoped limits toward binding utilization

max_utilization/binding_window now range over [5h, 7d] + scoped windows
(first-wins on ties). IndicatorMetric::Binding's has-data guard includes
scoped, so the menu bar %, most-urgent pick, auto-switch, and threshold
notifications all react when Fable runs hot.
EOF
)"
```

---

### Task 3: `app.rs` display/sampling/projection wiring + `--check` scoped lines

**Files:** Modify: `src/app.rs` (add free fn `scoped_window_lines`; `build_row` Claude branch; `record_usage_samples` Claude loop; `projectable_windows` Claude branch; `status_tip`) and its `#[cfg(test)] mod tests` (add 1 test) / Modify: `src/main.rs` (`check_claude` return + `check` print loop)
**Interfaces:** Consumes: `UsageReport.scoped` (Task 1) | Produces: one bar line + one projection sample + one tooltip fragment + one `--check` line per scoped window

- [ ] **Step 1: Write the failing test**

Append inside the `#[cfg(test)] mod tests` block in `src/app.rs`:

```rust
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
```

- [ ] **Step 2: Run tests, verify they fail**

```
cargo test --lib scoped_window_lines_one_per_scoped
```

Expected: `FAIL` — `error[E0425]: cannot find function \`scoped_window_lines\` in this scope`.

- [ ] **Step 3: Minimal implementation**

**3a.** Add the free function `scoped_window_lines` immediately above the existing `fn window_line(…)` (before line 1458):

```rust
/// One detail bar line per scoped weekly limit (Fable, …), in report order.
fn scoped_window_lines(report: &UsageReport) -> Vec<String> {
    report
        .scoped
        .iter()
        .map(|s| window_line(&s.label, s.window.utilization, s.window.resets_at))
        .collect()
}
```

**3b.** In `build_row` (Claude branch), insert the scoped bars after the `"7d"` line (after line 1106, before `let mut extras…`):

```rust
            for line in scoped_window_lines(report) {
                detail.push(line);
            }
```

**3c.** In `record_usage_samples`, inside the `for (key, report) in &self.usage` loop, after the `seven_day` push (after line 619, before the loop's closing `}`):

```rust
            for s in &report.scoped {
                if let Some(u) = s.window.utilization {
                    windows.push((key.clone(), s.label.clone(), u));
                }
            }
```

**3d.** In `projectable_windows`, inside the `if let Some(report) = self.usage.get(key)` branch, after the `"7d"` push (after line 683, before `return v;`):

```rust
            for s in &report.scoped {
                if let Some(u) = s.window.utilization {
                    v.push((s.label.clone(), u, s.window.resets_at));
                }
            }
```

**3e.** In `status_tip`, after the `tip` initializer (after line 1255, before the `if let Some(err)…` block):

```rust
        for s in &report.scoped {
            tip += &format!(" · {} {}", s.label, format::percent(s.window.utilization));
        }
```

**3f.** In `src/main.rs`, make `check_claude` also surface scoped windows. Change its return type (line 264) to `Result<(String, String, Vec<String>), String>`, and replace the final `Ok((five, seven))` (line 302) with:

```rust
    let scoped: Vec<String> = report
        .scoped
        .iter()
        .map(|s| {
            format!(
                "{}  {}  {}",
                s.label,
                format::percent(s.window.utilization),
                format::reset(s.window.resets_at)
            )
        })
        .collect();
    Ok((five, seven, scoped))
```

Then update the `check()` call site (lines 84-87) to print the scoped lines:

```rust
            Ok((five, seven, scoped)) => {
                println!("   5-hour  {five}");
                println!("   weekly  {seven}");
                for line in &scoped {
                    println!("   {line}");
                }
            }
```

- [ ] **Step 4: Run tests, verify they pass**

```
cargo test --lib scoped_window_lines_one_per_scoped
```

Expected: `PASS`. Then:

```
cargo build && cargo test && cargo clippy --all-targets -- -D warnings
```

Expected: clean build, full suite green, zero clippy warnings.

- [ ] **Step 5: Commit**

```
git add src/app.rs src/main.rs && git commit -m "$(cat <<'EOF'
feat(fable): render scoped limits as bars, sample + project them

build_row pushes one labelled bar per scoped window after 7d;
record_usage_samples and projectable_windows key them as "{key}#{label}"
so projections work ("on pace to hit Fable limit ~4:10 PM"); status_tip
appends " · {label} {percent}"; --check lists scoped windows too.
EOF
)"
```

---

### Task 4: Projection floor (≥25% used or ETA ≤3h)

**Files:** Modify: `src/app.rs:1341-1372` (`projected_full_from_samples` — add floor guard) and its `#[cfg(test)] mod tests` (add helper + 3 tests)
**Interfaces:** Consumes: `projected_full_from_samples` (existing) | Produces: same signature; a barely-used window far from full no longer projects

- [ ] **Step 1: Write the failing tests**

Append inside the `#[cfg(test)] mod tests` block in `src/app.rs`:

```rust
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
```

- [ ] **Step 2: Run tests, verify they fail**

```
cargo test --lib floor_low_use_far_eta_returns_none floor_hot_window_projects floor_low_use_imminent_eta_projects
```

Expected: `floor_hot_window_projects` and `floor_low_use_imminent_eta_projects` already `PASS` (they should project), but `floor_low_use_far_eta_returns_none` `FAIL`s (`assertion failed: …is_none()`) — today a 2%-used window 9h out still projects.

- [ ] **Step 3: Minimal implementation**

In `src/app.rs`, in `projected_full_from_samples`, add the floor guard immediately after the `if secs_to_full <= 0.0 { return None; }` check (after line 1364, before `let projected = …`):

```rust
    // A barely-used window projecting far into the future is noise, not a
    // warning — only surface once the window is meaningfully used (>= 25 %) or
    // the limit is genuinely close (ETA <= 3 h). Matches macOS d062687.
    if current < 25.0 && secs_to_full > 10800.0 {
        return None;
    }
```

- [ ] **Step 4: Run tests, verify they pass**

```
cargo test --lib floor_low_use_far_eta_returns_none floor_hot_window_projects floor_low_use_imminent_eta_projects
```

Expected: all 3 `PASS`. Then:

```
cargo build && cargo test && cargo clippy --all-targets -- -D warnings
```

Expected: clean build, full suite green, zero clippy warnings.

- [ ] **Step 5: Commit**

```
git add src/app.rs && git commit -m "$(cat <<'EOF'
feat(fable): floor the usage projection at 25% used or a 3-hour ETA

A 2%-used window "on pace to hit limit" nine hours out is alarmist noise;
only project once the window is meaningfully used or the limit is close.
EOF
)"
```

---

## Self-review checklist

- [x] **`percent` NOT `utilization`** (Risk 1): `limit_window_entry` reads `entry.get("percent")` — verified against Mac `limitWindow` (`74a313a`).
- [x] **`ScopedWindow` shape:** `pub struct ScopedWindow { pub label: String, pub window: UsageWindow }`, `#[derive(Clone)]` (no Debug — matches `UsageWindow`/`UsageReport`); `UsageReport.scoped: Vec<ScopedWindow>` added, `seven_day_opus`/`seven_day_sonnet` removed.
- [x] **Legacy fallback:** top-level `five_hour`/`seven_day` preferred; fall back to limits `session`/`weekly_all` only when absent — matches Mac `if report.fiveHour == nil { … }`.
- [x] **Label fallback:** `scope.model.display_name` → `"Scoped"`; deep-`get` chain tolerates missing `scope`/`model`.
- [x] **Unknown-kind tolerance:** `filter(kind == "weekly_scoped")` drops all other kinds; `unknown_limit_kinds_ignored` proves `scoped` stays empty and parse succeeds.
- [x] **Binding math:** `[5h, 7d] + scoped`, first-wins on ties (strict `>`) — matches Mac `bindingWindow`; `max_utilization = binding_window?.utilization ?? 0`.
- [x] **has-data guard:** `model.rs` `Binding` arm includes `scoped.iter().any(...)`; tested both directions.
- [x] **Field removal is atomic with references:** Task 1 deletes the `usage_api.rs` fields/parse lines AND the `app.rs` Opus/Sonnet extras arms in one commit; `grep` confirms no other references — every task ends green.
- [x] **Display/sampling/projection:** `scoped_window_lines` (tested) drives `build_row`; `record_usage_samples`/`projectable_windows` key scoped as `"{key}#{label}"` (same schema as 5h/7d/Codex); `status_tip` appends `" · {label} {percent}"`; `--check` lists scoped windows.
- [x] **`window_name` passthrough:** scoped labels (e.g. "Fable") are unknown to `window_name`, which returns them unchanged — no change needed.
- [x] **Projection floor:** `current < 25.0 && secs_to_full > 10800.0 → None`, placed after the `secs_to_full <= 0.0` guard — matches spec Plan-1 item 5 and Mac `d062687` (2%/9h→None, 26%→Some, 2%/2h→Some tested via exact linear samples).
- [x] **No new deps:** `serde_json::Value`, `chrono`, `HashMap`, `Instant` only.
- [x] **No placeholders:** every step contains complete, compilable code and a real command.
