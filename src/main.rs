use std::io;
use std::os::unix::process::CommandExt;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use corral::config::Config;
use corral::status::Status;
use corral::systemd;

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
    On,
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
    // The daemons are started by systemd, not typed, so they stay out of help and completions
    if let Some("web" | "guard") = std::env::args().nth(1).as_deref() {
        eprintln!("corral daemons: not implemented yet");
        return ExitCode::FAILURE;
    }

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Status { json: false }) {
        Command::On => on(&Config::load()),
        Command::Off => off(),
        Command::Status { json } => status(json),
        Command::Logs => logs(),
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "corral", &mut io::stdout());
            ExitCode::SUCCESS
        }
    }
}

fn on(config: &Config) -> ExitCode {
    if let Err(error) = systemd::start(&[systemd::TARGET]) {
        eprintln!("corral: couldn't arm: {error}");
        return ExitCode::FAILURE;
    }
    let gate = format!("http://ai.localhost:{}", config.gate_port());
    println!("Corral armed. Models start on their first request and stop after idle.");
    println!("  Web UI:  {gate}");
    println!("  API:     {gate}/v1");
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
    let units: Vec<&str> = units.iter().map(String::as_str).collect();
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
