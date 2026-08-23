<div align="center">

# Laudacode

**A fast, lightweight AI coding agent for your terminal.**
Pure Rust, no Node.js, tiny binary, built for Termux.

[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange)](https://rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Termux%20%7C%20Linux%20%7C%20macOS-green)]()

</div>

---

Laudacode is an AI agent that lives in your terminal. It reads your project,
edits files, runs commands and fetches web docs — all under your approval —
using **any OpenAI-compatible API**: OpenAI, OpenRouter, Groq, DeepSeek,
Together, Ollama, LM Studio, llama.cpp server, vLLM…

## Features

- ⚡ **Pure Rust + Tokio + reqwest (rustls)** — one small static-ish binary, perfect for Android/Termux
- 🔌 **Any OpenAI-compatible endpoint** — custom `base_url` / `api_key` / `model`
- 🧠 **Agentic tool loop** — `list_dir`, `read_file`, `write_file`, `edit_file`, `run_command`, `fetch_url`
- 🌐 **Web fetch built in** — the agent can pull documentation from the internet
- 🛡️ **Approval modes** — `suggest`, `auto-edit`, `full-auto` (+ hard confirmation for dangerous commands)
- 📡 **Streaming responses** with reasoning-model support (dimmed "thinking" indicator)
- 💬 **Slash commands** — `/provider add|list|use|edit|rm|show`, `/model`, `/mode`, `/diff`, `/compact`, `/init`, `/context`, `/status`, `/clear`
- 📄 **AGENTS.md support** — project instructions auto-loaded into context (`/init` generates one)
- 💾 **Session persistence** — autosaved; resume with `--continue`
- 🖥️ **One-shot mode** — `laudacode exec "fix the failing test"`

## Install

### Termux / Android

```sh
pkg update && pkg install rust git -y
git clone https://github.com/Anon4You/Laudacode.git
cd Laudacode
cargo build --release
cp target/release/laudacode $PREFIX/bin/
```

> Building on low-RAM phones? Reduce codegen pressure:
> `CARGO_PROFILE_RELEASE_LTO=off cargo build --release`

### Linux / macOS

```sh
cargo install --locked --git https://github.com/Anon4You/Laudacode
# or from a clone:
cargo install --locked --path .
```

## Quick start

```sh
export OPENAI_API_KEY="sk-or-v1-..."          # any OpenAI-compatible key
export OPENAI_BASE_URL="https://openrouter.ai/api/v1"
export OPENAI_MODEL="stealth/ox-alpha"

laudacode                                     # interactive REPL
```

Or skip env vars entirely and use providers:

```sh
laudacode provider add                        # guided setup (name, url, key, model)
laudacode provider list
laudacode provider use openrouter
laudacode
```

One-shot tasks:

```sh
laudacode exec "explain what this repo does"
laudacode exec "add input validation to src/main.rs" --mode full-auto
```

## Configuration

Precedence: **CLI flags > environment variables > config file**.

Environment variables:

| Variable         | Meaning                    |
|------------------|----------------------------|
| `OPENAI_API_KEY` | API key                    |
| `OPENAI_BASE_URL`| e.g. `https://api.groq.com/openai/v1` |
| `OPENAI_MODEL`   | model name                 |

Config file at `~/.config/laudacode/config.toml`
(or `.json`; override location with `LAUDACODE_CONFIG`):

```toml
active_provider = "openrouter"
approval_mode   = "suggest"

[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
api_key  = "sk-or-v1-..."
model    = "stealth/ox-alpha"

[providers.openrouter.headers]        # optional custom headers
"HTTP-Referer" = "https://github.com/Anon4You/Laudacode"
"X-Title"      = "Laudacode"
```

See [`config.example.toml`](config.example.toml) for presets (OpenAI,
OpenRouter, Groq, DeepSeek, Ollama, LM Studio).

## Approval modes

| Mode         | File edits    | Shell commands | Dangerous commands |
|--------------|---------------|----------------|--------------------|
| `suggest`    | ask           | ask            | ask                |
| `auto-edit`  | ✅ auto       | ask            | ask                |
| `full-auto`  | ✅ auto       | ✅ auto        | ask (always)       |

You can also answer `[a]always` on any prompt to auto-approve the rest of the
session.

## CLI reference

```
laudacode                          # interactive session
laudacode "quick question"         # one-shot prompt
laudacode exec "<task>"            # same as above
laudacode -P groq -m llama-3.3-70b-versatile
laudacode --base-url http://localhost:11434/v1 --api-key ollama --model qwen2.5-coder:7b
laudacode -c                       # continue last session
laudacode provider add|list|use|edit|remove <name>
```

## Slash commands

| Command              | Description                                  |
|----------------------|----------------------------------------------|
| `/help`              | command overview                             |
| `/provider …`        | manage providers (`add list use edit rm show`)|
| `/model <name>`      | switch model                                 |
| `/mode <m>`          | suggest / auto-edit / full-auto              |
| `/status`            | current provider/model/session               |
| `/context [file…]`   | attach file(s), or print project tree        |
| `/diff`              | git diff of working tree                     |
| `/init`              | generate AGENTS.md for this project          |
| `/compact`           | summarize history to free context window     |
| `/clear`             | fresh conversation                           |
| `/save`              | persist session                              |
| `/exit`              | quit                                         |

## Why Rust?

Typical coding agents drag in Node.js and hundreds of megabytes of runtime.
On Android that is painful. Laudacode compiles to a small native executable
(~3–8 MB stripped) with zero runtime dependencies — instant startup, minimal
battery and RAM usage.

## License

MIT — see [LICENSE](LICENSE).

<div align="center"><sub>Built with ⚡ by Anon4You</sub></div>
