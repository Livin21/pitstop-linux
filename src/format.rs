//! Human-readable formatting for percentages, reset times, and relative
//! durations. Ported from `Format.swift`. Reset timestamps come in as UTC and
//! are rendered in the local timezone.

use chrono::{DateTime, Local, Utc};

pub fn percent(v: Option<f64>) -> String {
    match v {
        Some(v) => format!("{}%", v.round() as i64),
        None => "–".to_string(),
    }
}

/// "resets 9:49 PM (in 3h 34m)" / "resets Thu 5 Jun 10:29 AM (in 5d 16h)".
pub fn reset(date: Option<DateTime<Utc>>) -> String {
    let Some(date) = date else {
        return String::new();
    };
    let local = date.with_timezone(&Local);
    let stamp = if local.date_naive() == Local::now().date_naive() {
        local.format("%-I:%M %p").to_string()
    } else {
        local.format("%a %-d %b %-I:%M %p").to_string()
    };
    let secs = (date - Utc::now()).num_seconds() as f64;
    format!("resets {stamp} ({})", relative(secs))
}

pub fn relative(seconds: f64) -> String {
    let total = seconds.max(0.0) as i64;
    let (d, h, m) = (total / 86400, (total % 86400) / 3600, (total % 3600) / 60);
    if d > 0 {
        format!("in {d}d {h}h")
    } else if h > 0 {
        format!("in {h}h {m}m")
    } else if m > 0 {
        format!("in {m}m")
    } else {
        format!("in {total}s")
    }
}

/// Short reset stamp for the menu rows: "9:49 PM · 3h 34m" /
/// "Thu 10:29 AM · 5d 16h".
pub fn compact_reset(date: Option<DateTime<Utc>>) -> String {
    let Some(date) = date else {
        return String::new();
    };
    let local = date.with_timezone(&Local);
    let stamp = if local.date_naive() == Local::now().date_naive() {
        local.format("%-I:%M %p").to_string()
    } else {
        local.format("%a %-I:%M %p").to_string()
    };
    let secs = (date - Utc::now()).num_seconds() as f64;
    format!("{stamp} · {}", relative_short(secs))
}

pub fn relative_short(seconds: f64) -> String {
    let total = seconds.max(0.0) as i64;
    let (d, h, m) = (total / 86400, (total % 86400) / 3600, (total % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// "9:49:32 PM" — for "Updated …" / "showing … data".
pub fn updated(date: DateTime<Local>) -> String {
    date.format("%-I:%M:%S %p").to_string()
}

/// "3:40 PM" — short clock for the projection line ("on pace to hit limit ~3:40 PM").
/// Unlike `updated`, this omits seconds.
#[allow(dead_code)] // wired up in a later task
pub fn short_clock(date: DateTime<Local>) -> String {
    date.format("%-I:%M %p").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_clock_has_one_colon_and_am_pm() {
        let dt = chrono::Local::now();
        let s = short_clock(dt);
        assert!(
            s.ends_with("AM") || s.ends_with("PM"),
            "short_clock should end with AM or PM, got: {s}"
        );
        assert_eq!(
            s.matches(':').count(),
            1,
            "short_clock must have exactly one colon (H:MM), got: {s}"
        );
    }
}
