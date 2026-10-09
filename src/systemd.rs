//! Corral's units in the user's systemd instance.

use std::process::Command;

pub const TARGET: &str = "corral.target";
pub const ROUTER_SOCKET: &str = "corral-router.socket";
pub const ROUTER: &str = "corral-router.service";
pub const GUARD: &str = "corral-guard.service";

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
