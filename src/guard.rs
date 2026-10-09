//! `corral guard`: runs exactly as long as the router and keeps models from crowding each other and the desktop out.
//!
//! - **Page cache:** when the router switches to a model from another file, the old file is dropped from the page
//!   cache, so stale cache doesn't crowd the next load. An unloaded file waits a moment first, in case the next
//!   preset uses the same file.
//! - **Making room:** when a preset listed under `exclusive` starts loading, helpers that yield are stopped.
//! - **GPU memory:** while a model is loading or loaded, memory is checked every second. Past the warning level,
//!   yielding helpers are stopped first, then a notification is sent; at the unload level, the model is unloaded.
//! - **Helpers that claim the GPU:** when one starts, the router's model is unloaded.
//!
//! It talks to the router's own port, so it never starts the router or keeps it alive.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, sleep};
use std::time::Duration;

use crate::config::{Config, GpuLimits};
use crate::helpers::{self, Helper};
use crate::router::{self, Event};
use crate::system;
use crate::systemd;

/// After an unload with nothing loading behind it, how long a file stays cached: switching between two presets of
/// the same file reuses it.
const EVICT_DELAY: Duration = Duration::from_secs(10);
const CHECK_EVERY: Duration = Duration::from_secs(1);
/// After unloading, give the memory a moment to come back before checking again.
const AFTER_UNLOAD: Duration = Duration::from_secs(5);
/// How far below the warning level memory must fall before a new warning can be sent.
const WARN_AGAIN_BELOW: u64 = 512 << 20;
const RECONNECT_AFTER: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
struct State {
    /// The preset that's loading or loaded.
    current: Option<String>,
    /// Each preset's model file.
    files: HashMap<String, String>,
    /// An unloaded model's file, dropped after `EVICT_DELAY` unless a load decides first.
    pending_drop: Option<String>,
}

type Shared = Arc<Mutex<State>>;

fn lock(state: &Shared) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn run() -> ExitCode {
    let state = Shared::default();
    let watchdog_state = Arc::clone(&state);
    thread::spawn(move || watch_memory(&watchdog_state));
    loop {
        let port = Config::load().router_port();
        if let Err(error) = follow_router(port, &state)
            && error.kind() != std::io::ErrorKind::ConnectionRefused
        {
            eprintln!("router connection ended: {error}");
        }
        lock(&state).current = None;
        sleep(RECONNECT_AFTER);
    }
}

/// Follows the router's events until the stream ends (the router stopped; systemd then stops the guard with it).
fn follow_router(port: u16, state: &Shared) -> std::io::Result<()> {
    let files = router::preset_files(port).ok_or(std::io::ErrorKind::ConnectionRefused)?;
    let current = router::active_models(port)
        .unwrap_or_default()
        .into_iter()
        .next()
        .map(|model| model.id);
    println!(
        "watching the router (current model: {})",
        current.as_deref().unwrap_or("none")
    );
    if let Some(model) = &current {
        make_room_for(model);
    }
    {
        let mut state = lock(state);
        state.files = files;
        state.current = current;
    }

    router::follow_events(port, |event| {
        match event {
            Event::Reload => {
                if let Some(files) = router::preset_files(port) {
                    lock(state).files = files;
                }
            }
            Event::State { model, state: new } if new == "loading" => on_loading(state, &model),
            Event::State { model, state: new } if matches!(new.as_str(), "unloaded" | "failed") => {
                on_unloaded(state, &model);
            }
            Event::State { .. } => {}
        }
        true
    })
}

/// "Loading" arrives repeatedly while a load progresses; only the first one for a model acts.
fn on_loading(state: &Shared, model: &str) {
    let mut guard = lock(state);
    if guard.current.as_deref() == Some(model) {
        return;
    }
    let to_drop = file_to_drop(&guard, model);
    guard.current = Some(model.to_owned());
    drop(guard);
    make_room_for(model);
    if let Some(file) = to_drop {
        evict(&file);
    }
}

/// The file to drop when `model` starts loading: the previous model's (still loaded, or waiting for its delayed
/// drop), unless the new model uses the same file, whose cache is exactly what the load needs.
fn file_to_drop(state: &State, model: &str) -> Option<String> {
    let new_file = state.files.get(model);
    let previous = state
        .current
        .as_deref()
        .filter(|previous| *previous != model)
        .and_then(|previous| state.files.get(previous))
        .or(state.pending_drop.as_ref());
    previous.filter(|old| Some(*old) != new_file).cloned()
}

fn on_unloaded(state: &Shared, model: &str) {
    let mut guard = lock(state);
    if guard.current.as_deref() != Some(model) {
        return;
    }
    guard.current = None;
    let Some(file) = guard.files.get(model).cloned() else {
        return;
    };
    guard.pending_drop = Some(file.clone());
    drop(guard);
    let state = Arc::clone(state);
    thread::spawn(move || {
        sleep(EVICT_DELAY);
        let mut guard = lock(&state);
        if guard.pending_drop.as_deref() == Some(file.as_str()) {
            guard.pending_drop = None;
            drop(guard);
            evict(&file);
        }
    });
}

/// Drops a file from the page cache.
fn evict(path: &str) {
    let name = path.rsplit('/').next().unwrap_or(path);
    let Ok(file) = File::open(path) else {
        eprintln!("couldn't open {name} to drop it from the page cache");
        return;
    };
    // SAFETY: `file` is open for the whole call; offset 0 and length 0 cover the whole file.
    let result = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    if result == 0 {
        println!("dropped from the page cache: {name}");
    } else {
        eprintln!("couldn't drop {name} from the page cache (error {result})");
    }
}

/// An `exclusive` preset is starting: stop the helpers that yield.
fn make_room_for(model: &str) {
    if !Config::load().list("exclusive").iter().any(|preset| preset == model) {
        return;
    }
    let stopped = stop_yielding_helpers();
    if !stopped.is_empty() {
        let names = stopped.join(", ");
        println!("stopped {names} to make room for {model}");
        notify(
            "Helpers stopped to make room",
            &format!(
                "{model} needs the whole GPU, so Corral stopped {names}. `corral on --with {names}` brings them back once it's unloaded."
            ),
        );
    }
}

/// Stops every helper that yields and is armed or running; returns their names.
fn stop_yielding_helpers() -> Vec<String> {
    let (helpers, _) = helpers::load();
    let running: Vec<&Helper> = helpers
        .iter()
        .filter(|helper| helper.yields && systemd::active(&helper.units()).into_iter().any(|active| active))
        .collect();
    let units: Vec<String> = running.iter().flat_map(|helper| helper.units()).collect();
    if !units.is_empty()
        && let Err(error) = systemd::stop(&units)
    {
        eprintln!("couldn't stop helpers: {error}");
    }
    running.iter().map(|helper| helper.name.clone()).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pressure {
    Normal,
    Warn,
    Unload,
}

fn pressure(used: u64, limits: GpuLimits) -> Pressure {
    if used >= limits.unload {
        Pressure::Unload
    } else if used >= limits.warn {
        Pressure::Warn
    } else {
        Pressure::Normal
    }
}

/// Checks GPU memory every second while a model is loading or loaded, and helpers that claim the GPU.
fn watch_memory(state: &Shared) {
    let mut warned = false;
    let mut claiming: HashSet<String> = HashSet::new();
    loop {
        sleep(CHECK_EVERY);
        let config = Config::load();
        let Some(model) = lock(state).current.clone() else {
            warned = false;
            claiming.clear();
            continue;
        };

        if let Some(helper) = newly_claiming(&mut claiming) {
            let unloaded = router::unload(config.router_port(), &model);
            println!("{helper} claims the GPU: unloading {model} ({})", done(unloaded));
            notify(
                &format!("{model} unloaded"),
                &format!("{helper} needs the whole GPU, so Corral unloaded {model}."),
            );
            continue;
        }

        let Some(gpu) = system::read_gpu_memory() else {
            continue;
        };
        let limits = config.gpu_limits(gpu.usage.total);
        let (used, total) = (gib(gpu.usage.used), gib(gpu.usage.total));
        match pressure(gpu.usage.used, limits) {
            Pressure::Unload => {
                let unloaded = router::unload(config.router_port(), &model);
                println!(
                    "GPU memory {used} of {total} GiB: unloading {model} ({})",
                    done(unloaded)
                );
                notify(
                    &format!("{model} unloaded"),
                    &format!(
                        "GPU memory reached {used} of {total} GiB, so Corral unloaded the model to protect the desktop. Close something before loading it again."
                    ),
                );
                lock(state).current = None;
                warned = false;
                sleep(AFTER_UNLOAD);
            }
            Pressure::Warn if !warned => {
                let stopped = stop_yielding_helpers();
                if stopped.is_empty() {
                    println!("GPU memory {used} of {total} GiB with {model} loaded: warning sent");
                    notify(
                        "GPU memory is nearly full",
                        &format!(
                            "{used} of {total} GiB in use with {model} loaded. Close something, or unload the model; at {} GiB Corral unloads it.",
                            gib(limits.unload)
                        ),
                    );
                } else {
                    let names = stopped.join(", ");
                    println!("GPU memory {used} of {total} GiB: stopped {names}");
                    notify(
                        "Helpers stopped to free GPU memory",
                        &format!("{used} of {total} GiB in use with {model} loaded, so Corral stopped {names}."),
                    );
                }
                warned = true;
            }
            _ if gpu.usage.used + WARN_AGAIN_BELOW < limits.warn => warned = false,
            _ => {}
        }
    }
}

/// A helper that claims the GPU and has started since the last check.
fn newly_claiming(claiming: &mut HashSet<String>) -> Option<String> {
    let (helpers, _) = helpers::load();
    let running: HashSet<String> = helpers
        .iter()
        .filter(|helper| helper.claims_gpu && systemd::active(&[helper.service_unit()])[0])
        .map(|helper| helper.name.clone())
        .collect();
    let new = running.difference(claiming).next().cloned();
    *claiming = running;
    new
}

fn done(success: bool) -> &'static str {
    if success { "done" } else { "failed" }
}

#[allow(clippy::cast_precision_loss)] // memory sizes are far below f64's exact range
fn gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / f64::from(1 << 30))
}

/// A desktop notification over the session bus; without a notification service, it's silently skipped.
fn notify(title: &str, body: &str) {
    let _ = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.Notifications",
            "--object-path",
            "/org/freedesktop/Notifications",
            "--method",
            "org.freedesktop.Notifications.Notify",
            "Corral",
            "0",
            "",
            title,
            body,
            "[]",
            "{}",
            "10000",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn state(current: Option<&str>, pending: Option<&str>) -> State {
        State {
            current: current.map(str::to_owned),
            files: HashMap::from([
                ("gemma".to_owned(), "/m/gemma.gguf".to_owned()),
                ("gemma-long".to_owned(), "/m/gemma.gguf".to_owned()),
                ("qwen".to_owned(), "/m/qwen.gguf".to_owned()),
            ]),
            pending_drop: pending.map(str::to_owned),
        }
    }

    #[test]
    fn switching_files_drops_the_old_one() {
        assert_eq!(
            file_to_drop(&state(Some("gemma"), None), "qwen").as_deref(),
            Some("/m/gemma.gguf")
        );
    }

    #[test]
    fn the_same_file_keeps_its_cache() {
        assert_eq!(file_to_drop(&state(Some("gemma"), None), "gemma-long"), None);
        assert_eq!(file_to_drop(&state(None, Some("/m/gemma.gguf")), "gemma-long"), None);
    }

    #[test]
    fn a_pending_drop_happens_at_once_when_another_file_loads() {
        assert_eq!(
            file_to_drop(&state(None, Some("/m/gemma.gguf")), "qwen").as_deref(),
            Some("/m/gemma.gguf")
        );
    }

    #[test]
    fn pressure_levels() {
        let limits = GpuLimits {
            warn: 20 * GIB,
            unload: 22 * GIB,
        };
        assert_eq!(pressure(10 * GIB, limits), Pressure::Normal);
        assert_eq!(pressure(20 * GIB, limits), Pressure::Warn);
        assert_eq!(pressure(23 * GIB, limits), Pressure::Unload);
    }
}
