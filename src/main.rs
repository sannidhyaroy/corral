use std::io;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use corral::config::Config;
use corral::status::Status;

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
    let name = match cli.command.unwrap_or(Command::Status { json: false }) {
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "corral", &mut io::stdout());
            return ExitCode::SUCCESS;
        }
        Command::Status { json } => {
            let status = Status::gather(&Config::load());
            if json {
                println!("{:#}", status.to_json());
            } else if corral::look::fancy() {
                print!("{}", status.render_fancy());
            } else {
                print!("{}", status.render_plain());
            }
            return ExitCode::SUCCESS;
        }
        Command::On => "on",
        Command::Off => "off",
        Command::Logs => "logs",
    };
    eprintln!("corral {name}: not implemented yet");
    ExitCode::FAILURE
}
