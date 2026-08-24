<div align="center">

# Morrow

**A local-first coding agent — CLI, interactive REPL, web dashboard, and desktop app, backed by any OpenAI-compatible API.**

[![Release](https://img.shields.io/github/v/release/catDforD/morrow?style=flat-square)](https://github.com/catDforD/morrow/releases)
[![License](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange?style=flat-square)](Cargo.toml)

**English** · [简体中文](README.zh-CN.md)

![Morrow web dashboard](web_design/dashboard_v2.png)

</div>

Morrow reads and edits files, applies patches, runs shell commands behind explicit permissions, streams model output, and persists project-scoped sessions — all against your own OpenAI-compatible Chat Completions endpoint.

## Features

- **Several faces, one runtime** — CLI one-shots, interactive REPL, local web dashboard, and Tauri 2 desktop app.
- **Bring your own model** — any OpenAI-compatible endpoint, configured per provider and per session.
- **Real tools** — file reads/edits, patches, search, directory listing, and shell commands.
- **Permission profiles** — read-only, workspace-write, and full-access modes, with shell controlled separately.
- **Policy hooks** — trusted command hooks at `before_prompt`, `before_tool`, `permission_request`, `after_tool`, `after_turn`, and compaction boundaries; project hooks require an explicit `morrow hooks trust`.
- **MCP support** — stdio and Streamable HTTP MCP servers.
- **Session-scoped subagents** — persistent `explore`, `plan`, `worker`, and `reviewer` instances running in the background.
- **Long-session friendly** — named, resumable sessions with automatic context compaction.
- **Scriptable** — JSONL event output for automation.

## Installation

### Desktop app (early access)

Download the installer from [GitHub Releases](https://github.com/catDforD/morrow/releases):

| Platform | Installer |
| --- | --- |
| Windows 10 22H2 / Windows 11 x64 | `Morrow_<version>_x64-setup.exe` |
| macOS 14+ (Apple Silicon) | `Morrow_<version>_aarch64.dmg` |
| macOS 14+ (Intel) | `Morrow_<version>_x64.dmg` |

Early builds are unsigned — download only from this project's Releases page, and confirm the OS security prompt on first launch. The desktop app bundles the same runtime as `morrow server` but does not include the CLI; settings and sessions live in `~/.morrow`.

### CLI

macOS and Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/catDforD/morrow/main/install.sh | sh
morrow init
```

Pin a version or install directory with `MORROW_VERSION` / `MORROW_INSTALL_DIR`. On Windows, download `morrow-x86_64-pc-windows-msvc.zip` from Releases, extract `morrow.exe` and `morrow-rg.exe` together, and add that directory to `PATH`.

From source:

```bash
cargo install --git https://github.com/catDforD/morrow --locked -p agent-cli
```

## Quick start

```bash
morrow "summarize this repository"   # one-shot
morrow                               # interactive REPL
morrow server                        # web dashboard on 127.0.0.1:3000
```

The dashboard is local-first — keep it bound to localhost. On startup it prints a one-time bootstrap URL that signs the browser in with an `HttpOnly` cookie; other local processes get `401`. Pass `--no-auth` to disable this for debugging, and `--permission-ceiling` (or `[server] permission_ceiling` in `morrow.toml`) to cap the permission mode the browser may pick per turn. `[permissions]` applies to the CLI only.

## Configuration

`morrow init` writes `~/.morrow/config.toml` and prompts for an API key. Lookup order: `--config` → `morrow.toml` in the current directory → `~/.morrow/config.toml`.

```toml
[model]
base_url = "https://api.openai.com/v1"
model = "gpt-4.1"
api_key_env = "OPENAI_API_KEY"

[permissions]
mode = "read_only"
shell = "deny"
```

An inline `OPENAI_API_KEY` wins when present; otherwise Morrow reads the `api_key_env` variable. Never commit a config containing a real key. Web-only settings (models, MCP servers, commands, subagents) are managed in the dashboard and stored under `~/.morrow/`. See [`morrow.example.toml`](morrow.example.toml) for the full set of options, including context compaction tuning.

### Project instructions

Morrow reads `AGENTS.md` from the workspace root and appends it to the system prompt for the main agent and all subagents. It cannot grant tool access beyond the active permission profile, and it is sent to your model provider — don't put secrets in it.

### MCP tools

Register stdio and Streamable HTTP MCP servers in config; their tools are exposed as `mcp__server__tool`:

```toml
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
enabled = true
```

MCP tools that the server does not mark with `readOnlyHint` require per-call approval by default; set `require_approval = false` on a server to opt out. Review server commands and endpoints before enabling them or disabling approval.

Use `[tools] allow` / `deny` in `morrow.toml` to restrict which tools the main agent sees at all. Entries match built-in tool names exactly, a whole MCP server (`mcp__filesystem`), or a prefix wildcard (`mcp__filesystem__*`); `deny` wins over `allow`, and an empty `allow` list allows everything. Skipped MCP tools are reported as startup diagnostics.

### Policy hooks

Command hooks run at the lifecycle boundaries above. User-level hooks live in `~/.morrow/hooks.toml`; project hooks live in `<workspace>/.morrow/hooks.toml` and are **disabled until you run `morrow hooks trust`** for that exact hook configuration (fingerprint-pinned, `morrow hooks revoke` to remove). Hooks execute with your user permissions, so review them like shell commands. Manage them with `morrow hooks list | trust | revoke`.

An `after_turn` hook runs when the model declares the turn complete, before the turn is accepted. It receives the final text and a turn summary, and answers `{"decision": "complete" | "continue" | "fail"}`: `continue` feeds `additional_context` back into the conversation for one more model call (at most 3 times per turn, then the turn completes with a warning), `fail` fails the turn with the given reason. For example, a verification gate that reruns the test suite:

```toml
[[hooks]]
id = "verify-tests"
event = "after_turn"
command = ["/bin/sh", "-c", "cargo test --workspace >/dev/null 2>&1 && printf '%s' '{\"decision\":\"complete\"}' || printf '%s' '{\"decision\":\"continue\",\"additional_context\":[\"cargo test is still red; fix the failures before finishing\"]}'"]
```

### Subagents

Web/Desktop sessions can spawn persistent background subagents (`spawn_subagent`, `send_subagent`, `wait_subagents`, …) and inspect, continue, cancel, or delete them from the Subagents inspector. A parent turn can end while its subagents keep running.

| Role | Built-in tools | Permission ceiling |
| --- | --- | --- |
| `explore` | Read, list, search | Read-only; shell denied |
| `plan` | Read, list, search | Read-only; shell denied |
| `worker` | File reads/writes, patches, shell | Workspace-write; shell always prompts |
| `reviewer` | Read, list, search, shell | No file writes; every shell command prompts |

Effective access is the intersection of the parent's permission profile, the role ceiling, and the role's tool allowlist; subagents never receive MCP or delegation tools. Each session keeps at most 8 instances and runs at most 4 concurrently. The synchronous, read-only `delegate_task` tool remains available everywhere (including the CLI) for quick one-off delegation. Per-role model, prompt, timeout, and identity settings live under **Settings → Subagents**.

### Web custom commands

**Settings → Commands** manages slash commands stored in `~/.morrow/commands/*.md`. Type `/` in the composer to search; `$ARGUMENTS` is replaced with the supplied args.

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

Defaults from `morrow init`: `read_only` + `shell = "deny"`. Override per run with `--permission` / `--allow-shell`:

```bash
morrow --permission workspace-write "update the README"
morrow --allow-shell "run the test suite and explain failures"
```

Shell policy is an approval boundary, not an OS sandbox — an approved command runs with your user permissions. Use an external sandbox when stronger isolation is required.

## Sessions

Named, project-scoped sessions persist under `~/.morrow/sessions/`:

```bash
morrow --session work "continue the refactor"
morrow --session work --reset-session "start over in the same project"
morrow session list
morrow session show work
morrow session export work --output work-session.json
morrow session rename work backend-refactor
morrow session delete backend-refactor
```

Useful REPL commands: `/status`, `/permissions ...`, `/compact`, `/reset`, `/exit`. The legacy `--thread` / `--reset-thread` aliases still work; prefer `--session` / `--reset-session`.

## Automation

```bash
morrow --jsonl "inspect this crate" > events.jsonl
```

JSONL mode requires a prompt and is unavailable in interactive mode or with session subcommands.

## Development

Crate boundaries, turn lifecycle, and extension points: [`ARCHITECTURE.md`](ARCHITECTURE.md).

<p align="center">
  <img src="docs/architecture/architecture-ports.svg" alt="Morrow architecture — core defines ports, adapters implement them" width="720">
</p>

| Crate | Responsibility |
| --- | --- |
| `agent-cli` | CLI, REPL, JSONL, session/hooks commands, server wiring |
| `agent-desktop` | Tauri 2 shell, embedded server lifecycle, WSL |
| `agent-config` | Config loading |
| `agent-core` | Turn execution, ports, middleware, event streams |
| `agent-eval` | Deterministic regression suite for the agent loop |
| `agent-hooks` | Command hooks and middleware adapters |
| `agent-model` | OpenAI-compatible client and streaming |
| `agent-protocol` | Shared protocol types |
| `agent-remote` | Desktop/WSL remote protocol and forwarding |
| `agent-runtime` | Sessions, compaction, workspace, turn helpers |
| `agent-server` | HTTP/WebSocket and embedded dashboard |
| `agent-sandbox` | Permission evaluation |
| `agent-tools` | Built-in file and shell tools |

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p agent-eval -- run   # agent loop regression suite

cargo run -p agent-cli -- "hello"
cargo run -p agent-cli -- server
```

Web dashboard (with the server running in a separate terminal):

```bash
cd crates/agent-server/web && pnpm install && pnpm dev
```

Desktop app:

```bash
pnpm --dir crates/agent-server/web install
pnpm --dir crates/agent-desktop install
pnpm --dir crates/agent-desktop dev
```

Tagging the workspace version (e.g. `v0.3.1`) triggers GitHub Actions to publish CLI archives and desktop installers.

## Uninstall

Remove the CLI binary or delete the desktop app; local state under `~/.morrow` is retained intentionally:

```bash
rm -f ~/.local/bin/morrow
rm -rf ~/.morrow   # sessions, config, and keys — only if you want them gone
```

## License

[MIT](LICENSE) © 2026 Gargantua
