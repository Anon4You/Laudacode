# AGENTS.md — instructions for AI coding agents working on Laudacode

## Project overview

Laudacode is a terminal-based AI coding agent written in pure Rust.
It talks to any OpenAI-compatible API (OpenAI, OpenRouter,
Groq, DeepSeek, Ollama/LM Studio, …) and can read/write/edit files, run shell
commands and fetch web pages inside the working directory.

Primary target platform is **Termux/Android** — keep dependencies minimal,
avoid OpenSSL (use rustls), avoid glibc-only code.

## Build & test

```sh
cargo build            # debug build
cargo build --release  # optimized binary at target/release/laudacode
cargo clippy -- -D warnings
```

There is no test suite yet; verify changes with a manual live run:

```sh
export OPENAI_BASE_URL="https://openrouter.ai/api/v1"
export OPENAI_API_KEY="<key>"
export OPENAI_MODEL="stealth/ox-alpha"
laudacode exec "list the files in this directory"
```

## Architecture

| File             | Responsibility                                                        |
|------------------|-----------------------------------------------------------------------|
| `src/main.rs`    | entry point, CLI dispatch, profile merging, provider subcommand handling |
| `src/cli.rs`     | clap definitions (`--profile`, `-i/--image`, `--json`)                |
| `src/config.rs`  | config load/save (TOML or JSON), env precedence, profiles, provider resolution |
| `src/permissions.rs` | allow/ask/deny rule engine (wildcards per tool) gating tool execution |
| `src/provider`   | *(managed through `config.rs` + `repl.rs` helpers)*                   |
| `src/api.rs`     | OpenAI-compatible chat client, SSE streaming, tool-call accumulation, vision multipart messages |
| `src/agent.rs`   | agent loop (LLM ↔ tools), approval modes, system prompt, /compact     |
| `src/tools.rs`   | tool schemas + execution: list_dir, read_file, write_file, edit_file, apply_patch, run_command, fetch_url, grep, glob, update_plan |
| `src/diff.rs`    | dependency-free unified-diff engine (colored edit previews everywhere) |
| `src/agents.rs`  | specialist sub-agent registry (`delegate` tool) + concurrent sub-agent runner |
| `src/patch.rs`   | V4A patch parser/applier (`*** Begin Patch` format)                   |
| `src/session.rs` | conversation persistence (~/.local/share/laudacode/sessions), resume by unique id |
| `src/repl.rs`    | interactive REPL, slash commands, streaming UI, provider flows        |
| `src/theme.rs`   | 12 built-in color themes; one palette drives TUI, markdown, syntax and banner gradient |
| `src/effects.rs` | ambient particle effects in the banner band (petals/rain/lightning/…), xorshift PRNG, auto-pauses while streaming |
| `src/markdown.rs`| assistant markdown → styled transcript lines (fences highlighted via `syntax.rs`) |
| `src/syntax.rs`  | dependency-free syntax highlighter feeding code blocks and diff views  |

Precedence rules (do not break): **CLI flags > profile > env vars
(`OPENAI_API_KEY`, `OPENAI_BASE_URL`, `OPENAI_MODEL`) > config file**.

## Conventions

- Edition 2021, no async trait crates; keep it dependency-light.
- Errors: `anyhow::Result` everywhere; user-facing errors get context.
- Never commit real API keys — the `.gitignore` excludes local config files.
- Shell commands execute via `sh -c`; classify danger in
  `tools::is_dangerous_command` before touching approval logic.
- Write containment is symlink-aware (`tools::contained_in_workspace*`);
  keep it that way when touching path handling.
- Tool outputs are truncated (`MAX_TOOL_OUTPUT`) — respect those limits.
- Never hardcode colors — pull from `theme::get()` so `/theme` recolors everything.
