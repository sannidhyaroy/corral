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
    /// Whole folders removed at the end (only when purging).
    pub folders: Vec<PathBuf>,
    /// Problems that stop the install, such as a missing llama-server.
    pub blockers: Vec<String>,
    /// Things worth knowing that don't stop it.
    pub notes: Vec<String>,
    /// What's left for the user to do by hand, said again at the end.
    pub afterwards: Vec<String>,
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
    if config.name().is_none() {
        for unit in ["corral-web.socket", "corral-web.service"] {
            let path = places.system_units.join(unit);
            if path.exists() {
                actions.push(removal(path, true));
            }
        }
    }
    Plan {
        actions,
        folders: Vec::new(),
        blockers,
        notes,
        afterwards: Vec::new(),
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

    let entry = format!("{} {name}", context.config.name_address());
    match fs::read_to_string(&places.hosts) {
        Ok(hosts) => {
            if let Some(updated) = hosts_with(&hosts, &entry) {
                notes.push(format!("adds this line to {}: {entry}", places.hosts.display()));
                actions.push(action(places.hosts.clone(), updated.into_bytes(), false, true));
            }
            // Corral never removes lines from the hosts file; an entry for an earlier name is pointed out instead
            for stale in corral_hosts_lines(&hosts, None)
                .iter()
                .filter(|line| !is_entry(line, &entry))
            {
                notes.push(format!(
                    "{} still has an entry Corral added for an earlier name; remove it if you no longer need it: {stale}",
                    places.hosts.display()
                ));
            }
        }
        // Never write a hosts file Corral couldn't read: it would replace everything in it
        Err(error) => notes.push(format!(
            "{} couldn't be read ({error}), so it's left alone; add `{entry}` to it by hand",
            places.hosts.display()
        )),
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

fn is_unit(name: &str) -> bool {
    name.ends_with(".service") || name.ends_with(".socket") || name.ends_with(".target")
}

/// The Corral unit files in a folder.
fn installed_units(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("corral") && is_unit(name))
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
        (Change::Remove, "Remove (backed up first, except with --purge)"),
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
    if !plan.folders.is_empty() {
        let _ = writeln!(out, "Remove with everything in them (not backed up):");
        for folder in &plan.folders {
            let _ = writeln!(out, "  {}/", folder.display());
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Install,
    Uninstall,
    /// Uninstall, settings and backups included; nothing is backed up.
    Purge,
}

/// Carries out a plan: backs up every file it replaces or removes (except when purging), writes the user's files
/// atomically, and hands system files to `sudo`. Returns where the backups went, if any were made.
pub fn apply(plan: &Plan, places: &Places, purpose: Purpose) -> Result<Option<PathBuf>, String> {
    let backups = places.data.join("backups").join(timestamp());
    let mut backed_up = false;
    if purpose != Purpose::Purge {
        for action in &plan.actions {
            if matches!(action.change, Change::Replace | Change::Remove) {
                back_up(&action.path, &backups)?;
                backed_up = true;
            }
        }
    }

    // Units on their way out are stopped first, so their ports are free for the units replacing them
    let unit_name = |action: &&Action| -> Option<String> {
        let name = action.path.file_name()?.to_string_lossy().into_owned();
        is_unit(&name).then_some(name)
    };
    let leaving = |system: bool| -> Vec<String> {
        plan.actions
            .iter()
            .filter(|action| action.system == system && action.change == Change::Remove)
            .filter_map(|action| unit_name(&action))
            .collect()
    };
    for (system, units) in [(false, leaving(false)), (true, leaving(true))] {
        if !units.is_empty() {
            let mut words = vec!["systemctl"];
            if !system {
                words.push("--user");
            }
            words.extend(["disable", "--now"]);
            words.extend(units.iter().map(String::as_str));
            // A unit that never ran can't be stopped; that's fine
            let _ = run(&words, system);
        }
    }

    for action in plan.actions.iter().filter(|action| action.change != Change::Keep) {
        if action.system {
            apply_system(action)?;
        } else {
            apply_user(action)?;
        }
    }
    for folder in &plan.folders {
        fs::remove_dir_all(folder).map_err(|error| format!("couldn't remove {}: {error}", folder.display()))?;
    }

    let changed = |system: bool| {
        plan.actions
            .iter()
            .any(|action| action.system == system && action.change != Change::Keep)
    };
    run(&["systemctl", "--user", "daemon-reload"], false)?;
    if changed(true) {
        run(&["systemctl", "daemon-reload"], true)?;
    }
    if purpose == Purpose::Install {
        // The web front's socket only takes new ports when restarted; its service restarts with it
        let web_changed = |system: bool| {
            plan.actions.iter().any(|action| {
                action.system == system
                    && matches!(action.change, Change::Create | Change::Replace)
                    && action
                        .path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("corral-web."))
            })
        };
        for system in [false, true] {
            if web_changed(system) {
                let user: &[&'static str] = if system { &[] } else { &["--user"] };
                let systemctl =
                    |rest: &[&'static str]| -> Vec<&'static str> { [&["systemctl"][..], user, rest].concat() };
                let _ = run(&systemctl(&["stop", "corral-web.service"]), system);
                run(&systemctl(&["enable", "corral-web.socket"]), system)?;
                run(&systemctl(&["restart", "corral-web.socket"]), system)?;
            }
        }
    }
    Ok(backed_up.then_some(backups))
}

/// Works out what `corral uninstall` removes: everything Corral installed, and with `purge` the settings folder and
/// Corral's data folder (backups included). Model files are never Corral's to remove.
pub fn plan_uninstall(places: &Places, purge: bool) -> Plan {
    let mut actions: Vec<Action> = Vec::new();
    let mut user_files = vec![places.bin.join("corral"), places.completions.join("corral")];
    user_files.extend(LEGACY_BINARIES.iter().map(|legacy| places.bin.join(legacy)));
    user_files.extend(
        installed_units(&places.units)
            .into_iter()
            .map(|unit| places.units.join(unit)),
    );
    actions.extend(
        user_files
            .into_iter()
            .filter(|path| path.exists())
            .map(|path| removal(path, false)),
    );

    for unit in ["corral-web.socket", "corral-web.service"] {
        let path = places.system_units.join(unit);
        if path.exists() {
            actions.push(removal(path, true));
        }
    }
    // Corral never removes lines from the hosts file: it points out its own and leaves the edit to the user
    let config = Config::load();
    let entry = config.name().map(|name| format!("{} {name}", config.name_address()));
    let hosts = fs::read_to_string(&places.hosts).unwrap_or_default();
    let mut afterwards: Vec<String> = corral_hosts_lines(&hosts, entry.as_deref())
        .into_iter()
        .map(|line| {
            format!(
                "remove Corral's line from {} if you no longer need it: {line}",
                places.hosts.display()
            )
        })
        .collect();
    if !purge {
        for (folder, holds) in [
            (&places.config, "your settings and certificate"),
            (&places.data, "backups"),
        ] {
            if folder.exists() {
                afterwards.push(format!(
                    "{}/ is kept ({holds}); `corral uninstall --purge` removes it",
                    folder.display()
                ));
            }
        }
    }

    let folders = if purge {
        [&places.config, &places.data]
            .into_iter()
            .filter(|folder| folder.exists())
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    Plan {
        actions,
        folders,
        blockers: Vec::new(),
        notes: Vec::new(),
        afterwards,
    }
}

/// `hosts` with Corral's entry appended after everything already there, or `None` if the entry is already present.
pub fn hosts_with(hosts: &str, entry: &str) -> Option<String> {
    if hosts.lines().any(|line| is_entry(line, entry)) {
        return None;
    }
    let mut updated = hosts.to_owned();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    let _ = writeln!(updated, "{entry} # Corral");
    debug_assert!(updated.starts_with(hosts));
    Some(updated)
}

/// Whether a hosts line's entry (the part before any `#`) is `entry`, word for word.
fn is_entry(line: &str, entry: &str) -> bool {
    let before_comment = line.split('#').next().unwrap_or_default();
    before_comment.split_whitespace().eq(entry.split_whitespace())
}

/// The hosts lines Corral recognises as its own: entries marked with exactly `# Corral` (as `corral install` writes
/// them), or the entry for the configured name. Comments alone never count.
pub fn corral_hosts_lines(hosts: &str, entry: Option<&str>) -> Vec<String> {
    hosts
        .lines()
        .filter(|line| {
            let (before, comment) = line.split_once('#').unwrap_or((line, ""));
            !before.trim().is_empty()
                && (comment.trim() == "Corral" || entry.is_some_and(|entry| is_entry(line, entry)))
        })
        .map(str::to_owned)
        .collect()
}

/// Copies a file into the backup folder under its full path.
fn back_up(path: &Path, backups: &Path) -> Result<(), String> {
    let target = backups.join(path.strip_prefix("/").unwrap_or(path));
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("couldn't create {}: {error}", parent.display()))?;
    }
    fs::copy(path, &target).map_err(|error| format!("couldn't back up {}: {error}", path.display()))?;
    Ok(())
}

/// Writes or removes one of the user's files. New content goes to a temporary file first and replaces the old one
/// in a single rename, so a file is never half-written.
fn apply_user(action: &Action) -> Result<(), String> {
    let path = &action.path;
    let Some(content) = &action.content else {
        return fs::remove_file(path).map_err(|error| format!("couldn't remove {}: {error}", path.display()));
    };
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no folder", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("couldn't create {}: {error}", parent.display()))?;
    let temporary = parent.join(format!(
        ".{}.corral-new",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::write(&temporary, content).map_err(|error| format!("couldn't write {}: {error}", temporary.display()))?;
    let mode = if action.executable { 0o755 } else { 0o644 };
    fs::set_permissions(&temporary, std::os::unix::fs::PermissionsExt::from_mode(mode))
        .map_err(|error| format!("couldn't set permissions on {}: {error}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|error| format!("couldn't replace {}: {error}", path.display()))
}

/// Writes a system file through `sudo`. An existing file is overwritten in place with `cp`, which keeps its owner,
/// permissions and security labels (a replacement `/etc/hosts` would lose the label some systems give it); a new file is
/// created with `install`, owned by root.
fn apply_system(action: &Action) -> Result<(), String> {
    let path = action.path.display().to_string();
    let Some(content) = &action.content else {
        return run(&["rm", "--", &path], true);
    };
    let staged = env::temp_dir().join(format!("corral-{}", std::process::id()));
    fs::write(&staged, content).map_err(|error| format!("couldn't stage {path}: {error}"))?;
    let staged_path = staged.display().to_string();
    let command: Vec<&str> = if action.path.exists() {
        vec!["cp", "--", &staged_path, &path]
    } else {
        vec![
            "install",
            "-m",
            "644",
            "-o",
            "root",
            "-g",
            "root",
            "--",
            &staged_path,
            &path,
        ]
    };
    let result = run(&command, true);
    let _ = fs::remove_file(&staged);
    result
}

/// Runs a command, through `sudo` when `root`, showing it first.
fn run(command: &[&str], root: bool) -> Result<(), String> {
    let mut words: Vec<&str> = command.to_vec();
    if root {
        words.insert(0, "sudo");
    }
    println!("  $ {}", words.join(" "));
    let status = std::process::Command::new(words[0])
        .args(&words[1..])
        .status()
        .map_err(|error| format!("couldn't run {}: {error}", words[0]))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{}` failed", words.join(" ")))
    }
}

/// The current time in UTC as `2026-10-09-201530`, for backup folder names.
fn timestamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let (days, time) = (seconds / 86_400, seconds % 86_400);
    let (year, month, day) = civil_date(days);
    format!(
        "{year:04}-{month:02}-{day:02}-{:02}{:02}{:02}",
        time / 3600,
        time % 3600 / 60,
        time % 60
    )
}

/// Days since 1970-01-01 as a calendar date (Howard Hinnant's algorithm).
fn civil_date(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_dates() {
        assert_eq!(civil_date(0), (1970, 1, 1));
        assert_eq!(civil_date(20_735), (2026, 10, 9));
        assert_eq!(civil_date(11_016), (2000, 2, 29));
    }

    #[test]
    fn user_files_are_written_whole_and_backed_up() {
        let root = env::temp_dir().join(format!("corral-install-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let file = root.join("bin/corral");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "old").unwrap();

        let backups = root.join("backups");
        back_up(&file, &backups).unwrap();
        let replace = action(file.clone(), b"new".to_vec(), true, false);
        assert_eq!(replace.change, Change::Replace);
        apply_user(&replace).unwrap();

        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
        let saved = backups.join(file.strip_prefix("/").unwrap());
        assert_eq!(fs::read_to_string(saved).unwrap(), "old");
        let mode = std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&file).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o755);
        fs::remove_dir_all(&root).unwrap();
    }

    const HOSTS: &str = "127.0.0.1\tlocalhost\n::1\tlocalhost ipv6-localhost\n\n# Corral Rules\n127.0.97.105\tcorral.ai\t# Corral: 127.0.'a'.'i'\n10.0.0.5 nas # Corral-ish name, not ours\n";

    #[test]
    fn corrals_hosts_lines_are_recognised_not_removed() {
        let marked = "127.0.0.1 localhost\n127.0.97.105 old.example # Corral\n# Corral\n10.0.0.5 nas # Corral-ish\n";
        assert_eq!(corral_hosts_lines(marked, None), ["127.0.97.105 old.example # Corral"]);
        assert_eq!(
            corral_hosts_lines(HOSTS, Some("127.0.97.105 corral.ai")),
            ["127.0.97.105\tcorral.ai\t# Corral: 127.0.'a'.'i'"]
        );
        assert!(corral_hosts_lines(HOSTS, None).is_empty());
    }

    #[test]
    fn hosts_gain_one_line_at_the_end() {
        let base = "127.0.0.1 localhost";
        assert_eq!(
            hosts_with(base, "127.0.97.105 corral.example").as_deref(),
            Some("127.0.0.1 localhost\n127.0.97.105 corral.example # Corral\n")
        );
        assert_eq!(hosts_with(HOSTS, "127.0.97.105 corral.ai"), None);
    }
}
