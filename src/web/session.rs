//! The user's systemd, reached from the web front. With a custom name the web front runs as a system service on the
//! user's behalf, outside the user's session, so it points `systemctl --user` at the session explicitly.

use std::fs;
use std::process::Command;

use crate::systemd;

/// The user's session: its runtime folder and message bus.
pub fn environment() -> Result<[(String, String); 2], String> {
    let uid = fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            let line = status.lines().find(|line| line.starts_with("Uid:"))?;
            line.split_whitespace().nth(1).map(str::to_owned)
        })
        .ok_or("can't tell which user this runs as")?;
    let runtime = format!("/run/user/{uid}");
    Ok([
        ("XDG_RUNTIME_DIR".to_owned(), runtime.clone()),
        (
            "DBUS_SESSION_BUS_ADDRESS".to_owned(),
            format!("unix:path={runtime}/bus"),
        ),
    ])
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .envs(environment()?)
        .output()
        .map_err(|error| format!("couldn't run systemctl: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

/// Whether Corral is armed (`corral on`).
pub fn is_armed() -> bool {
    systemctl(&["is-active", "--quiet", systemd::TARGET]).is_ok()
}

/// Starts the router and returns once it answers (its unit waits for that).
pub fn start_router() -> Result<(), String> {
    systemctl(&["start", systemd::ROUTER])
}

/// Stops the router; the guard goes with it.
pub fn stop_router() -> Result<(), String> {
    systemctl(&["stop", systemd::ROUTER])
}
