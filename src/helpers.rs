//! Helpers: models that run beside the router, each in its own server, defined in `helpers.ini`. One section per
//! helper; the section name becomes its unit names (`corral-<name>.socket` / `.service`).
//!
//! A `llama` helper (the default) runs llama-server, and its keys are llama-server options exactly as in
//! `models.ini`. A `custom` helper runs `command`, with `{port}` replaced by its internal port. Corral's own keys:
//! `type`, `port` (where clients reach it), `internal-port` (default: `port` + 10000), `armed` (whether plain
//! `corral on` includes it), `yields` (steps aside for whole-GPU presets; default yes), `claims` (`gpu`: the router's
//! model is unloaded when it starts).

use std::fs;

use crate::config::{Config, config_dir};

const RESERVED_NAMES: &[&str] = &["router", "guard", "web"];
const CORRAL_KEYS: &[&str] = &["type", "port", "internal-port", "armed", "yields", "claims", "command"];
const INTERNAL_PORT_OFFSET: u16 = 10000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// llama-server with the helper's options.
    Llama { options: Vec<(String, String)> },
    /// Any other server.
    Custom { command: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Helper {
    pub name: String,
    pub kind: Kind,
    pub port: u16,
    pub internal_port: u16,
    pub armed: bool,
    pub yields: bool,
    pub claims_gpu: bool,
}

impl Helper {
    pub fn socket_unit(&self) -> String {
        format!("corral-{}.socket", self.name)
    }

    pub fn service_unit(&self) -> String {
        format!("corral-{}.service", self.name)
    }

    /// The command line that starts this helper's server on its internal port.
    pub fn command_line(&self, home: &str) -> Vec<String> {
        match &self.kind {
            Kind::Llama { options } => {
                let mut args = vec!["llama-server".to_owned()];
                for (key, value) in options {
                    match value.as_str() {
                        "true" => args.push(format!("--{key}")),
                        "false" => {}
                        value => {
                            args.push(format!("--{key}"));
                            args.push(expand_home(value, home));
                        }
                    }
                }
                let port = self.internal_port.to_string();
                args.extend(["--host", "127.0.0.1", "--port", &port, "--no-webui"].map(str::to_owned));
                args
            }
            Kind::Custom { command } => command
                .replace("{port}", &self.internal_port.to_string())
                .split_whitespace()
                .map(|word| expand_home(word, home))
                .collect(),
        }
    }
}

/// The helpers `corral on` arms: those marked `armed`, plus `with`, minus `without`. Names that aren't helpers are
/// an error, so a typo doesn't silently arm less than intended.
pub fn choose<'a>(helpers: &'a [Helper], with: &[String], without: &[String]) -> Result<Vec<&'a Helper>, String> {
    if let Some(unknown) = with
        .iter()
        .chain(without)
        .find(|name| !helpers.iter().any(|h| &h.name == *name))
    {
        return Err(format!("there is no helper named `{unknown}` in helpers.ini"));
    }
    Ok(helpers
        .iter()
        .filter(|helper| (helper.armed || with.contains(&helper.name)) && !without.contains(&helper.name))
        .collect())
}

/// The helpers in `helpers.ini`, plus a message for each section that couldn't be used.
pub fn load() -> (Vec<Helper>, Vec<String>) {
    fs::read_to_string(config_dir().join("helpers.ini")).map_or_else(|_| (Vec::new(), Vec::new()), |text| parse(&text))
}

pub fn parse(text: &str) -> (Vec<Helper>, Vec<String>) {
    let mut helpers = Vec::new();
    let mut problems = Vec::new();
    for (name, body) in sections(text) {
        match helper(&name, &Config::parse(&body)) {
            Ok(helper) if helpers.iter().any(|h: &Helper| h.name == helper.name) => {
                problems.push(format!("[{name}]: defined twice"));
            }
            Ok(helper) => helpers.push(helper),
            Err(problem) => problems.push(format!("[{name}]: {problem}")),
        }
    }
    (helpers, problems)
}

/// The `[name]` sections of an INI text and the lines under each.
fn sections(text: &str) -> Vec<(String, String)> {
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
}

fn helper(name: &str, section: &Config) -> Result<Helper, String> {
    let valid_name = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-');
    if !valid_name {
        return Err("names may only use lowercase letters, digits and dashes".to_owned());
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(format!("`{name}` is reserved for Corral itself"));
    }

    let port: u16 = match section.get("port").map(str::parse) {
        Some(Ok(port)) if port != 0 => port,
        Some(_) => return Err("`port` must be a number from 1 to 65535".to_owned()),
        None => return Err("`port` is missing".to_owned()),
    };
    let internal_port = match section.get("internal-port").map(str::parse) {
        Some(Ok(internal)) if internal != 0 => internal,
        Some(_) => return Err("`internal-port` must be a number from 1 to 65535".to_owned()),
        None => port
            .checked_add(INTERNAL_PORT_OFFSET)
            .ok_or("`port` is too high for the default internal port; set `internal-port`")?,
    };

    let kind = match section.get("type").unwrap_or("llama") {
        "llama" => {
            if section.get("model").is_none() {
                return Err("a llama helper needs a `model`".to_owned());
            }
            let options = section
                .entries()
                .filter(|(key, _)| !CORRAL_KEYS.contains(key))
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect();
            Kind::Llama { options }
        }
        "custom" => Kind::Custom {
            command: section
                .get("command")
                .ok_or("a custom helper needs a `command`")?
                .to_owned(),
        },
        other => return Err(format!("unknown type `{other}` (use `llama` or `custom`)")),
    };

    Ok(Helper {
        name: name.to_owned(),
        kind,
        port,
        internal_port,
        armed: section.flag("armed", false),
        yields: section.flag("yields", true),
        claims_gpu: section.get("claims") == Some("gpu"),
    })
}

fn expand_home(value: &str, home: &str) -> String {
    match value.strip_prefix("~/") {
        Some(rest) => format!("{home}/{rest}"),
        None => value.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# Helpers beside the router
[sweep]
model = ~/Models/sweep-next-edit-1.5b.q8_0.v2.gguf
ctx-size = 8192
spec-type = ngram-mod
no-mmap = true
port = 8081
armed = no

[kokoro]
type = custom
command = kokoro-server --host 127.0.0.1 --port {port}
port = 8083
internal-port = 9083
yields = no

[router]
port = 9000

[whisper]
model = ~/Models/ggml-large-v3-turbo.bin
";

    #[test]
    fn reads_valid_helpers_and_reports_the_rest() {
        let (helpers, problems) = parse(SAMPLE);
        let names: Vec<&str> = helpers.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["sweep", "kokoro"]);
        assert_eq!(
            problems,
            [
                "[router]: `router` is reserved for Corral itself",
                "[whisper]: `port` is missing",
            ]
        );
    }

    #[test]
    fn llama_helpers_become_llama_server_command_lines() {
        let (helpers, _) = parse(SAMPLE);
        let sweep = &helpers[0];
        assert_eq!(sweep.internal_port, 18081);
        assert!(!sweep.armed && sweep.yields && !sweep.claims_gpu);
        assert_eq!(sweep.socket_unit(), "corral-sweep.socket");
        assert_eq!(
            sweep.command_line("/home/me").join(" "),
            "llama-server --ctx-size 8192 --model /home/me/Models/sweep-next-edit-1.5b.q8_0.v2.gguf --no-mmap \
             --spec-type ngram-mod --host 127.0.0.1 --port 18081 --no-webui"
        );
    }

    #[test]
    fn custom_helpers_get_their_internal_port() {
        let (helpers, _) = parse(SAMPLE);
        let kokoro = &helpers[1];
        assert!(!kokoro.yields);
        assert_eq!(
            kokoro.command_line("/home/me").join(" "),
            "kokoro-server --host 127.0.0.1 --port 9083"
        );
    }

    #[test]
    fn names_and_types_are_checked() {
        let (_, problems) = parse("[Sweep]\nport = 1\n[x]\ntype = sd\nport = 2\n[y]\ntype = custom\nport = 3\n");
        assert_eq!(problems.len(), 3);
        assert!(problems[0].contains("lowercase"));
        assert!(problems[1].contains("unknown type `sd`"));
        assert!(problems[2].contains("needs a `command`"));
    }

    #[test]
    fn choosing_what_corral_on_arms() {
        let (helpers, _) = parse(
            "[a]\nport = 1\ntype = custom\ncommand = a\narmed = yes\n[b]\nport = 2\ntype = custom\ncommand = b\n",
        );
        let names = |chosen: Vec<&Helper>| chosen.iter().map(|h| h.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(choose(&helpers, &[], &[]).unwrap()), ["a"]);
        assert_eq!(
            names(choose(&helpers, &["b".to_owned()], &["a".to_owned()]).unwrap()),
            ["b"]
        );
        assert!(
            choose(&helpers, &["c".to_owned()], &[])
                .unwrap_err()
                .contains("no helper named `c`")
        );
    }
}
