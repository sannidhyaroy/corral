//! API keys for other devices. A key looks like `sk-corral-` + 32 random letters and digits + a 6-character
//! checksum, so a typo or a cut-off key is rejected before any lookup, and secret scanners can recognise the format.
//!
//! Only each key's SHA-256 hash is stored, in `api-keys` in Corral's config folder (readable only by its owner), one
//! line per device: `<hash> <device> [allow=tools]`. A key is shown once, when it's made; revoking one removes its
//! line, and the web front notices the change on the next request.

use std::fmt::Write;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::config::config_dir;

const PREFIX: &str = "sk-corral-";
const RANDOM_LENGTH: usize = 32;
const CHECKSUM_LENGTH: usize = 6;
const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub device: String,
    hash: String,
    /// The router's built-in tools (`/tools`) are allowed with this key.
    pub allow_tools: bool,
}

pub fn path() -> PathBuf {
    config_dir().join("api-keys")
}

/// A new key from the system's random source.
pub fn generate() -> std::io::Result<String> {
    let mut random = String::with_capacity(RANDOM_LENGTH);
    let mut source = File::open("/dev/urandom")?;
    let mut byte = [0u8; 1];
    while random.len() < RANDOM_LENGTH {
        source.read_exact(&mut byte)?;
        // 248 is the largest multiple of 62 below 256; higher bytes would make some characters more likely
        if byte[0] < 248 {
            random.push(char::from(ALPHABET[usize::from(byte[0] % 62)]));
        }
    }
    let checksum = checksum(&random);
    Ok(format!("{PREFIX}{random}{checksum}"))
}

/// The CRC-32 of the random part, in 6 characters of the same alphabet.
fn checksum(random: &str) -> String {
    let mut crc = flate2::Crc::new();
    crc.update(random.as_bytes());
    let mut value = crc.sum();
    let mut out = [b'0'; CHECKSUM_LENGTH];
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[(value % 62) as usize];
        value /= 62;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether `key` has Corral's format and a matching checksum.
pub fn well_formed(key: &str) -> bool {
    let Some(rest) = key.strip_prefix(PREFIX) else {
        return false;
    };
    if rest.len() != RANDOM_LENGTH + CHECKSUM_LENGTH || !rest.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return false;
    }
    let (random, check) = rest.split_at(RANDOM_LENGTH);
    checksum(random) == check
}

fn hash(key: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, key.as_bytes());
    digest.as_ref().iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

pub fn parse(text: &str) -> Vec<Entry> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let (hash, device) = (words.next()?, words.next()?);
            Some(Entry {
                device: device.to_owned(),
                hash: hash.to_owned(),
                allow_tools: words.any(|word| word == "allow=tools"),
            })
        })
        .collect()
}

fn render(entries: &[Entry]) -> String {
    let mut text = String::from("# Corral's API keys: SHA-256 hash, device, permissions. Manage with `corral key`.\n");
    for entry in entries {
        let allow = if entry.allow_tools { " allow=tools" } else { "" };
        let _ = writeln!(text, "{} {}{allow}", entry.hash, entry.device);
    }
    text
}

pub fn load() -> Vec<Entry> {
    fs::read_to_string(path()).map(|text| parse(&text)).unwrap_or_default()
}

/// Writes the key file, readable and writable only by its owner.
fn save(entries: &[Entry]) -> Result<(), String> {
    let path = path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("couldn't create {}: {error}", parent.display()))?;
    }
    let temporary = path.with_extension("corral-new");
    fs::write(&temporary, render(entries))
        .map_err(|error| format!("couldn't write {}: {error}", temporary.display()))?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("couldn't protect {}: {error}", temporary.display()))?;
    fs::rename(&temporary, &path).map_err(|error| format!("couldn't save {}: {error}", path.display()))
}

/// Makes a key for `device` and returns it; it's never stored and can't be shown again.
pub fn add(device: &str, allow_tools: bool) -> Result<String, String> {
    check_name(device)?;
    let mut entries = load();
    if entries.iter().any(|entry| entry.device == device) {
        return Err(format!(
            "`{device}` already has a key; revoke it first to make a new one"
        ));
    }
    let key = generate().map_err(|error| format!("couldn't make a random key: {error}"))?;
    entries.push(Entry {
        device: device.to_owned(),
        hash: hash(&key),
        allow_tools,
    });
    save(&entries)?;
    Ok(key)
}

fn check_name(device: &str) -> Result<(), String> {
    let valid = !device.is_empty()
        && device
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if valid {
        Ok(())
    } else {
        Err("device names may only use letters, digits, dashes, underscores and dots".to_owned())
    }
}

/// Gives `device`'s key a new name; the key itself stays the same, so the device needs nothing new.
pub fn rename(device: &str, new_name: &str) -> Result<(), String> {
    check_name(new_name)?;
    let mut entries = load();
    if entries.iter().any(|entry| entry.device == new_name) {
        return Err(format!("`{new_name}` already has a key"));
    }
    let entry = entries
        .iter_mut()
        .find(|entry| entry.device == device)
        .ok_or_else(|| format!("`{device}` has no key"))?;
    new_name.clone_into(&mut entry.device);
    save(&entries)
}

/// Removes `device`'s key; false if it had none.
pub fn revoke(device: &str) -> Result<bool, String> {
    let mut entries = load();
    let before = entries.len();
    entries.retain(|entry| entry.device != device);
    if entries.len() == before {
        return Ok(false);
    }
    save(&entries)?;
    Ok(true)
}

/// The key file as the web front sees it: reread whenever it changes on disk, checked with one `stat` per request.
pub struct Keys {
    entries: Vec<Entry>,
    modified: Option<SystemTime>,
}

impl Keys {
    /// No keys yet; the file is read on the first check.
    pub const fn empty() -> Self {
        Self {
            entries: Vec::new(),
            modified: None,
        }
    }

    /// The device whose key this is, if it's valid.
    pub fn check(&mut self, key: &str) -> Option<&Entry> {
        if !well_formed(key) {
            return None;
        }
        let modified = fs::metadata(path()).and_then(|meta| meta.modified()).ok();
        if modified != self.modified {
            self.entries = load();
            self.modified = modified;
        }
        let hash = hash(key);
        self.entries.iter().find(|entry| entry.hash == hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_have_the_format_and_a_valid_checksum() {
        let key = generate().unwrap();
        assert!(key.starts_with(PREFIX));
        assert_eq!(key.len(), PREFIX.len() + RANDOM_LENGTH + CHECKSUM_LENGTH);
        assert!(well_formed(&key));
        assert_ne!(key, generate().unwrap());
    }

    #[test]
    fn typos_and_cut_off_keys_are_rejected() {
        let key = generate().unwrap();
        let mut typo = key.clone().into_bytes();
        let i = PREFIX.len() + 5;
        typo[i] = if typo[i] == b'a' { b'b' } else { b'a' };
        assert!(!well_formed(&String::from_utf8(typo).unwrap()));
        assert!(!well_formed(&key[..key.len() - 1]));
        assert!(!well_formed("sk-other-0123"));
    }

    #[test]
    fn the_file_holds_hashes_devices_and_permissions() {
        let key = generate().unwrap();
        let entries = vec![
            Entry {
                device: "phone".to_owned(),
                hash: hash(&key),
                allow_tools: false,
            },
            Entry {
                device: "laptop2".to_owned(),
                hash: hash("other"),
                allow_tools: true,
            },
        ];
        let text = render(&entries);
        assert!(!text.contains(&key));
        assert_eq!(parse(&text), entries);
    }
}
