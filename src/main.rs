mod auth;
mod cmd;
mod github;
mod jj;
mod repo;
mod stack;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

use cmd::Ctx;

/// jj-native access to GitHub pull requests, with first-class support for stacks.
///
/// Add `gh = ["util", "exec", "--", "jj-gh"]` under `[aliases]` in your jj
/// config to run these as `jj gh <command>`.
#[derive(Parser)]
#[command(name = "jj-gh", version = env!("JJ_GH_VERSION"), about, long_about = None)]
struct Cli {
    /// Path to the jj repository (default: current directory).
    #[arg(short = 'R', long, global = true, value_name = "PATH")]
    repository: Option<PathBuf>,

    /// Git remote that points at GitHub.
    #[arg(long, global = true, default_value = "origin", env = "JJ_GH_REMOTE")]
    remote: String,

    /// GitHub repository as OWNER/NAME or HOST/OWNER/NAME, instead of
    /// inferring it from the remote's URL.
    #[arg(long, global = true, env = "JJ_GH_REPO", value_name = "SLUG")]
    repo: Option<String>,

    /// Group changes into PRs that end at existing bookmarks (plus the stack
    /// head), instead of making one PR per change.
    #[arg(long, global = true, env = "JJ_GH_BY_BOOKMARK")]
    by_bookmark: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the stack of pull requests between trunk and the working copy.
    #[command(visible_aliases = ["st", "stack"])]
    Status {
        /// Revset for the stack (default: the native stack, else trunk()..@; an unpushable @ is left out).
        #[arg(short, long)]
        revision: Option<String>,
    },
    /// Push the stack and create or update one PR per segment, each based on the one below.
    #[command(visible_alias = "sub")]
    Submit {
        #[arg(short, long)]
        revision: Option<String>,
        /// Create new PRs as drafts.
        #[arg(long)]
        draft: bool,
        /// Say what would happen without touching jj or GitHub.
        #[arg(short = 'n', long)]
        dry_run: bool,
        /// Overwrite titles and descriptions of existing PRs from the change descriptions.
        #[arg(long)]
        update_description: bool,
        /// Request review from these users on newly created PRs.
        #[arg(long = "reviewer", value_name = "LOGIN")]
        reviewers: Vec<String>,
    },
    /// Fetch, drop merged PRs from the bottom of the stack, and restack onto trunk.
    Sync {
        #[arg(short, long)]
        revision: Option<String>,
        /// Run `submit` afterwards to push the restacked branches.
        #[arg(long)]
        submit: bool,
    },
    /// Merge the bottom PR of the stack once it is ready, then sync.
    Merge {
        #[arg(short, long)]
        revision: Option<String>,
        /// Merge method.
        #[arg(long, default_value = "squash", value_parser = ["squash", "merge", "rebase"])]
        method: String,
        /// Skip readiness checks (draft, CI, reviews, unpushed changes).
        #[arg(long)]
        force: bool,
        /// Run `submit` after syncing, to push the rest of the stack.
        #[arg(long)]
        submit: bool,
    },
    /// Work with individual pull requests.
    #[command(subcommand)]
    Pr(PrCommand),
    /// Show which credentials are in use and who they belong to.
    Auth,
    /// Print shell completions.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand)]
enum PrCommand {
    /// Open the PR for a change in the browser (default: the stack head).
    Open {
        rev: Option<String>,
        /// Only print the URL.
        #[arg(long)]
        print: bool,
    },
    /// Fetch a PR's branch and start a new change on top of it.
    Checkout { number: u64 },
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let ctx = Ctx {
        jj: jj::Jj {
            repository: cli.repository,
        },
        remote: cli.remote,
        repo_override: cli.repo,
        by_bookmark: cli.by_bookmark,
    };
    match cli.command {
        Command::Status { revision } => cmd::status::run(&ctx, revision.as_deref()),
        Command::Submit {
            revision,
            draft,
            dry_run,
            update_description,
            reviewers,
        } => cmd::submit::run(
            &ctx,
            &cmd::submit::Args {
                revset: revision,
                draft,
                dry_run,
                update_description,
                reviewers,
            },
        ),
        Command::Sync { revision, submit } => cmd::sync::run(
            &ctx,
            &cmd::sync::Args {
                revset: revision,
                submit,
            },
        ),
        Command::Merge {
            revision,
            method,
            force,
            submit,
        } => cmd::merge::run(
            &ctx,
            &cmd::merge::Args {
                revset: revision,
                method,
                force,
                submit,
            },
        ),
        Command::Pr(PrCommand::Open { rev, print }) => cmd::pr::open(&ctx, rev.as_deref(), print),
        Command::Pr(PrCommand::Checkout { number }) => cmd::pr::checkout(&ctx, number),
        Command::Auth => {
            let client = ctx.client()?;
            println!("host:   {}", client.repo.host);
            println!("repo:   {}", client.repo.slug());
            println!("source: {}", client.token_source());
            println!("user:   {}", client.viewer_login()?);
            Ok(())
        }
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "jj-gh", &mut std::io::stdout());
            Ok(())
        }
    }
}
