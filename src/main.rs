mod agent;
mod api;
mod cli;
mod config;
mod repl;
mod session;
mod tools;

use anyhow::{bail, Context, Result};
use clap::Parser;
use crossterm::style::Stylize;

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

use agent::ApprovalMode;
use cli::{Cli, Command, ProviderCmd};
use repl::App;
use session::Session;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("{} {e:#}", "✗".red().bold());
        std::process::exit(1);
    }
}

async fn run(mut cli: Cli) -> Result<()> {
    match cli.command.take() {
        Some(Command::Provider { cmd }) => return provider_cli(cmd),
        Some(Command::Exec { prompt }) => {
            if prompt.is_empty() {
                bail!("usage: laudacode exec \"<task>\"");
            }
            let app = build_app(&cli)?;
            app.run_once(&prompt.join(" ")).await
        }
        None => {
            if !cli.prompt.is_empty() {
                let task = cli.prompt.join(" ");
                let app = build_app(&cli)?;
                app.run_once(&task).await?;
                return Ok(());
            }
            interactive(&cli).await
        }
    }
}

fn mode_from_cli(cli: &Cli) -> Result<Option<ApprovalMode>> {
    if cli.full_auto {
        return Ok(Some(ApprovalMode::FullAuto));
    }
    match &cli.mode {
        Some(s) => match ApprovalMode::parse(s) {
            Some(m) => Ok(Some(m)),
            None => bail!("invalid mode '{s}' — use suggest | auto-edit | full-auto"),
        },
        None => Ok(None),
    }
}

fn build_app(cli: &Cli) -> Result<App> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let app = App::build(
        cwd,
        cli.provider.as_deref(),
        cli.base_url.as_deref(),
        cli.api_key.as_deref(),
        cli.model.as_deref(),
        mode_from_cli(cli)?,
    )?;
    Ok(app)
}

async fn interactive(cli: &Cli) -> Result<()> {
    let mut app = build_app(cli)?;
    if cli.continue_last {
        if let Some(sess) = Session::latest() {
            app.restore_session(sess);
        } else {
            println!("{}", "· no previous session found, starting fresh".dark_grey());
        }
    }
    app.run_repl().await
}

fn provider_cli(cmd: ProviderCmd) -> Result<()> {
    let mut cfg = config::Config::load()?;
    match cmd {
        ProviderCmd::Add { name, base_url, api_key, model } => {
            match (base_url, api_key, model) {
                (Some(b), Some(k), Some(m)) => {
                    let n = match name {
                        Some(n) => config::sanitize_name(&n)?,
                        None => bail!("--name required when using flags"),
                    };
                    cfg.providers.insert(
                        n.clone(),
                        config::Provider { base_url: b, api_key: k, model: m, headers: Default::default() },
                    );
                    cfg.active_provider = Some(n.clone());
                    cfg.save()?;
                    println!("· saved and activated provider '{}'", n.green());
                }
                _ => {
                    let n = repl::add_provider_flow(&mut cfg, name.as_deref())?;
                    cfg.active_provider = Some(n);
                    cfg.save()?;
                    println!("· activated");
                }
            }
        }
        ProviderCmd::List => repl::list_providers(&cfg),
        ProviderCmd::Use { name } => {
            if !cfg.providers.contains_key(&name) {
                bail!("provider '{name}' not found — list with: laudacode provider list");
            }
            cfg.active_provider = Some(name.clone());
            cfg.save()?;
            println!("· active provider is now '{}'", name.green());
        }
        ProviderCmd::Edit { name } => repl::edit_provider_flow(&mut cfg, &name)?,
        ProviderCmd::Remove { name } => {
            if cfg.providers.remove(&name).is_none() {
                bail!("provider '{name}' not found");
            }
            if cfg.active_provider.as_deref() == Some(name.as_str()) {
                cfg.active_provider = None;
            }
            cfg.save()?;
            println!("· removed '{name}'");
        }
    }
    Ok(())
}
