mod agent;
mod api;
mod cli;
mod config;
mod markdown;
mod patch;
mod repl;
mod session;
mod tools;
mod tui;

use anyhow::{bail, Context, Result};
use clap::Parser;
use crossterm::style::Stylize;

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

use agent::ApprovalMode;
use cli::{Cli, Command, ProviderCmd};
use config::{Config, Profile};
use repl::App;
use session::Session;

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("{} {e:#}", "✗".red().bold());
        std::process::exit(1);
    }
}

fn run(mut cli: Cli) -> Result<()> {
    match cli.command.take() {
        Some(Command::Provider { cmd }) => provider_cli(cmd)?,
        Some(Command::Exec { prompt }) => {
            if prompt.is_empty() {
                bail!("usage: laudacode exec \"<task>\"");
            }
            let app = build_app(&cli)?;
            // Propagate both runtime-build and task errors — swallowing them
            // here made every exec failure exit silently with status 0.
            repl::block_current(app.run_once(&prompt.join(" "), &load_images(&cli)?))??;
        }
        None => {
            if !cli.prompt.is_empty() {
                let task = cli.prompt.join(" ");
                let app = build_app(&cli)?;
                repl::block_current(app.run_once(&task, &load_images(&cli)?))??;
                return Ok(());
            }
            interactive(&cli)?
        }
    }
    Ok(())
}

/// Everything `App::build` needs after CLI flags and profiles are merged.
struct Overrides {
    provider: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    mode: Option<ApprovalMode>,
}

/// Merge `[profiles.<name>]` defaults underneath the CLI flags.
/// Precedence: CLI flags > profile > env > config (env is handled later
/// inside `Config::resolve_active`).
fn apply_profile(config: &Config, cli: &Cli) -> Result<Overrides> {
    let profile: Option<&Profile> = match cli.profile.as_deref() {
        Some(name) => Some(
            config
                .profiles
                .get(name)
                .with_context(|| format!("profile '{name}' not found in {}", Config::toml_path().display()))?,
        ),
        None => None,
    };
    let provider = cli.provider.clone().or_else(|| profile.and_then(|p| p.provider.clone()));
    let model = cli.model.clone().or_else(|| profile.and_then(|p| p.model.clone()));
    let mode = if cli.full_auto {
        Some(ApprovalMode::FullAuto)
    } else if let Some(s) = &cli.mode {
        Some(ApprovalMode::parse(s).with_context(|| {
            format!("invalid mode '{s}' — use suggest | auto-edit | full-auto")
        })?)
    } else {
        profile
            .and_then(|p| p.approval_policy.as_deref())
            .and_then(ApprovalMode::parse)
    };
    Ok(Overrides {
        provider,
        base_url: cli.base_url.clone(),
        api_key: cli.api_key.clone(),
        model,
        mode,
    })
}

/// Encode `-i/--image` attachments as data URIs for vision-capable models.
fn load_images(cli: &Cli) -> Result<Vec<String>> {
    let mut uris = Vec::new();
    for path in &cli.image {
        uris.push(repl::load_image_data_uri(&std::env::current_dir().unwrap_or_default(), path)?);
    }
    Ok(uris)
}

fn build_app(cli: &Cli) -> Result<App> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let config = Config::load()?;
    let ov = apply_profile(&config, cli)?;
    let mut app = App::build_with_config(
        cwd,
        config,
        ov.provider.as_deref(),
        ov.base_url.as_deref(),
        ov.api_key.as_deref(),
        ov.model.as_deref(),
        ov.mode,
    )?;
    app.ui.json_out = cli.json;
    // Interactive runs with -i pre-queue the attachments for the first prompt.
    if !cli.image.is_empty() && cli.prompt.is_empty() {
        app.pending_images = load_images(cli)?;
    }
    Ok(app)
}

fn interactive(cli: &Cli) -> Result<()> {
    // Interactive-first: never bail out on missing config. Build what we can,
    // run the onboarding wizard if needed, then hand over to the full-screen
    // TUI. Only a hard build failure (bad cwd etc.) exits here.
    let mut app = match build_app(cli) {
        Ok(app) => app,
        Err(e) => {
            // Config problems (no provider yet) should not kill the session;
            // fall back to a bare default and let the wizard fix it.
            eprintln!("{} {e:#}", "·".dark_grey());
            App::build_unconfigured(std::env::current_dir()?)?
        }
    };

    if cli.continue_last {
        if let Some(sess) = Session::latest() {
            app.restore_session(sess);
        } else {
            println!("{}", "· no previous session found, starting fresh".dark_grey());
        }
    }

    // First-run wizard before entering the full-screen UI.
    if app.needs_onboarding() {
        let name = repl::onboarding_wizard(&mut app.config)?;
        let active = app.config.resolve_active(Some(&name), None, None, None)?;
        app.agent.client = repl::rebuild_client(&active)?;
        app.agent.model = active.model.clone();
        app.active = active;
    }

    app.run_tui()
}


fn provider_cli(cmd: ProviderCmd) -> Result<()> {
    let mut cfg = Config::load()?;
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
                        config::Provider { base_url: b, api_key: k, model: m, headers: Default::default(), reasoning_effort: None },
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
