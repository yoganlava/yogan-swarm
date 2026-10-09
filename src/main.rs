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
    let mut args = std::env::args_os();
    let argv0 = args.next().unwrap_or_default();
    if std::path::Path::new(&argv0).file_name() == Some("cargo".as_ref()) {
        std::process::exit(yogan_swarm::shim::run(args.collect())?);
    }
    match Cli::parse().command {
        None => yogan_swarm::tui::run(&std::env::current_dir()?),
        Some(Command::Worker { id }) => yogan_swarm::worker::run(&std::env::current_dir()?, &id),
        Some(Command::Task {
            command: TaskCommand::Propose,
        }) => anyhow::bail!("task propose: not built yet (T20)"),
    }
}
