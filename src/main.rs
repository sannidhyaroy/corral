use std::io;
use std::os::unix::process::CommandExt;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use corral::config::Config;
use corral::helpers::{self, Helper};
use corral::status::Status;
use corral::{router, systemd};

/// Herd and serve your llama.cpp models, on demand and behind one gate.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Arm: open the gate; servers start on their first request and stop after idle
    On {
        /// Also arm these helpers this time (comma-separated)
        #[arg(long, value_delimiter = ',', value_name = "HELPERS")]
        with: Vec<String>,
        /// Leave these helpers out this time (comma-separated)
        #[arg(long, value_delimiter = ',', value_name = "HELPERS")]
        without: Vec<String>,
    },
    /// Disarm: close the gate and stop anything running
    Off,
    /// Armed or not, what is running, which model is loaded, GPU memory (default)
    #[command(visible_alias = "st")]
    Status {
        /// Print the status as JSON, for scripts and status bars
        #[arg(long)]
        json: bool,
    },
    /// Follow the servers' logs
    #[command(visible_alias = "lg")]
    Logs,
    /// Print the completion script for a shell
    Completions { shell: Shell },
}

fn main() -> ExitCode {
    // Run by systemd, not typed, so these stay out of help and completions
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("guard") => return corral::guard::run(),
        Some("web") => return corral::web::run(),
        Some("ready") => return ready(&args[2..]),
        _ => {}
    }

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Status { json: false }) {
        Command::On { with, without } => on(&Config::load(), &with, &without),
        Command::Off => off(),
        Command::Status { json } => status(json),
        Command::Logs => logs(),
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "corral", &mut io::stdout());
            ExitCode::SUCCESS
        }
    }
}

fn on(config: &Config, with: &[String], without: &[String]) -> ExitCode {
    let (helpers, problems) = helpers::load();
    for problem in &problems {
        eprintln!("corral: helpers.ini {problem}");
    }
    let chosen = match helpers::choose(&helpers, with, without) {
        Ok(chosen) => chosen,
        Err(error) => {
            eprintln!("corral: {error}");
            return ExitCode::FAILURE;
        }
    };

    // A loaded preset that needs the whole GPU keeps yielding helpers out
    let exclusive = config.list("exclusive");
    let blocker = router::active_models(config.router_port())
        .unwrap_or_default()
        .into_iter()
        .find(|model| exclusive.contains(&model.id));
    let (armed, held_back): (Vec<&Helper>, Vec<&Helper>) = chosen
        .into_iter()
        .partition(|helper| blocker.is_none() || !helper.yields);

    // Arming states the whole setup, so helpers not chosen this time are closed
    let installed = systemd::corral_units();
    let is_installed = |unit: &String| installed.contains(unit);
    let mut start = vec![systemd::TARGET.to_owned()];
    start.extend(armed.iter().map(|helper| helper.socket_unit()).filter(is_installed));
    let stop: Vec<String> = helpers
        .iter()
        .filter(|helper| !armed.iter().any(|a| a.name == helper.name))
        .flat_map(Helper::units)
        .filter(is_installed)
        .collect();
    if let Err(error) = systemd::start(&start) {
        eprintln!("corral: couldn't arm: {error}");
        return ExitCode::FAILURE;
    }
    if !stop.is_empty()
        && let Err(error) = systemd::stop(&stop)
    {
        eprintln!("corral: couldn't close a helper: {error}");
    }

    let gate = format!("http://ai.localhost:{}", config.gate_port());
    println!("Corral armed. Models start on their first request and stop after idle.");
    println!("  Web UI:  {gate}");
    println!("  API:     {gate}/v1");
    for helper in &helpers {
        let line = if armed.iter().any(|a| a.name == helper.name) {
            if is_installed(&helper.socket_unit()) {
                format!("http://{}.localhost:{}/v1", helper.name, helper.port)
            } else {
                format!("no units yet ({} is missing)", helper.socket_unit())
            }
        } else if held_back.iter().any(|h| h.name == helper.name) {
            let model = blocker.as_ref().map_or("a preset", |model| model.name());
            format!(
                "held back: {model} needs the whole GPU (corral on --with {} once it's unloaded)",
                helper.name
            )
        } else {
            format!("not armed (corral on --with {})", helper.name)
        };
        println!("  {:<8} {line}", format!("{}:", helper.name));
    }
    ExitCode::SUCCESS
}

fn off() -> ExitCode {
    // Every unit by name, so systemctl waits until each one's processes are gone; stopping only the target can
    // return while a large model is still exiting
    let units = systemd::corral_units();
    if units.is_empty() {
        eprintln!("corral: no Corral units are installed");
        return ExitCode::FAILURE;
    }
    if let Err(error) = systemd::stop(&units) {
        eprintln!("corral: couldn't disarm: {error}");
        return ExitCode::FAILURE;
    }
    println!("Corral disarmed. Nothing is listening and nothing will start.");
    ExitCode::SUCCESS
}

fn logs() -> ExitCode {
    let units = systemd::corral_units();
    if units.is_empty() {
        eprintln!("corral: no Corral units are installed");
        return ExitCode::FAILURE;
    }
    // journalctl can't match a wildcard against user units, so each one is named
    let mut journalctl = std::process::Command::new("journalctl");
    journalctl.args(["--user", "--follow"]);
    for unit in &units {
        journalctl.args(["--unit", unit]);
    }
    // Replaces this process, so Ctrl+C and the exit code are journalctl's own
    let error = journalctl.exec();
    eprintln!("corral: couldn't run journalctl: {error}");
    ExitCode::FAILURE
}

/// `corral ready <port> [--connect]`: waits up to a minute until a server on this machine answers its health check
/// (or, with `--connect`, accepts connections), so systemd reports it started only once it can serve.
fn ready(args: &[String]) -> ExitCode {
    let Some(port) = args.first().and_then(|port| port.parse::<u16>().ok()) else {
        eprintln!("usage: corral ready <port> [--connect]");
        return ExitCode::FAILURE;
    };
    let connect_only = args.iter().any(|arg| arg == "--connect");
    for _ in 0..120 {
        let up = if connect_only {
            std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
        } else {
            corral::http::request(port, "GET", "/health", None, corral::http::SHORT, |_| true)
                .is_ok_and(|status| status == 200)
        };
        if up {
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    eprintln!("corral: nothing answered on port {port} within a minute");
    ExitCode::FAILURE
}

fn status(json: bool) -> ExitCode {
    let status = Status::gather(&Config::load());
    if json {
        println!("{:#}", status.to_json());
    } else if corral::look::fancy() {
        print!("{}", status.render_fancy());
    } else {
        print!("{}", status.render_plain());
    }
    ExitCode::SUCCESS
}
