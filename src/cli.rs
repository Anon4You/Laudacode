use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "laudacode",
    version,
    about = "Fast AI coding agent for your terminal — pure Rust.",
    after_help = "Examples:\n  laudacode\n  laudacode exec \"explain the build error\"\n  laudacode -P openrouter -m anthropic/claude-3.7-sonnet\n  laudacode provider add groq"
)]
pub struct Cli {
    /// Provider name from your config
    #[arg(short = 'P', long, global = true)]
    pub provider: Option<String>,

    /// Model override
    #[arg(short = 'm', long, global = true)]
    pub model: Option<String>,

    /// OpenAI-compatible base URL override
    #[arg(long, global = true)]
    pub base_url: Option<String>,

    /// API key override
    #[arg(long, global = true)]
    pub api_key: Option<String>,

    /// Approval mode: suggest | auto-edit | full-auto
    #[arg(long, global = true)]
    pub mode: Option<String>,

    /// Shorthand for --mode full-auto
    #[arg(short = 'y', long, global = true)]
    pub full_auto: bool,

    /// Continue the most recent session
    #[arg(short = 'c', long, global = true)]
    pub continue_last: bool,

    /// Resume a saved session by its unique id or assigned name
    #[arg(
        long,
        global = true,
        value_name = "SESSION_ID",
        conflicts_with = "continue_last"
    )]
    pub resume: Option<String>,

    /// Activate a named profile from your config ([profiles.<name>])
    #[arg(long, global = true)]
    pub profile: Option<String>,

    /// Attach an image (png/jpg/jpeg/webp/gif) to the prompt — repeatable
    #[arg(short = 'i', long, global = true)]
    pub image: Vec<String>,

    /// One-shot prompt: run non-interactively and exit
    #[arg(trailing_var_arg = true)]
    pub prompt: Vec<String>,

    /// Emit JSON event lines instead of prose (non-interactive runs)
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run a single task non-interactively and exit
    Exec {
        /// The task to run
        #[arg(trailing_var_arg = true)]
        prompt: Vec<String>,
    },

    /// Manage API providers
    Provider {
        #[command(subcommand)]
        cmd: ProviderCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum ProviderCmd {
    /// Add a new provider (interactive unless all flags are given)
    Add {
        name: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    /// List configured providers
    List,
    /// Make a provider active
    Use { name: String },
    /// Edit an existing provider
    Edit { name: String },
    /// Delete a provider
    Remove { name: String },
}
