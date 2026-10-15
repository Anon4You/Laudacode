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

Run `cargo test` for the automated suite. Network-dependent tests are ignored by default. Also verify changes with a manual live run:

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
| `src/cli.rs`     | clap definitions (`--profile`, `-i/--image`, `--json`, `--quiet`)   |
| `src/config.rs`  | config load/save (TOML or JSON), env precedence, profiles, provider resolution |
| `src/permissions.rs` | allow/ask/deny rule engine (wildcards per tool) gating tool execution |
| `src/provider`   | *(managed through `config.rs` + `repl.rs` helpers)*                   |
| `src/api.rs`     | OpenAI-compatible chat client, SSE streaming, tool-call accumulation, vision multipart messages |
| `src/agent.rs`   | agent loop (LLM ↔ tools), approval modes, system prompt, /compact     |
| `src/tools.rs`   | tool schemas + execution: list_dir, read_file, write_file, edit_file, apply_patch, run_command, fetch_url, grep, glob, git, update_plan, start_process, poll_process, write_process, stop_process |
| `src/processes.rs` | `ProcessManager`: long-lived processes across tool calls (pipes, own process group via setpgid, bounded 8 KiB output, stdin + EOF, 1 s stdin timeout, max 16, monotonic ids, group-kill on stop/drop) |
| `src/lsp.rs`   | stdio LSP client (framing, document sync, diagnostics, definitions/references/symbols/hover), per-extension server registry |
| `src/mcp.rs`   | stdio MCP client: JSON-RPC framing, `initialize` handshake, paged `tools/list`, `tools/call`, qualified `mcp__<server>__<tool>` names, per-server timeouts, notification/stderr capture, name-collision rejection |
| `src/diff.rs`    | dependency-free unified-diff engine (colored edit previews everywhere) |
| `src/agents.rs`  | specialist sub-agent registry (`delegate` tool) + concurrent sub-agent runner |
| `src/patch.rs`   | V4A patch parser/applier (`*** Begin Patch` format)                   |
| `src/session.rs` | conversation persistence (~/.local/share/laudacode/sessions), resume by unique id |
| `src/repl.rs`    | interactive REPL, slash commands, streaming UI, provider flows        |
| `src/theme.rs`   | 17 built-in color themes; one palette drives TUI, markdown, syntax and banner gradient |
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
- The `git` tool is local-only and allowlisted in three tiers
  (`GIT_READ`/`GIT_WRITE`/`GIT_ALWAYS_ASK`); no `push`/`fetch`/`reset` —
  those go through `run_command` so the shell detector still sees them.
  `checkout`/`stash` are `Danger::High` and always ask, even in full-auto.
  Git writes are excluded from `/undo` and post-edit hooks: which files they
  touched is only knowable by diffing against HEAD.
- Write containment is symlink-aware (`tools::contained_in_workspace*`);
  keep it that way when touching path handling.
- Tool outputs are truncated (`MAX_TOOL_OUTPUT`) — respect those limits.
- MCP is stdio-only and must degrade, never abort: a server that fails to
  connect is reported via `/mcp` and skipped, and the toolset for every other
  tool is unaffected. Connect servers on the *agent's* runtime — the stderr
  drain is a spawned task and a throwaway runtime cancels it.
- LSP servers are configured per extension (`[lsp_servers.*].filetypes` maps
  extension → LSP `languageId`); never guess a `languageId` from the
  extension, it is server-specific. Two servers claiming one extension is a
  load-time error.
- LSP framing is `Content-Length` over stdio — *not* the newline framing
  `mcp.rs` uses. `Lsp::notify` is the only writer, so the header cannot drift
  from the body length.
- Diagnostics are **asynchronous**. A first `publishDiagnostics` of zero
  problems does not mean the file is clean: `rust-analyzer` republishes
  `0 → 1 → 0 → 2` as its index settles, ~26s cold. Wait at most
  `diagnostics_secs` for quiescence and report "still analysing" when the
  budget runs out. Never render an unsettled result as clean — there is a
  test that asserts exactly that.
- `position_encoding` and `diagnostic_provider` need explicit serde renames
  (`positionEncoding`, `diagnosticProvider`); without them a server that
  speaks UTF-8 or inter-file diagnostics is silently mis-handled.
- Tool-facing lines/columns are **1-based**; the wire is **0-based UTF-16**.
  Convert in `to_lsp`, count characters in `to_byte_offset`. Mixing the two
  counters mis-places every position after an emoji.
- MCP tools are third-party code with uninspectable effects: no
  danger-based auto-approval, consent in BUILD/PLAN, silent only in FULL
  AUTO, overridable via `[permission.mcp]`. `plan = true` on a server is an
  explicit opt-in to PLAN mode, never a default.
- `Config::save` rewrites the whole file from the struct, so every section
  needs a `Config` field or it is silently deleted. There is a round-trip
  test per section for exactly this reason.
- Never hardcode colors — pull from `theme::get()` so `/theme` recolors everything.
