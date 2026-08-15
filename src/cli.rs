use std::ffi::OsString;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "agentree",
    version,
    about = "Isolated Git worktree task manager"
)]
pub struct Cli {
    #[arg(long, global = true)]
    pub json: bool,
    #[arg(long, global = true)]
    pub quiet: bool,
    #[arg(long, global = true)]
    pub repository: Option<OsString>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Init,
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    New(NewArgs),
    Status,
    Context {
        task: String,
    },
    Diff {
        task: String,
    },
    Run(RunArgs),
    Shell {
        task: String,
    },
    Git {
        task: String,
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    Remove {
        task: String,
    },
    Archive {
        task: String,
    },
    DeleteBranch {
        task: String,
        #[arg(long)]
        yes: bool,
    },
    Doctor {
        #[arg(long)]
        operation: Option<String>,
        #[arg(long)]
        plan: bool,
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        plan_fingerprint: Option<String>,
    },
    Checkpoint {
        #[command(subcommand)]
        command: CheckpointCommand,
    },
    Overlap(OverlapArgs),
    Check {
        task: String,
    },
    Fetch {
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    Sync(SyncArgs),
    Resolve {
        task: String,
        #[arg(long)]
        shell: bool,
    },
    Land(LandArgs),
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Scaffold,
}

#[derive(Debug, Subcommand)]
pub enum CheckpointCommand {
    Create {
        task: String,
        #[arg(short, long)]
        message: Option<String>,
    },
    List {
        task: String,
    },
    Show {
        id: String,
    },
    Restore {
        id: String,
        #[arg(long)]
        to_new_task: String,
    },
    #[command(external_subcommand)]
    Legacy(Vec<OsString>),
}

#[derive(Debug, Args)]
pub struct OverlapArgs {
    pub tasks: Vec<String>,
    #[arg(long, conflicts_with_all = ["actual", "all"])]
    pub planned: bool,
    #[arg(long, conflicts_with_all = ["planned", "all"])]
    pub actual: bool,
    #[arg(long, conflicts_with_all = ["planned", "actual"])]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct NewArgs {
    pub slug: String,
    #[arg(long)]
    pub base: Option<String>,
    #[arg(long = "scope")]
    pub scopes: Vec<String>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    pub task: String,
    #[arg(last = true, required = true)]
    pub program: Vec<OsString>,
    #[arg(long)]
    pub require_post_checks: bool,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    pub task: String,
    #[arg(long)]
    pub onto: Option<String>,
    #[arg(long)]
    pub r#continue: bool,
    #[arg(long)]
    pub abort: bool,
}

#[derive(Debug, Args)]
pub struct LandArgs {
    pub task: String,
    #[arg(long, conflicts_with = "onto")]
    pub into_current: bool,
    #[arg(long, conflicts_with = "into_current")]
    pub onto: Option<String>,
}

pub fn parse_args(args: Vec<OsString>) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(args)
}

pub fn is_git_shim(args: &[OsString]) -> bool {
    args.first()
        .and_then(|arg| std::path::Path::new(arg).file_name())
        .is_some_and(|name| name == "git")
}
