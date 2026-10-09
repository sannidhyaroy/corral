//! `corral status`: whether Corral is armed, what runs, which model is loaded, and how the machine is doing.

use std::fmt::Write;

use crate::config::Config;
use crate::router::{self, Model};
use crate::system::{Snapshot, Usage};
use crate::systemd;

/// Swap use below this is noise and isn't shown.
const SWAP_SHOWN_FROM: u64 = 100 << 20;
/// Temperatures at or above this get a warning sign.
const HOT_CELSIUS: i64 = 85;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Router {
    /// Corral is off.
    Off,
    /// Armed; the router starts on the first request.
    Waiting,
    /// Running but not answering yet.
    Starting,
    Running(Vec<Model>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub armed: bool,
    pub gate_port: u16,
    pub router: Router,
    pub guard_running: bool,
    pub system: Snapshot,
}

impl Status {
    pub fn gather(config: &Config) -> Self {
        let units = [systemd::TARGET, systemd::ROUTER_SOCKET, systemd::ROUTER, systemd::GUARD];
        let [armed, socket, running, guard_running] = systemd::active(&units)[..] else {
            unreachable!("systemd::active returns one state per unit")
        };
        let router = if running {
            // The router's own port: asking through the gate would start a stopped router
            router::active_models(config.router_port()).map_or(Router::Starting, Router::Running)
        } else if socket {
            Router::Waiting
        } else {
            Router::Off
        };
        Self {
            armed,
            gate_port: config.gate_port(),
            router,
            guard_running,
            system: Snapshot::read(),
        }
    }

    /// The plain-text page: no colors, no art, stable for scripts and pipes.
    pub fn render_plain(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Corral: {}", if self.armed { "armed" } else { "off" });

        let router = match &self.router {
            Router::Off => "off".to_owned(),
            Router::Waiting => "waiting for a request".to_owned(),
            Router::Starting => "running · starting".to_owned(),
            Router::Running(models) if models.is_empty() => "running · no model loaded".to_owned(),
            Router::Running(models) => {
                let models: Vec<String> = models.iter().map(|m| format!("{} {}", m.name(), m.state)).collect();
                format!("running · {}", models.join(", "))
            }
        };
        let address = format!("ai.localhost:{}", self.gate_port);
        let _ = writeln!(out, "  {:<8} {address:<22} {router}", "Models");
        if self.guard_running {
            let _ = writeln!(out, "  {:<8} watching", "Guard");
        } else if matches!(self.router, Router::Running(_) | Router::Starting) {
            let _ = writeln!(
                out,
                "  {:<8} not running ⚠ (journalctl --user -u {})",
                "Guard",
                systemd::GUARD
            );
        }

        out.push('\n');
        out.push_str(&render_system(&self.system));
        out
    }
}

fn render_system(system: &Snapshot) -> String {
    let mut out = String::new();
    if let Some(gpu) = system.gpu_memory {
        let _ = writeln!(out, "  {:<12} {}", "GPU memory", usage(gpu.usage));
    }
    if let Some(ram) = system.ram {
        let swap = system
            .swap
            .filter(|swap| swap.used > SWAP_SHOWN_FROM)
            .map(|swap| format!(" · swap {} GiB in use", gib(swap.used)))
            .unwrap_or_default();
        let _ = writeln!(out, "  {:<12} {}{swap}", "RAM", usage(ram));
    }

    let mut readings = Vec::new();
    if let Some(cpu) = system.cpu_temp {
        readings.push(format!("CPU {}", celsius(cpu)));
    }
    if let Some(gpu) = system.gpu_temp {
        readings.push(format!("GPU {}", celsius(gpu)));
    }
    if let Some(fan) = system.fan_rpm {
        readings.push(format!("fan {} rpm", thousands(fan)));
    }
    if !readings.is_empty() {
        let _ = writeln!(out, "  {:<12} {}", "Temperature", readings.join(" · "));
    }

    if let Some(on_charger) = system.on_charger {
        let mut power = vec![if on_charger { "charger" } else { "battery" }.to_owned()];
        if let Some(profile) = &system.power_profile {
            power.push(format!("{profile} profile"));
        }
        if let Some(microwatts) = system.gpu_power {
            power.push(format!("chip drawing {} W", microwatts / 1_000_000));
        }
        let _ = writeln!(out, "  {:<12} {}", "Power", power.join(" · "));
    }
    out
}

fn usage(usage: Usage) -> String {
    let percent = (usage.used * 100).checked_div(usage.total).unwrap_or(0);
    format!("{} of {} GiB  ({percent}%)", gib(usage.used), gib(usage.total))
}

#[allow(clippy::cast_precision_loss)] // memory sizes are far below f64's exact range
fn gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / f64::from(1 << 30))
}

fn celsius(millidegrees: i64) -> String {
    let degrees = millidegrees / 1000;
    if degrees >= HOT_CELSIUS {
        format!("{degrees} °C ⚠")
    } else {
        format!("{degrees} °C")
    }
}

fn thousands(number: u64) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::{GpuMemory, GpuMemoryKind};

    const GIB: u64 = 1 << 30;

    fn machine() -> Snapshot {
        Snapshot {
            gpu_memory: Some(GpuMemory {
                kind: GpuMemoryKind::Gtt,
                usage: Usage {
                    used: 2 * GIB,
                    total: 23 * GIB,
                },
            }),
            ram: Some(Usage {
                used: 11 * GIB,
                total: 30 * GIB,
            }),
            swap: Some(Usage {
                used: GIB / 2,
                total: 8 * GIB,
            }),
            cpu_temp: Some(52_400),
            gpu_temp: Some(91_000),
            fan_rpm: Some(2100),
            gpu_power: Some(9_300_000),
            on_charger: Some(true),
            power_profile: Some("balanced".to_owned()),
        }
    }

    #[test]
    fn armed_with_a_loaded_model() {
        let status = Status {
            armed: true,
            gate_port: 24937,
            router: Router::Running(vec![Model {
                id: "gemma-32k".to_owned(),
                alias: Some("Gemma 4 · Gallop".to_owned()),
                state: "loaded".to_owned(),
            }]),
            guard_running: true,
            system: machine(),
        };
        assert_eq!(
            status.render_plain(),
            "Corral: armed
  Models   ai.localhost:24937     running · Gemma 4 · Gallop loaded
  Guard    watching

  GPU memory   2.0 of 23.0 GiB  (8%)
  RAM          11.0 of 30.0 GiB  (36%) · swap 0.5 GiB in use
  Temperature  CPU 52 °C · GPU 91 °C ⚠ · fan 2,100 rpm
  Power        charger · balanced profile · chip drawing 9 W
"
        );
    }

    #[test]
    fn guard_warning_only_while_the_router_runs() {
        let mut status = Status {
            armed: true,
            gate_port: 24937,
            router: Router::Waiting,
            guard_running: false,
            system: Snapshot::default(),
        };
        assert!(!status.render_plain().contains("Guard"));
        status.router = Router::Running(Vec::new());
        assert!(status.render_plain().contains("Guard    not running ⚠"));
    }

    #[test]
    fn thousands_separator() {
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(2100), "2,100");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
