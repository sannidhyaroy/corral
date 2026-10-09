//! What the machine reports about itself: GPU memory, RAM, temperatures, fan, power. Everything is detected from
//! `/sys` and `/proc`; a missing sensor is simply absent.

use std::fs;
use std::path::{Path, PathBuf};

/// Below this much dedicated memory, a GPU is integrated and runs models in shared system memory (GTT).
const INTEGRATED_VRAM_LIMIT: u64 = 2 << 30;

const CPU_SENSORS: &[&str] = &["k10temp", "zenpower", "coretemp", "cpu_thermal"];
const GPU_SENSORS: &[&str] = &["amdgpu", "radeon", "nouveau"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub used: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuMemoryKind {
    /// Dedicated memory on a graphics card.
    Vram,
    /// System memory the GPU may map (integrated GPUs).
    Gtt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuMemory {
    pub kind: GpuMemoryKind,
    pub usage: Usage,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub gpu_memory: Option<GpuMemory>,
    pub ram: Option<Usage>,
    pub swap: Option<Usage>,
    /// Millidegrees Celsius.
    pub cpu_temp: Option<i64>,
    /// Millidegrees Celsius.
    pub gpu_temp: Option<i64>,
    pub fan_rpm: Option<u64>,
    /// Microwatts.
    pub gpu_power: Option<u64>,
    /// `None` on machines without a mains adapter (desktops) or when unknown.
    pub on_charger: Option<bool>,
    pub power_profile: Option<String>,
}

impl Snapshot {
    pub fn read() -> Self {
        Self::read_from(Path::new("/"))
    }

    /// Reads under `root` instead of `/`, for tests.
    pub fn read_from(root: &Path) -> Self {
        let (ram, swap) = memory(root);
        Self {
            gpu_memory: gpu_memory(root),
            ram,
            swap,
            cpu_temp: sensor(root, "temp1_input", CPU_SENSORS),
            gpu_temp: sensor(root, "temp1_input", GPU_SENSORS),
            fan_rpm: sensor(root, "fan1_input", &[]),
            gpu_power: sensor(root, "power1_input", &["amdgpu"])
                .or_else(|| sensor(root, "power1_average", &["amdgpu"])),
            on_charger: on_charger(root),
            power_profile: read_trimmed(&root.join("sys/firmware/acpi/platform_profile"))
                .or_else(|| read_trimmed(&root.join("etc/tuned/active_profile"))),
        }
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn read_number<T: std::str::FromStr>(path: &Path) -> Option<T> {
    read_trimmed(path)?.parse().ok()
}

/// Entries of a directory whose names start with `prefix`, sorted so the result doesn't depend on the filesystem.
fn entries(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
        .map(|entry| entry.path())
        .collect();
    found.sort();
    found
}

/// The first GPU that reports its memory.
fn gpu_memory(root: &Path) -> Option<GpuMemory> {
    entries(&root.join("sys/class/drm"), "card")
        .into_iter()
        .filter(|card| {
            card.file_name()
                .is_some_and(|name| !name.to_string_lossy().contains('-'))
        })
        .find_map(|card| {
            let device = card.join("device");
            let vram_total: u64 = read_number(&device.join("mem_info_vram_total"))?;
            let (kind, prefix) = if vram_total < INTEGRATED_VRAM_LIMIT {
                (GpuMemoryKind::Gtt, "mem_info_gtt")
            } else {
                (GpuMemoryKind::Vram, "mem_info_vram")
            };
            let usage = Usage {
                used: read_number(&device.join(format!("{prefix}_used")))?,
                total: read_number(&device.join(format!("{prefix}_total")))?,
            };
            Some(GpuMemory { kind, usage })
        })
}

/// RAM in use (total minus available) and swap in use, from `/proc/meminfo`.
fn memory(root: &Path) -> (Option<Usage>, Option<Usage>) {
    let Ok(meminfo) = fs::read_to_string(root.join("proc/meminfo")) else {
        return (None, None);
    };
    let field = |name: &str| -> Option<u64> {
        meminfo.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?.strip_prefix(':')?;
            let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
            Some(kib * 1024)
        })
    };
    let usage = |total: Option<u64>, free: Option<u64>| {
        let (total, free) = (total?, free?);
        Some(Usage {
            used: total.saturating_sub(free),
            total,
        })
    };
    (
        usage(field("MemTotal"), field("MemAvailable")),
        usage(field("SwapTotal"), field("SwapFree")),
    )
}

/// A hwmon reading from the first driver in `drivers` (in order of preference) that has the file; an empty list
/// accepts any driver.
fn sensor<T: std::str::FromStr>(root: &Path, file: &str, drivers: &[&str]) -> Option<T> {
    let sensors = entries(&root.join("sys/class/hwmon"), "hwmon");
    let has_file = |dir: &&PathBuf| dir.join(file).is_file();
    let found = if drivers.is_empty() {
        sensors.iter().find(has_file)
    } else {
        drivers.iter().find_map(|driver| {
            sensors
                .iter()
                .filter(has_file)
                .find(|dir| read_trimmed(&dir.join("name")).as_deref() == Some(driver))
        })
    };
    read_number(&found?.join(file))
}

/// Whether a mains adapter (of any name: AC, ACAD, ADP1…) is connected.
fn on_charger(root: &Path) -> Option<bool> {
    entries(&root.join("sys/class/power_supply"), "")
        .into_iter()
        .find(|supply| read_trimmed(&supply.join("type")).as_deref() == Some("Mains"))
        .and_then(|supply| read_number::<u8>(&supply.join("online")))
        .map(|online| online == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway folder that looks like `/` with only the given files.
    struct FakeRoot(PathBuf);

    impl FakeRoot {
        fn new(name: &str, files: &[(&str, &str)]) -> Self {
            let root = std::env::temp_dir().join(format!("corral-test-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            for (path, content) in files {
                let path = root.join(path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, content).unwrap();
            }
            Self(root)
        }
    }

    impl Drop for FakeRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const GIB: u64 = 1 << 30;

    #[test]
    fn integrated_gpu_reports_shared_memory() {
        let root = FakeRoot::new(
            "igpu",
            &[
                ("sys/class/drm/card1/device/mem_info_vram_total", "536870912"),
                ("sys/class/drm/card1/device/mem_info_gtt_total", &(23 * GIB).to_string()),
                ("sys/class/drm/card1/device/mem_info_gtt_used", &(5 * GIB).to_string()),
                ("sys/class/drm/card1-eDP-1/status", "connected"),
            ],
        );
        let gpu = Snapshot::read_from(&root.0).gpu_memory.unwrap();
        assert_eq!(gpu.kind, GpuMemoryKind::Gtt);
        assert_eq!(
            gpu.usage,
            Usage {
                used: 5 * GIB,
                total: 23 * GIB
            }
        );
    }

    #[test]
    fn graphics_card_reports_its_own_memory() {
        let root = FakeRoot::new(
            "dgpu",
            &[
                (
                    "sys/class/drm/card0/device/mem_info_vram_total",
                    &(16 * GIB).to_string(),
                ),
                ("sys/class/drm/card0/device/mem_info_vram_used", &GIB.to_string()),
            ],
        );
        let gpu = Snapshot::read_from(&root.0).gpu_memory.unwrap();
        assert_eq!(gpu.kind, GpuMemoryKind::Vram);
        assert_eq!(gpu.usage.total, 16 * GIB);
    }

    #[test]
    fn memory_comes_from_meminfo() {
        let meminfo = "MemTotal: 1000 kB\nMemFree: 100 kB\nMemAvailable: 400 kB\nSwapTotal: 200 kB\nSwapFree: 200 kB\n";
        let root = FakeRoot::new("meminfo", &[("proc/meminfo", meminfo)]);
        let snapshot = Snapshot::read_from(&root.0);
        assert_eq!(
            snapshot.ram,
            Some(Usage {
                used: 600 * 1024,
                total: 1000 * 1024
            })
        );
        assert_eq!(
            snapshot.swap,
            Some(Usage {
                used: 0,
                total: 200 * 1024
            })
        );
    }

    #[test]
    fn sensors_follow_the_driver_preference() {
        let root = FakeRoot::new(
            "hwmon",
            &[
                ("sys/class/hwmon/hwmon0/name", "acpitz"),
                ("sys/class/hwmon/hwmon0/temp1_input", "40000"),
                ("sys/class/hwmon/hwmon1/name", "amdgpu"),
                ("sys/class/hwmon/hwmon1/temp1_input", "48000"),
                ("sys/class/hwmon/hwmon1/power1_input", "9000000"),
                ("sys/class/hwmon/hwmon2/name", "k10temp"),
                ("sys/class/hwmon/hwmon2/temp1_input", "62000"),
                ("sys/class/hwmon/hwmon3/name", "thinkpad"),
                ("sys/class/hwmon/hwmon3/fan1_input", "2400"),
            ],
        );
        let snapshot = Snapshot::read_from(&root.0);
        assert_eq!(snapshot.cpu_temp, Some(62_000));
        assert_eq!(snapshot.gpu_temp, Some(48_000));
        assert_eq!(snapshot.gpu_power, Some(9_000_000));
        assert_eq!(snapshot.fan_rpm, Some(2400));
    }

    #[test]
    fn charger_and_power_profile() {
        let root = FakeRoot::new(
            "power",
            &[
                ("sys/class/power_supply/BAT0/type", "Battery"),
                ("sys/class/power_supply/ACAD/type", "Mains"),
                ("sys/class/power_supply/ACAD/online", "1"),
                ("etc/tuned/active_profile", "balanced\n"),
            ],
        );
        let snapshot = Snapshot::read_from(&root.0);
        assert_eq!(snapshot.on_charger, Some(true));
        assert_eq!(snapshot.power_profile.as_deref(), Some("balanced"));
    }

    #[test]
    fn missing_sensors_are_absent() {
        let root = FakeRoot::new("empty", &[]);
        assert_eq!(Snapshot::read_from(&root.0), Snapshot::default());
    }
}
