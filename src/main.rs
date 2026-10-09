use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "yogan",
    version,
    about = "Run Claude Code sessions in parallel"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run one task's lifecycle (internal, spawned detached)
    Worker { id: String },
    /// Task commands for the lead (internal)
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
}

#[derive(Subcommand)]
enum TaskCommand {
    /// File a proposed task
    Propose,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        None => anyhow::bail!("TUI not built yet (T14)"),
        Some(Command::Worker { id }) => anyhow::bail!("worker {id}: not built yet (T10)"),
        Some(Command::Task {
            command: TaskCommand::Propose,
        }) => anyhow::bail!("task propose: not built yet (T20)"),
    }
}
