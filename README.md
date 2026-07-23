<div align="center">

# Morrow

**A local-first coding agent — CLI, interactive REPL, web dashboard, and desktop app, backed by any OpenAI-compatible API.**

[![Release](https://img.shields.io/github/v/release/catDforD/morrow?style=flat-square)](https://github.com/catDforD/morrow/releases)
[![License](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange?style=flat-square)](Cargo.toml)

**English** · [简体中文](README.zh-CN.md)

![Morrow web dashboard](web_design/dashboard_v2.png)

</div>

Morrow streams model output, persists project-scoped sessions, reads and edits files, applies patches, runs shell commands behind explicit permissions, and can emit JSONL events for automation. Everything runs against your own OpenAI-compatible Chat Completions endpoint.

## Features

- **Several faces, one runtime** — CLI one-shots, an interactive REPL, a local browser dashboard, and a Tauri 2 desktop app.
- **Bring your own model** — OpenAI-compatible config via `--config`, local `morrow.toml`, or `~/.morrow/config.toml`; Web-only provider management with per-session model and reasoning selection.
- **Persistent sessions** — named, project-scoped sessions you can list, rename, export, and resume.
- **Real tools** — file reads/edits, patches, search, directory listing, and shell commands.
- **Permission profiles** — read-only, workspace-write, and full-access modes, with shell controlled separately.
- **MCP support** — stdio and Streamable HTTP MCP servers from TOML or the dashboard.
- **Read-only subagents** — delegate isolated workspace investigations and run independent tasks in parallel.
- **Long-session friendly** — automatic context compaction.
- **Scriptable** — JSONL event output for automation and integrations.

## Installation

### Desktop app (early access)

The Tauri 2 desktop app uses the same dashboard and local agent runtime as `morrow server`. It does not install the `morrow` CLI.

Download the installer from [GitHub Releases](https://github.com/catDforD/morrow/releases):

| Platform | Installer |
| --- | --- |
| Windows 10 22H2 / Windows 11 x64 | `Morrow_<version>_x64-setup.exe` |
| macOS 14+ (Apple Silicon) | `Morrow_<version>_aarch64.dmg` |
| macOS 14+ (Intel) | `Morrow_<version>_x64.dmg` |

These early builds are not formally signed or notarized — download only from this project's Release page. On Windows, if SmartScreen blocks the installer, verify the source and choose **Run anyway**. On macOS, first launch via Finder **Open**, or allow it under **System Settings → Privacy & Security**.

The app restores the last workspace and offers **File → Open Folder** / **Open Recent**. Updates are manual (**Help → Download Latest Version** or GitHub Releases). Model settings, MCP settings, commands, and sessions remain in `~/.morrow`.

### CLI

macOS and Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/catDforD/morrow/main/install.sh | sh
morrow init
```

Specific version or custom install directory:

```bash
MORROW_VERSION=v0.1.0 curl -fsSL https://raw.githubusercontent.com/catDforD/morrow/main/install.sh | sh
MORROW_INSTALL_DIR=/usr/local/bin curl -fsSL https://raw.githubusercontent.com/catDforD/morrow/main/install.sh | sh
```

On Windows, download `morrow-x86_64-pc-windows-msvc.zip` from GitHub Releases, extract `morrow.exe` and `morrow-rg.exe` into the same directory, and put that directory on `PATH`.

Install from source:

```bash
cargo install --git https://github.com/catDforD/morrow --locked -p agent-cli
```

## Quick start

```bash
morrow "summarize this repository"   # one-shot
morrow                               # interactive
morrow server                        # local web dashboard
```

The dashboard listens on `127.0.0.1:3000` by default and uses the current workspace, config, sessions, and permissions. It is local-first and unauthenticated — do not bind it to a public interface. Customize with `morrow server --host 127.0.0.1 --port 3000`.

The dashboard picks permissions per turn (default `workspace_write`, remembering the latest browser choice); `[permissions]` in `morrow.toml` applies to CLI only.

## Configuration

```bash
morrow init
```

Writes `~/.morrow/config.toml` and prompts for an API key. The generated key is stored inline — treat it as private and do not commit it. Use `morrow init --template` for a template without a real key, or `morrow init --force` to overwrite.

Lookup order: `--config` → `morrow.toml` in the current directory → `~/.morrow/config.toml`.

```toml
[model]
base_url = "https://api.openai.com/v1"
model = "gpt-4.1"
api_key_env = "OPENAI_API_KEY"
timeout_secs = 120
context_window_tokens = 128000
reserved_output_tokens = 8192

[agent]
system_prompt = "You are a helpful assistant."

[context]
auto_compact = true
auto_compact_threshold = 0.835
retain_recent_turns = 6
summary_target_tokens = 12000
compact_max_retries = 2

[permissions]
mode = "read_only"
shell = "deny"
```

Inline `[model].OPENAI_API_KEY` wins when present; otherwise Morrow reads `api_key_env` (default `OPENAI_API_KEY`). CLI requires a valid model and API key. Without `--config`, `morrow server` can start with no config so the first provider can be set up in the browser.

Web-only models, MCP servers, and custom commands are managed under **Settings → Models / MCP Servers / Commands** and stored under `~/.morrow/`; they do not change the CLI TOML config. See [`morrow.example.toml`](morrow.example.toml) for more examples.

### MCP tools

Register stdio and Streamable HTTP MCP servers in config. Discovered tools are exposed as `mcp__server__tool`:

```toml
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
env = {}
cwd = "."
enabled = true
startup_timeout_sec = 10
tool_timeout_sec = 60
```

MCP tools are treated as explicitly trusted — review server commands and remote endpoints before enabling them.

### Read-only subagents

Tool-capable models get `delegate_task` for isolated workspace investigations. Subagents can only read/search/list files — no writes, shell, MCP, or further delegation. At most four run in parallel per parent turn, with a five-minute timeout each. Manage identities under **Settings → Subagents** (`~/.morrow/subagents.json`).

### Web custom commands

**Settings → Commands** manages slash commands in `~/.morrow/commands/*.md` (Web only). Type `/` in the composer to search; `$ARGUMENTS` is replaced with the supplied args.

## Permissions

| `permissions.mode` | Behavior |
| --- | --- |
| `read_only` | Write tools denied |
| `workspace_write` | File changes need approval and stay in the workspace |
| `danger_full_access` | File I/O may leave the workspace |

| `permissions.shell` | Behavior |
| --- | --- |
| `deny` | Shell denied |
| `prompt` | Shell needs approval |
| `allow` | Shell runs without a prompt |

Default from `morrow init`: `read_only` + `shell = "deny"`. Override for one run:

```bash
morrow --permission workspace-write "update the README"
morrow --allow-shell "run the test suite and explain failures"
```

## Sessions

Project-scoped sessions live under `~/.morrow/sessions/`:

```bash
morrow --session work "continue the refactor"
morrow session list
morrow session show work
morrow session export work --output work-session.json
morrow session rename work backend-refactor
morrow session delete backend-refactor
```

Useful REPL commands: `/status`, `/permissions ...`, `/compact`, `/reset`, `/exit`. Compatibility aliases `--thread` / `--reset-thread` still work; prefer `--session` for new usage.

## Automation

```bash
morrow --jsonl "inspect this crate" > events.jsonl
```

JSONL mode requires a prompt and is not available for interactive mode or session subcommands.

## Development

Crate boundaries, turn lifecycle, and extension points: [`ARCHITECTURE.md`](ARCHITECTURE.md).

| Crate | Responsibility |
| --- | --- |
| `agent-cli` | CLI, REPL, JSONL, server, config wiring |
| `agent-desktop` | Tauri 2 shell and local server lifecycle |
| `agent-config` | Config loading |
| `agent-core` | Turn execution and event streams |
| `agent-model` | OpenAI-compatible client and streaming |
| `agent-protocol` | Shared protocol types |
| `agent-runtime` | Sessions, compaction, workspace, turn helpers |
| `agent-server` | HTTP/WebSocket and embedded dashboard |
| `agent-sandbox` | Permission evaluation |
| `agent-tools` | Built-in file and shell tools |

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets

cargo run -p agent-cli -- "hello"
cargo run -p agent-cli -- server
```

Web dashboard:

```bash
cd crates/agent-server/web && pnpm install && pnpm dev
# separate terminal: cargo run -p agent-cli -- server
```

Desktop:

```bash
pnpm --dir crates/agent-server/web install
pnpm --dir crates/agent-desktop install
pnpm --dir crates/agent-desktop dev
```

Native installers must be built on the target OS. Place the matching ripgrep binary under `crates/agent-desktop/src-tauri/binaries/`, then run `pnpm --dir crates/agent-desktop build:windows` or `build:macos`. Tagging the exact workspace version (e.g. `v0.3.0`) triggers GitHub Actions to publish CLI archives and desktop installers.

## Uninstall

Remove the CLI binary or uninstall/delete the desktop app. Local private state is intentionally retained:

```bash
rm -f ~/.local/bin/morrow
rm -rf ~/.morrow
```

## License

[MIT](LICENSE) © 2026 Gargantua
