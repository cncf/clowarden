#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::doc_markdown, clippy::similar_names)]

use anyhow::{format_err, Result};
use clap::{Args, Parser, Subcommand};
use clowarden_core::{
    cfg::Legacy,
    github::{GHApi, Source},
    multierror,
    services::{
        self,
        github::{
            self,
            service::{Ctx, SvcApi},
            State,
        },
        Change,
    },
};
use std::{env, sync::Arc};

#[derive(Parser)]
#[command(
    version,
    about = "CLOWarden CLI tool

This tool uses the Github API, which requires authentication. Please make sure
you provide a Github token (with repo and read:org scopes) by setting the
GITHUB_TOKEN environment variable."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate the configuration in the repository provided.
    Validate(BaseArgs),

    /// Display changes between the actual state (as defined in the services)
    /// and the desired state (as defined in the configuration).
    Diff(BaseArgs),
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

/// Environment variable containing Github token.
const GITHUB_TOKEN: &str = "GITHUB_TOKEN";

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Setup logging
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "clowarden_cli=debug");
    }
    tracing_subscriber::fmt::init();

    // Check if required Github token is present in environment
    let github_token = match env::var(GITHUB_TOKEN) {
        Err(_) => return Err(format_err!("{} not found in environment", GITHUB_TOKEN)),
        Ok(token) => token,
    };

    // Run command
    match cli.command {
        Command::Validate(args) => validate(args, github_token).await?,
        Command::Diff(args) => diff(args, github_token).await?,
    }

    Ok(())
}

/// Validate configuration.
async fn validate(args: BaseArgs, github_token: String) -> Result<()> {
    // GitHub

    // Setup services
    let (gh, svc) = setup_services(github_token);
    let legacy = setup_legacy(&args);
    let ctx = setup_context(&args);
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

/// Get changes between the actual state (service) and desired state (config).
async fn diff(args: BaseArgs, github_token: String) -> Result<()> {
    // GitHub

    // Setup services
    let (gh, svc) = setup_services(github_token);
    let legacy = setup_legacy(&args);
    let ctx = setup_context(&args);
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

/// Helper function to create a context instance from the arguments.
fn setup_context(args: &BaseArgs) -> Ctx {
    Ctx {
        inst_id: None,
        org: args.org.clone(),
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
