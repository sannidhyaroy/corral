//! The off page's live parts: system gauges, the presets, and the "Open the gate" action.
//!
//! - `GET /corral/status`: GPU memory, RAM, temperatures, fan, and the presets from `models.ini`
//! - `POST /corral/arm`: runs `corral on`, only from Corral's own page

use std::fs;

use serde_json::{Value, json};

use crate::config::{self, config_dir};
use crate::system::Snapshot;

/// A header plain links and forms can't send; with the Origin check, it stops other sites from arming Corral.
pub const ARM_HEADER: &str = "x-corral";

/// `GET /corral/status`
pub fn status() -> String {
    let system = Snapshot::read();
    json!({
        "gpu_used": system.gpu_memory.map(|gpu| gpu.usage.used),
        "gpu_total": system.gpu_memory.map(|gpu| gpu.usage.total),
        "ram_used": system.ram.map(|ram| ram.used),
        "ram_total": system.ram.map(|ram| ram.total),
        "cpu_temp": system.cpu_temp,
        "gpu_temp": system.gpu_temp,
        "fan_rpm": system.fan_rpm,
        "models": presets(&fs::read_to_string(config_dir().join("models.ini")).unwrap_or_default()),
    })
    .to_string()
}

/// The presets in `models.ini` with their shown name (`alias`) and badges (`tags`); `[*]` holds defaults, not a preset.
fn presets(models_ini: &str) -> Value {
    config::sections(models_ini)
        .into_iter()
        .filter(|(id, _)| id != "*")
        .map(|(id, preset)| {
            json!({
                "id": id,
                "name": preset.get("alias").unwrap_or(&id),
                "tags": preset.list("tags"),
            })
        })
        .collect()
}

/// Whether a `POST /corral/arm` came from Corral's own page.
pub fn arm_allowed(origins: &[String], origin: Option<&str>, header: Option<&str>) -> bool {
    origin.is_some_and(|origin| origins.iter().any(|allowed| allowed == origin)) && header == Some("arm")
}

/// `POST /corral/arm`: runs this same program's `corral on` against the user's systemd. The web front runs from a
/// system unit as the user, so it points at the user's session explicitly.
pub fn arm() -> Result<(), String> {
    let uid = fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            let line = status.lines().find(|line| line.starts_with("Uid:"))?;
            line.split_whitespace().nth(1).map(str::to_owned)
        })
        .ok_or("can't tell which user this runs as")?;
    let runtime = format!("/run/user/{uid}");
    let program = std::env::current_exe().map_err(|error| format!("can't find corral itself: {error}"))?;
    let output = std::process::Command::new(program)
        .arg("on")
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={runtime}/bus"))
        .output()
        .map_err(|error| format!("couldn't run corral: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_with_names_and_tags() {
        let ini = "[*]\nctx-size = 8192\n\n[gemma]\nalias = Gemma 4\ntags = 128k, vision\n\n[plain]\nmodel = x\n";
        assert_eq!(
            presets(ini),
            json!([
                {"id": "gemma", "name": "Gemma 4", "tags": ["128k", "vision"]},
                {"id": "plain", "name": "plain", "tags": []},
            ])
        );
    }

    #[test]
    fn only_corrals_own_page_may_arm() {
        let origins = ["https://corral.example".to_owned()];
        assert!(arm_allowed(&origins, Some("https://corral.example"), Some("arm")));
        assert!(!arm_allowed(&origins, Some("https://evil.example"), Some("arm")));
        assert!(!arm_allowed(&origins, Some("https://corral.example"), None));
        assert!(!arm_allowed(&[], Some("https://corral.example"), Some("arm")));
    }
}
