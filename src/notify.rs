//! Desktop notifications. The macOS build used `UNUserNotificationCenter`; on
//! Linux this shells out to `notify-send` (libnotify), which every mainstream
//! desktop — Cinnamon included — services over D-Bus. Fire-and-forget: if
//! `notify-send` is missing the message still goes to stderr.

use std::process::{Command, Stdio};

pub fn post(title: &str, body: &str) {
    let spawned = Command::new("notify-send")
        .arg("--app-name=PitStop")
        .arg("--icon=pitstop")
        .arg(title)
        .arg(body)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if spawned.is_err() {
        eprintln!("PitStop: {title} — {body}");
    }
}
