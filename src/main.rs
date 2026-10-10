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
        /// Resume the task's session with this, then run the gate and critic again
        #[arg(long, conflicts_with = "pr", allow_hyphen_values = true)]
        reply: Option<String>,
        /// Rebase the task in Review, re-gate if it moved, and push it to main
        #[arg(long, conflicts_with_all = ["pr", "reply"])]
        merge: bool,
    },
    /// Run one request's lead session (internal, spawned detached)
    Lead {
        id: String,
        /// Resume the lead's session with this prompt instead of planning afresh
        #[arg(long, allow_hyphen_values = true)]
        reply: Option<String>,
    },
    /// Task commands for the lead (internal)
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
}

#[derive(Subcommand)]
enum TaskCommand {
    /// File a proposed task and print its id
    Propose {
        #[arg(long, allow_hyphen_values = true)]
        title: String,
        /// Id of the task this one builds on
        #[arg(long)]
        parent: Option<String>,
        #[arg(long, value_delimiter = ',')]
        crates: Vec<String>,
        /// A testable criterion, 1 to 5 times
        #[arg(long, required = true, allow_hyphen_values = true)]
        accept: Vec<String>,
        #[arg(allow_hyphen_values = true)]
        body: String,
    },
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
            reply,
            merge,
        }) => {
            let pr = pr.then(|| instruction.unwrap_or_default());
            let dir = std::env::current_dir()?;
            yogan_swarm::worker::run(&dir, &id, pr.as_deref(), reply.as_deref(), merge)
        }
        Some(Command::Lead { id, reply }) => {
            yogan_swarm::lead::run(&std::env::current_dir()?, &id, reply.as_deref())
        }
        Some(Command::Task {
            command:
                TaskCommand::Propose {
                    title,
                    parent,
                    crates,
                    accept,
                    body,
                },
        }) => {
            let checkout = std::env::current_dir()?;
            let prefix = yogan_swarm::config::load(&checkout)?.branch_prefix;
            let dir = yogan_swarm::task::state_dir(&checkout)?;
            // the lead's request, which gives its tasks their plan and ticket
            let (plan, ticket) = match std::env::var("YOGAN_REQUEST") {
                Ok(id) => (id.clone(), yogan_swarm::lead::load(&dir, &id)?.ticket),
                Err(_) => (String::new(), None),
            };
            let proposal = yogan_swarm::task::Task {
                title,
                body,
                parent,
                crates,
                acceptance: accept,
                plan,
                ticket,
                ..Default::default()
            };
            let id = yogan_swarm::task::propose(&dir, &prefix, proposal)?;
            println!("{id}");
            Ok(())
        }
    }
}
