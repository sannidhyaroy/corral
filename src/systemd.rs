//! Corral's units in the user's systemd instance.

use std::process::Command;

pub const TARGET: &str = "corral.target";
pub const ROUTER_SOCKET: &str = "corral-router.socket";
pub const ROUTER: &str = "corral-router.service";
pub const GUARD: &str = "corral-guard.service";

/// Starts units and waits until systemd has done so. On failure, systemd's own message.
pub fn start(units: &[&str]) -> Result<(), String> {
    systemctl("start", units)
}

/// Stops units and waits until their processes are gone. On failure, systemd's own message.
pub fn stop(units: &[&str]) -> Result<(), String> {
    systemctl("stop", units)
}

/// Every Corral unit installed in the user's systemd instance.
pub fn corral_units() -> Vec<String> {
    Command::new("systemctl")
        .args(["--user", "list-unit-files", "corral*", "--no-legend", "--plain"])
        .output()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.split_whitespace().next())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn systemctl(verb: &str, units: &[&str]) -> Result<(), String> {
    let output = Command::new("systemctl")
        .args(["--user", verb])
        .args(units)
        .output()
        .map_err(|error| format!("couldn't run systemctl: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

/// Whether each unit is active, in the order given. If systemd can't be asked, none is.
pub fn active(units: &[&str]) -> Vec<bool> {
    let output = Command::new("systemctl")
        .args(["--user", "is-active"])
        .args(units)
        .output();
    let mut states: Vec<bool> = output
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|line| line.trim() == "active")
                .collect()
        })
        .unwrap_or_default();
    states.resize(units.len(), false);
    states
}
