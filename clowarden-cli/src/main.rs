#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::doc_markdown, clippy::similar_names)]

use std::{env, fs::File, path::PathBuf, sync::Arc};

use anyhow::{Result, format_err};
use clap::{Args, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use clowarden_core::{
    cfg::Legacy,
    directory,
    github::{GHApi, Source},
    multierror,
    services::{
        self, Change,
        github::{
            self, State,
            service::{Ctx, SvcApi},
        },
    },
};

/// Environment variable containing Github token.
const GITHUB_TOKEN: &str = "GITHUB_TOKEN";

#[derive(Parser)]
#[command(
    version,
    about = "CLOWarden CLI tool

This tool uses the GitHub API, which requires authentication. Please make sure
you provide a GitHub token (with repo and read:org scopes) by setting the
GITHUB_TOKEN environment variable."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Display changes between the actual state (as defined in the services)
    /// and the desired state (as defined in the configuration).
    Diff(BaseArgs),

    /// Generate configuration file from the actual state (experimental).
    Generate(GenerateArgs),

    /// Validate the configuration in the repository provided.
    Validate(BaseArgs),
}

#[derive(Args)]
struct BaseArgs {
    /// GitHub organization.
    #[arg(long)]
    org: String,

    /// Configuration repository.
    #[arg(long)]
    repo: String,

    /// Configuration repository branch.
    #[arg(long)]
    branch: String,

    /// Permissions file.
    #[arg(long, default_value = "config.yaml")]
    permissions_file: String,

    /// People file.
    #[arg(long)]
    people_file: Option<String>,
}

#[derive(Args)]
struct GenerateArgs {
    /// GitHub organization.
    #[arg(long)]
    org: String,

    /// Output file.
    #[arg(long)]
    output_file: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Setup logging
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("clowarden_cli=debug"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    // Check if required Github token is present in environment
    let Ok(github_token) = env::var(GITHUB_TOKEN) else {
        return Err(format_err!("{GITHUB_TOKEN} not found in environment"));
    };

    // Run command
    match cli.command {
        Command::Diff(args) => diff(args, github_token).await?,
        Command::Validate(args) => validate(args, github_token).await?,
        Command::Generate(args) => generate(args, github_token).await?,
    }

    Ok(())
}

/// Get changes between the actual state (service) and desired state (config).
async fn diff(args: BaseArgs, github_token: String) -> Result<()> {
    // GitHub

    // Setup services
    let (gh, svc) = setup_services(github_token);
    let legacy = setup_legacy(&args);
    let ctx = setup_context(&args.org);
    let src = setup_source(&args);

    // Get changes from the actual state to the desired state
    println!("Calculating diff between the actual state and the desired state...");
    let actual_state = State::new_from_service(svc.clone(), &ctx).await?;
    let desired_state = State::new_from_config(gh, svc, &legacy, &ctx, &src).await?;
    let changes = actual_state.diff(&desired_state);

    // Display changes
    println!("\n# GitHub");
    println!("\n## Directory changes\n");
    for change in changes.directory {
        println!("{}", change.template_format()?);
    }
    println!("\n## Repositories changes\n");
    for change in changes.repositories {
        println!("{}", change.template_format()?);
    }
    println!();

    Ok(())
}

/// Generate a configuration file from the actual state of the services.
///
/// NOTE: at the moment the configuration generated uses the legacy format for
/// backwards compatibility reasons.
async fn generate(args: GenerateArgs, github_token: String) -> Result<()> {
    #[derive(serde::Serialize)]
    struct LegacyCfg {
        teams: Vec<directory::legacy::sheriff::Team>,
        repositories: Vec<github::state::Repository>,
    }

    println!("Getting actual state from GitHub...");
    let (_, svc) = setup_services(github_token);
    let ctx = setup_context(&args.org);
    let actual_state = github::State::new_from_service(svc.clone(), &ctx).await?;

    println!("Generating configuration file and writing it to the output file provided...");
    let cfg = LegacyCfg {
        teams: actual_state.directory.teams.into_iter().map(Into::into).collect(),
        repositories: actual_state.repositories,
    };
    let file = File::create(&args.output_file)?;
    serde_yaml::to_writer(file, &cfg)?;

    println!("done!");
    Ok(())
}

/// Validate configuration.
async fn validate(args: BaseArgs, github_token: String) -> Result<()> {
    // GitHub

    // Setup services
    let (gh, svc) = setup_services(github_token);
    let legacy = setup_legacy(&args);
    let ctx = setup_context(&args.org);
    let src = setup_source(&args);

    // Validate configuration and display results
    println!("Validating configuration...");
    match github::State::new_from_config(gh, svc, &legacy, &ctx, &src).await {
        Ok(_) => println!("Configuration is valid!"),
        Err(err) => {
            println!("{}\n", multierror::format_error(&err)?);
            return Err(format_err!("Invalid configuration"));
        }
    }

    Ok(())
}

/// Helper function to setup some services from the arguments provided.
fn setup_services(github_token: String) -> (Arc<GHApi>, Arc<SvcApi>) {
    let gh = GHApi::new_with_token(github_token.clone());
    let svc = services::github::service::SvcApi::new_with_token(github_token);

    (Arc::new(gh), Arc::new(svc))
}

/// Helper function to create a legacy config instance from the arguments.
fn setup_legacy(args: &BaseArgs) -> Legacy {
    Legacy {
        enabled: true,
        sheriff_permissions_path: args.permissions_file.clone(),
        cncf_people_path: args.people_file.clone(),
    }
}

/// Helper function to create a context instance for the organization provided.
fn setup_context(org: &str) -> Ctx {
    Ctx {
        inst_id: None,
        org: org.to_string(),
    }
}

/// Helper function to create a source instance from the arguments.
fn setup_source(args: &BaseArgs) -> Source {
    Source {
        inst_id: None,
        owner: args.org.clone(),
        repo: args.repo.clone(),
        ref_: args.branch.clone(),
    }
}
