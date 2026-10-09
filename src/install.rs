//! `corral install`: puts Corral in place, and runs again to upgrade it.
//!
//! - The user's settings (`corral.conf`, `models.ini`, `helpers.ini`, `ui.json`, `tls/`) are never overwritten;
//!   a commented starter file is created only where one is missing.
//! - Everything else that's replaced or removed is backed up first, original paths kept, under Corral's data
//!   folder (`backups/<date-time>/`), so restoring is a copy back.
//! - `--dry-run` lists every file it would create, replace, keep or remove, and changes nothing.

use std::env;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{Config, config_dir};
use crate::helpers;
use crate::units::{self, Context};

const STARTER_CONF: &str = include_str!("../assets/corral.conf");
const STARTER_MODELS: &str = include_str!("../assets/models.ini");
const STARTER_HELPERS: &str = include_str!("../assets/helpers.ini");
/// Files from before Corral was one program, replaced by `corral web` and `corral guard`.
const LEGACY_BINARIES: &[&str] = &["corral-web", "corral-guard"];
const SOCKET_PROXYD_PLACES: &[&str] = &[
    "/usr/lib/systemd/systemd-socket-proxyd",
    "/lib/systemd/systemd-socket-proxyd",
    "/usr/libexec/systemd/systemd-socket-proxyd",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A new file.
    Create,
    /// An existing file with different content; backed up first.
    Replace,
    /// Already as it should be.
    Keep,
    /// No longer part of Corral; backed up, then removed.
    Remove,
}

#[derive(Debug, Clone)]
pub struct Action {
    pub path: PathBuf,
    pub change: Change,
    /// The new content (none for removals).
    pub content: Option<Vec<u8>>,
    /// Executable file.
    pub executable: bool,
    /// Outside the user's home: written through `sudo`.
    pub system: bool,
}

pub struct Plan {
    pub actions: Vec<Action>,
    /// Problems that stop the install, such as a missing llama-server.
    pub blockers: Vec<String>,
    /// Things worth knowing that don't stop it.
    pub notes: Vec<String>,
}

/// The folders Corral installs into.
pub struct Places {
    pub home: PathBuf,
    pub bin: PathBuf,
    pub config: PathBuf,
    pub units: PathBuf,
    pub data: PathBuf,
    pub completions: PathBuf,
    pub system_units: PathBuf,
    pub hosts: PathBuf,
}

impl Places {
    pub fn of_this_user() -> Self {
        let home = PathBuf::from(env::var_os("HOME").unwrap_or_default());
        let xdg = |name: &str, fallback: &str| {
            env::var_os(name)
                .filter(|dir| !dir.is_empty())
                .map_or_else(|| home.join(fallback), PathBuf::from)
        };
        let config_home = xdg("XDG_CONFIG_HOME", ".config");
        let data_home = xdg("XDG_DATA_HOME", ".local/share");
        Self {
            bin: home.join(".local/bin"),
            config: config_dir(),
            units: config_home.join("systemd/user"),
            data: data_home.join("corral"),
            completions: data_home.join("bash-completion/completions"),
            system_units: PathBuf::from("/etc/systemd/system"),
            hosts: PathBuf::from("/etc/hosts"),
            home,
        }
    }
}

/// The first executable called `name` on `PATH`.
fn find_on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// The user's name and primary group's name, from the system's account files.
fn user_and_group() -> Option<(String, String)> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let id = |field: &str| -> Option<String> {
        status
            .lines()
            .find(|line| line.starts_with(field))?
            .split_whitespace()
            .nth(1)
            .map(str::to_owned)
    };
    let (uid, gid) = (id("Uid:")?, id("Gid:")?);
    let name_with_id = |file: &str, id: &str| -> Option<String> {
        fs::read_to_string(file).ok()?.lines().find_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            (fields.get(2) == Some(&id)).then(|| fields[0].to_owned())
        })
    };
    Some((name_with_id("/etc/passwd", &uid)?, name_with_id("/etc/group", &gid)?))
}

fn action(path: PathBuf, content: Vec<u8>, executable: bool, system: bool) -> Action {
    let change = match fs::read(&path) {
        Ok(existing) if existing == content => Change::Keep,
        Ok(_) => Change::Replace,
        Err(_) => Change::Create,
    };
    Action {
        path,
        change,
        content: Some(content),
        executable,
        system,
    }
}

fn removal(path: PathBuf, system: bool) -> Action {
    Action {
        path,
        change: Change::Remove,
        content: None,
        executable: false,
        system,
    }
}

/// A starter settings file, only where none exists.
fn starter(path: PathBuf, content: &str) -> Option<Action> {
    (!path.exists()).then(|| action(path, content.as_bytes().to_vec(), false, false))
}

/// Works out everything `corral install` would do, without changing anything.
pub fn plan(places: &Places, completion: Vec<u8>) -> Plan {
    let mut blockers = Vec::new();
    let mut notes = Vec::new();
    let config = Config::load();
    let (helpers, problems) = helpers::load();
    notes.extend(problems.iter().map(|problem| format!("helpers.ini {problem}: skipped")));

    let llama_server = find_on_path("llama-server");
    if llama_server.is_none() {
        blockers.push("llama-server isn't on PATH; install llama.cpp first (Corral uses its router mode)".to_owned());
    }
    let socket_proxyd = SOCKET_PROXYD_PLACES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file());
    if socket_proxyd.is_none() {
        blockers.push("systemd-socket-proxyd wasn't found; Corral needs systemd".to_owned());
    }
    let (user, group) = user_and_group().unwrap_or_else(|| {
        blockers.push("couldn't tell which user and group this runs as".to_owned());
        Default::default()
    });

    let binary = places.bin.join("corral");
    let display = |path: Option<PathBuf>| path.map(|path| path.display().to_string()).unwrap_or_default();
    let context = Context {
        config: &config,
        helpers: &helpers,
        corral: binary.display().to_string(),
        llama_server: display(llama_server),
        socket_proxyd: display(socket_proxyd),
        config_dir: places.config.display().to_string(),
        config_home: env::var_os("XDG_CONFIG_HOME").map(|dir| PathBuf::from(dir).display().to_string()),
        home: places.home.display().to_string(),
        user,
        group,
        has_ui_defaults: places.config.join("ui.json").is_file(),
    };

    let mut actions = Vec::new();
    match env::current_exe().and_then(fs::read) {
        Ok(program) => actions.push(action(binary, program, true, false)),
        Err(error) => blockers.push(format!("couldn't read this program to copy it: {error}")),
    }

    let user_units = units::user_units(&context);
    for unit in &user_units {
        actions.push(action(
            places.units.join(&unit.name),
            unit.content.clone().into_bytes(),
            false,
            false,
        ));
    }
    // Corral units that the settings no longer produce, such as a removed helper's
    for stale in installed_units(&places.units) {
        if !user_units.iter().any(|unit| unit.name == stale) {
            actions.push(removal(places.units.join(stale), false));
        }
    }

    actions.extend(starter(places.config.join("corral.conf"), STARTER_CONF));
    actions.extend(starter(places.config.join("models.ini"), STARTER_MODELS));
    actions.extend(starter(places.config.join("helpers.ini"), STARTER_HELPERS));
    actions.push(action(places.completions.join("corral"), completion, false, false));
    for legacy in LEGACY_BINARIES {
        let path = places.bin.join(legacy);
        if path.exists() {
            actions.push(removal(path, false));
        }
    }

    actions.extend(name_actions(&context, places, &mut notes));
    Plan {
        actions,
        blockers,
        notes,
    }
}

/// With a custom `name`: the web front's system units, the hosts line, and a check for its certificate.
fn name_actions(context: &Context, places: &Places, notes: &mut Vec<String>) -> Vec<Action> {
    let Some(name) = context.config.name() else {
        return Vec::new();
    };
    let mut actions: Vec<Action> = units::system_units(context)
        .into_iter()
        .map(|unit| {
            action(
                places.system_units.join(&unit.name),
                unit.content.into_bytes(),
                false,
                true,
            )
        })
        .collect();

    let line = format!("{} {name}", context.config.name_address());
    let hosts = fs::read_to_string(&places.hosts).unwrap_or_default();
    let has_line = hosts.lines().any(|existing| {
        let without_comment = existing.split('#').next().unwrap_or_default();
        without_comment.split_whitespace().eq(line.split_whitespace())
    });
    if !has_line {
        let mut updated = hosts;
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        let _ = writeln!(updated, "{line} # Corral");
        actions.push(action(places.hosts.clone(), updated.into_bytes(), false, true));
    }

    let tls = places.config.join("tls");
    if !tls.join("fullchain.pem").is_file() || !tls.join("privkey.pem").is_file() {
        notes.push(format!(
            "`name = {name}` needs a certificate: put fullchain.pem and privkey.pem in {}",
            tls.display()
        ));
    }
    actions
}

/// The Corral unit files in a folder.
fn installed_units(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.starts_with("corral")
                && (name.ends_with(".service") || name.ends_with(".socket") || name.ends_with(".target"))
        })
        .collect();
    found.sort();
    found
}

/// The plan as a list, grouped by what happens to each file.
pub fn describe(plan: &Plan) -> String {
    let mut out = String::new();
    for (change, heading) in [
        (Change::Create, "Create"),
        (Change::Replace, "Replace (the current file is backed up first)"),
        (Change::Remove, "Remove (backed up first)"),
        (Change::Keep, "Already up to date"),
    ] {
        let paths: Vec<&Action> = plan.actions.iter().filter(|action| action.change == change).collect();
        if paths.is_empty() {
            continue;
        }
        let _ = writeln!(out, "{heading}:");
        for action in paths {
            let sudo = if action.system { "  (sudo)" } else { "" };
            let _ = writeln!(out, "  {}{sudo}", action.path.display());
        }
    }
    for note in &plan.notes {
        let _ = writeln!(out, "Note: {note}");
    }
    for blocker in &plan.blockers {
        let _ = writeln!(out, "Can't install: {blocker}");
    }
    out
}
