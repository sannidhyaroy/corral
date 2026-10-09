use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
    Status,
    /// Follow the servers' logs
    #[command(visible_alias = "lg")]
    Logs,
    /// The web front, started by systemd on the gate's sockets
    #[command(hide = true)]
    Web,
    /// The guard, started by systemd alongside the model router
    #[command(hide = true)]
    Guard,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let name = match cli.command.unwrap_or(Command::Status) {
        Command::On => "on",
        Command::Off => "off",
        Command::Status => "status",
        Command::Logs => "logs",
        Command::Web => "web",
        Command::Guard => "guard",
    };
    eprintln!("corral {name}: not implemented yet");
    ExitCode::FAILURE
}
