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
    Worker {
        id: String,
        /// Rebase the task in Review, re-gate if it moved, and draft its PR
        #[arg(long)]
        pr: bool,
        /// With --pr: redraft the current draft following this instruction
        #[arg(long, requires = "pr")]
        instruction: Option<String>,
    },
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
        Some(Command::Worker {
            id,
            pr,
            instruction,
        }) => {
            let pr = pr.then(|| instruction.unwrap_or_default());
            yogan_swarm::worker::run(&std::env::current_dir()?, &id, pr.as_deref())
        }
        Some(Command::Task {
            command: TaskCommand::Propose,
        }) => anyhow::bail!("task propose: not built yet (T20)"),
    }
}
