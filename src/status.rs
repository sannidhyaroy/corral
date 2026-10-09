//! `corral status`: whether Corral is armed, what runs, which model is loaded, and how the machine is doing.

use std::fmt::Write;

use crate::config::{Config, GpuLimits};
use crate::look::{self, BOLD, DIM, GREEN, RED, RESET, YELLOW};
use serde_json::{Value, json};

use crate::router::{self, Model};
use crate::system::{GpuMemoryKind, Snapshot, Usage};
use crate::systemd;

/// Swap use below this is noise and isn't shown.
const SWAP_SHOWN_FROM: u64 = 100 << 20;
/// Temperatures at or above this get a warning sign.
const HOT_CELSIUS: i64 = 85;
/// RAM has no guard, so its bar warns at fixed shares of the total.
const RAM_WARN: f64 = 0.85;
const RAM_CRITICAL: f64 = 0.95;

/// The llama: ears, a face that shows Corral's state, legs. The text column starts after it.
const EARS: &str = "   ∩∩";
const LEGS: &str = "  ╯   ╰";
const ART_COLUMNS: usize = 11;
const ASLEEP: &str = "(-ᴗ-) zᶻ";
const AWAKE: &str = "(•ᴗ•)";
const BUSY: &str = "(•̀ᴗ•́)و";
const WORRIED: &str = "(•_•;)";

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
    pub gpu_limits: Option<GpuLimits>,
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
        let system = Snapshot::read();
        let gpu_limits = system.gpu_memory.map(|gpu| config.gpu_limits(gpu.usage.total));
        Self {
            armed,
            gate_port: config.gate_port(),
            router,
            guard_running,
            system,
            gpu_limits,
        }
    }

    /// The page for a terminal: the llama, bars and colors.
    pub fn render_fancy(&self) -> String {
        let mut out = String::new();
        let (face, message) = self.mood();
        let headline = if self.armed {
            format!("{BOLD}{GREEN}Corral · armed{RESET}")
        } else {
            format!("{DIM}Corral · off{RESET}")
        };
        let address = if self.armed {
            format!("{DIM}http://ai.localhost:{}{RESET}", self.gate_port)
        } else {
            String::new()
        };
        let _ = writeln!(out, "{}{headline}", look::pad(EARS, ART_COLUMNS));
        let _ = writeln!(out, "{}{message}", look::pad(&format!("  {face}"), ART_COLUMNS));
        if address.is_empty() {
            let _ = writeln!(out, "{LEGS}");
        } else {
            let _ = writeln!(out, "{}{address}", look::pad(LEGS, ART_COLUMNS));
        }
        if !self.guard_running && matches!(self.router, Router::Running(_) | Router::Starting) {
            let _ = writeln!(
                out,
                "\n  {RED}The guard isn't running ⚠{RESET}  journalctl --user -u {}",
                systemd::GUARD
            );
        }
        out.push('\n');
        out.push_str(&self.render_system_fancy());
        out
    }

    /// The llama's face and what it says, from the router's state and how close GPU memory is to the guard's limit.
    fn mood(&self) -> (&'static str, String) {
        let (face, message) = match &self.router {
            Router::Off if !self.armed => (ASLEEP, "the herd is asleep · corral on opens the gate".to_owned()),
            Router::Off | Router::Waiting => (AWAKE, "waiting at the gate · no model loaded".to_owned()),
            Router::Starting => (AWAKE, "waking up…".to_owned()),
            Router::Running(models) if models.is_empty() => (AWAKE, "awake · no model loaded".to_owned()),
            Router::Running(models) => {
                let lines: Vec<String> = models
                    .iter()
                    .map(|model| match model.state.as_str() {
                        "loaded" => format!("{BOLD}{}{RESET} loaded", model.name()),
                        "loading" => format!("loading {BOLD}{}{RESET}…", model.name()),
                        state => format!("{BOLD}{}{RESET} {state}", model.name()),
                    })
                    .collect();
                let face = match models[0].state.as_str() {
                    "loaded" => BUSY,
                    "sleeping" => ASLEEP,
                    _ => AWAKE,
                };
                (face, lines.join(" · "))
            }
        };
        let near_limit = matches!(
            (self.system.gpu_memory, self.gpu_limits),
            (Some(gpu), Some(limits)) if gpu.usage.used >= limits.warn
        );
        if near_limit && matches!(self.router, Router::Running(_)) {
            (WORRIED, message)
        } else {
            (face, message)
        }
    }

    fn render_system_fancy(&self) -> String {
        let system = &self.system;
        let mut out = String::new();
        if let (Some(gpu), Some(limits)) = (system.gpu_memory, self.gpu_limits) {
            let color = if gpu.usage.used >= limits.unload {
                RED
            } else if gpu.usage.used >= limits.warn {
                YELLOW
            } else {
                GREEN
            };
            let _ = writeln!(
                out,
                "  {:<7}{}  {}",
                "GPU",
                look::bar(fraction(gpu.usage), color),
                amount(gpu.usage)
            );
        }
        if let Some(ram) = system.ram {
            let share = fraction(ram);
            let color = if share >= RAM_CRITICAL {
                RED
            } else if share >= RAM_WARN {
                YELLOW
            } else {
                GREEN
            };
            let swap = system
                .swap
                .filter(|swap| swap.used > SWAP_SHOWN_FROM)
                .map(|swap| format!("  {DIM}· swap {} GiB{RESET}", gib(swap.used)))
                .unwrap_or_default();
            let _ = writeln!(out, "  {:<7}{}  {}{swap}", "RAM", look::bar(share, color), amount(ram));
        }

        let hot = |millidegrees: i64| {
            let text = celsius(millidegrees);
            if millidegrees / 1000 >= HOT_CELSIUS {
                format!("{YELLOW}{text}{RESET}")
            } else {
                text
            }
        };
        let mut readings = Vec::new();
        if let Some(cpu) = system.cpu_temp {
            readings.push(format!("CPU {}", hot(cpu)));
        }
        if let Some(gpu) = system.gpu_temp {
            readings.push(format!("GPU {}", hot(gpu)));
        }
        if let Some(fan) = system.fan_rpm {
            readings.push(format!("fan {} rpm", thousands(fan)));
        }
        if !readings.is_empty() {
            let _ = writeln!(out, "  {:<7}{}", "Temp", readings.join(&format!(" {DIM}·{RESET} ")));
        }

        if let Some(on_charger) = system.on_charger {
            let mut power = vec![if on_charger { "charger" } else { "battery" }.to_owned()];
            if let Some(profile) = &system.power_profile {
                power.push(profile.clone());
            }
            if let Some(microwatts) = system.gpu_power {
                power.push(format!("chip {} W", microwatts / 1_000_000));
            }
            let _ = writeln!(out, "  {:<7}{}", "Power", power.join(&format!(" {DIM}·{RESET} ")));
        }
        out
    }

    /// The status for scripts and status bars: sizes in bytes, temperatures in °C, power in watts.
    pub fn to_json(&self) -> Value {
        let (router, models) = match &self.router {
            Router::Off => ("off", &[][..]),
            Router::Waiting => ("waiting", &[][..]),
            Router::Starting => ("starting", &[][..]),
            Router::Running(models) => ("running", models.as_slice()),
        };
        let models: Vec<Value> = models
            .iter()
            .map(|model| json!({"id": model.id, "name": model.name(), "state": model.state}))
            .collect();
        let usage = |usage: Option<Usage>| usage.map(|u| json!({"used": u.used, "total": u.total}));
        let system = &self.system;
        let gpu_memory = system.gpu_memory.map(|gpu| {
            json!({
                "kind": match gpu.kind {
                    GpuMemoryKind::Gtt => "gtt",
                    GpuMemoryKind::Vram => "vram",
                },
                "used": gpu.usage.used,
                "total": gpu.usage.total,
                "warn": self.gpu_limits.map(|limits| limits.warn),
                "unload": self.gpu_limits.map(|limits| limits.unload),
            })
        });
        #[allow(clippy::cast_precision_loss)] // sensor readings are small
        let celsius = |millidegrees: Option<i64>| millidegrees.map(|value| value as f64 / 1000.0);
        #[allow(clippy::cast_precision_loss)] // sensor readings are small
        let watts = system.gpu_power.map(|microwatts| microwatts as f64 / 1_000_000.0);
        json!({
            "armed": self.armed,
            "gate": format!("http://ai.localhost:{}", self.gate_port),
            "router": {"state": router, "models": models},
            "guard": if self.guard_running { "running" } else { "stopped" },
            "gpu_memory": gpu_memory,
            "ram": usage(system.ram),
            "swap": usage(system.swap),
            "temperature": {"cpu": celsius(system.cpu_temp), "gpu": celsius(system.gpu_temp)},
            "fan_rpm": system.fan_rpm,
            "power": {
                "on_charger": system.on_charger,
                "profile": system.power_profile,
                "gpu_watts": watts,
            },
        })
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

/// `used / total GiB`, right-aligned so numbers line up under each other.
fn amount(usage: Usage) -> String {
    format!("{:>4} / {} GiB", gib(usage.used), gib(usage.total))
}

#[allow(clippy::cast_precision_loss)] // memory sizes are far below f64's exact range
fn fraction(usage: Usage) -> f64 {
    if usage.total == 0 {
        0.0
    } else {
        usage.used as f64 / usage.total as f64
    }
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
            gpu_limits: None,
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
            gpu_limits: None,
        };
        assert!(!status.render_plain().contains("Guard"));
        status.router = Router::Running(Vec::new());
        assert!(status.render_plain().contains("Guard    not running ⚠"));
    }

    #[test]
    fn the_llama_shows_the_state() {
        let mut status = Status {
            armed: false,
            gate_port: 24937,
            router: Router::Off,
            guard_running: false,
            system: machine(),
            gpu_limits: Some(GpuLimits {
                warn: 20 * GIB,
                unload: 22 * GIB,
            }),
        };
        assert_eq!(status.mood().0, ASLEEP);
        status.armed = true;
        status.router = Router::Waiting;
        assert_eq!(status.mood().0, AWAKE);
        status.router = Router::Running(vec![Model {
            id: "a".to_owned(),
            alias: None,
            state: "loaded".to_owned(),
        }]);
        assert_eq!(status.mood().0, BUSY);
        status.system.gpu_memory = status.system.gpu_memory.map(|mut gpu| {
            gpu.usage.used = 21 * GIB;
            gpu
        });
        assert_eq!(status.mood().0, WORRIED);
    }

    #[test]
    fn thousands_separator() {
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(2100), "2,100");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn json_for_scripts() {
        let status = Status {
            armed: true,
            gate_port: 24937,
            router: Router::Waiting,
            guard_running: false,
            system: machine(),
            gpu_limits: Some(GpuLimits {
                warn: 20 * GIB,
                unload: 22 * GIB,
            }),
        };
        let json = status.to_json();
        assert_eq!(json["router"]["state"], "waiting");
        assert_eq!(json["gpu_memory"]["kind"], "gtt");
        assert_eq!(json["gpu_memory"]["warn"], 20 * GIB);
        assert_eq!(json["temperature"]["gpu"], 91.0);
        assert_eq!(json["power"]["profile"], "balanced");
    }
}
