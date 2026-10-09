//! Corral's settings file, `corral.conf`: one `key = value` per line, `#` starts a comment. When a key appears more
//! than once, the last one counts.

use std::collections::HashMap;
use std::path::PathBuf;
use std::{env, fs};

/// Where Corral keeps its settings: `$XDG_CONFIG_HOME/corral`, or `~/.config/corral`.
pub fn config_dir() -> PathBuf {
    let base = env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_default();
    base.join("corral")
}

/// The gate's port: 24937 is `0x61 0x69`, the bytes of "ai".
pub const DEFAULT_GATE_PORT: u16 = 24937;
/// llama.cpp's router, reachable only from this machine.
pub const DEFAULT_ROUTER_PORT: u16 = 18080;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuLimits {
    pub warn: u64,
    pub unload: u64,
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // positive GiB values, far below u64::MAX
fn gib_to_bytes(gib: f64) -> u64 {
    (gib * f64::from(1 << 30)) as u64
}

#[derive(Debug, Default)]
pub struct Config {
    values: HashMap<String, String>,
}

impl Config {
    /// Reads `corral.conf`. A missing or unreadable file means "all defaults".
    pub fn load() -> Self {
        fs::read_to_string(config_dir().join("corral.conf")).map_or_else(|_| Self::default(), |text| Self::parse(&text))
    }

    pub fn parse(text: &str) -> Self {
        let values = text
            .lines()
            .map(|line| line.split_once('#').map_or(line, |(before, _)| before))
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
            .filter(|(key, value)| !key.is_empty() && !value.is_empty())
            .collect();
        Self { values }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// Every key and its value, sorted by key.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        let mut entries: Vec<(&str, &str)> = self.values.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        entries.sort_unstable();
        entries.into_iter()
    }

    /// `yes`/`no` (also `true`/`false`, `on`/`off`, `1`/`0`); anything else gives the default.
    pub fn flag(&self, key: &str, default: bool) -> bool {
        match self.get(key).map(str::to_ascii_lowercase).as_deref() {
            Some("yes" | "true" | "on" | "1") => true,
            Some("no" | "false" | "off" | "0") => false,
            _ => default,
        }
    }

    pub fn number(&self, key: &str) -> Option<f64> {
        self.get(key)?.parse().ok()
    }

    /// A comma-separated list; items may contain spaces.
    pub fn list(&self, key: &str) -> Vec<String> {
        self.get(key)
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Where apps, browsers and other devices reach Corral (`gate_port`).
    pub fn gate_port(&self) -> u16 {
        self.port("gate_port", DEFAULT_GATE_PORT)
    }

    /// Where llama.cpp's router listens on this machine (`router_port`).
    pub fn router_port(&self) -> u16 {
        self.port("router_port", DEFAULT_ROUTER_PORT)
    }

    /// The guard's limits for GPU memory in bytes: a warning at `gpu_warn`, unloading the model at `gpu_unload`
    /// (both in GiB). Without settings: 90% and 95% of the GPU's `total`.
    pub fn gpu_limits(&self, total: u64) -> GpuLimits {
        let setting = |key: &str, default_percent: u64| {
            self.number(key)
                .filter(|gib| *gib > 0.0)
                .map_or(total / 100 * default_percent, gib_to_bytes)
        };
        GpuLimits {
            warn: setting("gpu_warn", 90),
            unload: setting("gpu_unload", 95),
        }
    }

    fn port(&self, key: &str, default: u16) -> u16 {
        self.get(key)
            .and_then(|value| value.parse().ok())
            .filter(|&port| port != 0)
            .unwrap_or(default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# A comment line
sweep = no   # a trailing comment
exclusive = Gemma 4-26b-a4b-256k, Gemma 4-26b-a4b-pantheon ,
gpu_warn = 22.4
gpu_warn = 21.0
broken line without equals
empty =
";

    #[test]
    fn reads_values_and_ignores_comments() {
        let config = Config::parse(SAMPLE);
        assert_eq!(config.get("sweep"), Some("no"));
        assert_eq!(config.get("empty"), None);
        assert_eq!(config.get("broken line without equals"), None);
    }

    #[test]
    fn last_value_wins() {
        assert_eq!(Config::parse(SAMPLE).number("gpu_warn"), Some(21.0));
    }

    #[test]
    fn flags_fall_back_to_the_default() {
        let config = Config::parse("a = yes\nb = Off\nc = maybe");
        assert!(config.flag("a", false));
        assert!(!config.flag("b", true));
        assert!(config.flag("c", true));
        assert!(!config.flag("missing", false));
    }

    #[test]
    fn lists_keep_spaces_inside_items() {
        assert_eq!(
            Config::parse(SAMPLE).list("exclusive"),
            ["Gemma 4-26b-a4b-256k", "Gemma 4-26b-a4b-pantheon"]
        );
        assert!(Config::parse("").list("exclusive").is_empty());
    }

    #[test]
    fn ports_default_unless_set_to_a_valid_port() {
        let config = Config::parse("router_port = 19090\ngate_port = 0");
        assert_eq!(config.router_port(), 19090);
        assert_eq!(config.gate_port(), DEFAULT_GATE_PORT);
        assert_eq!(Config::parse("gate_port = 70000").gate_port(), DEFAULT_GATE_PORT);
    }

    #[test]
    fn gpu_limits_from_settings_or_percentages() {
        const GIB: u64 = 1 << 30;
        let limits = Config::parse("gpu_warn = 22.5\ngpu_unload = 23").gpu_limits(24 * GIB);
        assert_eq!(
            limits,
            GpuLimits {
                warn: 22 * GIB + GIB / 2,
                unload: 23 * GIB
            }
        );
        let defaults = Config::parse("").gpu_limits(100 * GIB);
        assert_eq!(
            defaults,
            GpuLimits {
                warn: 90 * GIB,
                unload: 95 * GIB
            }
        );
    }
}
