//! Corral's settings file, `corral.conf`: one `key = value` per line, `#` starts a comment. When a key appears more
//! than once, the last one counts.

use std::collections::HashMap;
use std::net::IpAddr;
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

/// The `[name]` sections of an INI file such as `helpers.ini` or `models.ini`, each read like `corral.conf`. Lines
/// before the first section are ignored.
pub fn sections(text: &str) -> Vec<(String, Config)> {
    let mut sections: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let trimmed = line.split_once('#').map_or(line, |(before, _)| before).trim();
        if let Some(name) = trimmed.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            sections.push((name.trim().to_owned(), String::new()));
        } else if let Some((_, body)) = sections.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    sections
        .into_iter()
        .map(|(name, body)| (name, Config::parse(&body)))
        .collect()
}

/// The gate's port: 24937 is `0x61 0x69`, the bytes of "ai".
pub const DEFAULT_GATE_PORT: u16 = 24937;
/// llama.cpp's router, reachable only from this machine.
pub const DEFAULT_ROUTER_PORT: u16 = 18080;
/// The address the custom name points to: `127.0.'a'.'i'`, leaving 127.0.0.1's ports 80 and 443 to other software.
pub const DEFAULT_NAME_ADDRESS: &str = "127.0.97.105";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuLimits {
    pub warn: u64,
    pub unload: u64,
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // positive GiB values, far below u64::MAX
fn gib_to_bytes(gib: f64) -> u64 {
    (gib * f64::from(1 << 30)) as u64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameListen {
    /// Only this machine, on the name's own address.
    Local,
    /// Every address; other devices are let in only while Corral is armed.
    Armed,
    /// Every address, always.
    All,
}

/// An address or a range in CIDR notation, such as `100.64.0.0/10` or `fd7a:115c:a1e0::/48`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressRange {
    address: IpAddr,
    prefix: u8,
}

impl AddressRange {
    pub fn parse(text: &str) -> Option<Self> {
        let (address, prefix) = text.split_once('/').map_or((text, None), |(a, p)| (a, Some(p)));
        let address: IpAddr = address.trim().parse().ok()?;
        let longest = if address.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix.trim().parse().ok().filter(|&p| p <= longest)?,
            None => longest,
        };
        Some(Self { address, prefix })
    }

    /// Whether `ip` is in the range. IPv4 clients of a dual-stack socket arrive as `::ffff:a.b.c.d` and count as
    /// their IPv4 address.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.address, ip.to_canonical()) {
            (IpAddr::V4(range), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - u32::from(self.prefix)).unwrap_or(0);
                u32::from(range) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(range), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - u32::from(self.prefix)).unwrap_or(0);
                u128::from(range) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
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

    /// The web front's own HTTPS name (`name`), such as `corral.example`; none unless set.
    pub fn name(&self) -> Option<&str> {
        self.get("name")
    }

    /// Where the web UI is: the custom name over HTTPS, or the gate.
    pub fn web_address(&self) -> String {
        self.name().map_or_else(
            || format!("http://ai.localhost:{}", self.gate_port()),
            |name| format!("https://{name}"),
        )
    }

    /// The local address the custom name points to (`name_address`).
    pub fn name_address(&self) -> &str {
        self.get("name_address").unwrap_or(DEFAULT_NAME_ADDRESS)
    }

    /// Whether the gate listens on every address (`listen = all`) instead of only this machine's (`local`, the
    /// default). Other devices then need an API key.
    pub fn listen_on_all(&self) -> bool {
        self.get("listen") == Some("all")
    }

    /// Who reaches the custom name's ports 80 and 443 (`name_listen`): only this machine (`local`, the default),
    /// other devices too while Corral is armed (`armed`), or always (`all`).
    pub fn name_listen(&self) -> NameListen {
        match self.get("name_listen") {
            Some("armed") => NameListen::Armed,
            Some("all") => NameListen::All,
            _ => NameListen::Local,
        }
    }

    /// How many models the router keeps loaded at once (`models_max`, default 1).
    pub fn models_max(&self) -> u32 {
        self.get("models_max")
            .and_then(|value| value.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(1)
    }

    /// Address ranges treated like this machine itself (`trusted`): no key needed, the router's tools allowed. Safe
    /// for Tailscale's `100.64.0.0/10`, whose addresses belong to each device's key; not for a home Wi-Fi, where any
    /// device that joins can get an address. Entries that aren't addresses or ranges are skipped.
    pub fn trusted(&self) -> Vec<AddressRange> {
        self.list("trusted")
            .iter()
            .filter_map(|entry| AddressRange::parse(entry))
            .collect()
    }

    /// Extra llama-server options for the router (`router_options`), passed through as written.
    pub fn router_options(&self) -> Vec<String> {
        self.get("router_options")
            .map(|options| options.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default()
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

    #[test]
    fn trusted_ranges() {
        let config = Config::parse("trusted = 100.64.0.0/10, 192.168.1.7, fd7a:115c:a1e0::/48, not-an-address");
        let ranges = config.trusted();
        assert_eq!(ranges.len(), 3);
        let trusted = |ip: &str| ranges.iter().any(|range| range.contains(ip.parse().unwrap()));
        assert!(trusted("100.64.209.31"));
        assert!(trusted("::ffff:100.101.2.3"));
        assert!(!trusted("100.128.0.1"));
        assert!(trusted("192.168.1.7"));
        assert!(!trusted("192.168.1.8"));
        assert!(trusted("fd7a:115c:a1e0::1234"));
        assert!(Config::parse("trusted = 0.0.0.0/0").trusted()[0].contains("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn who_reaches_the_custom_name() {
        assert_eq!(Config::parse("").name_listen(), NameListen::Local);
        assert_eq!(Config::parse("name_listen = armed").name_listen(), NameListen::Armed);
        assert_eq!(Config::parse("name_listen = all").name_listen(), NameListen::All);
        assert_eq!(Config::parse("name_listen = everyone").name_listen(), NameListen::Local);
    }
}
