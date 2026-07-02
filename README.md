# Morrow

Morrow is a local coding agent CLI backed by an OpenAI-compatible chat completions API. It can stream model output, keep project-scoped sessions, read and edit files, apply patches, run shell commands with approval, and emit JSONL events for automation.

## Install

macOS and Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/catDforD/morrow/main/install.sh | sh
morrow init
```

Install a specific release:

```bash
MORROW_VERSION=v0.1.0 curl -fsSL https://raw.githubusercontent.com/catDforD/morrow/main/install.sh | sh
```

Install to a custom directory:

```bash
MORROW_INSTALL_DIR=/usr/local/bin curl -fsSL https://raw.githubusercontent.com/catDforD/morrow/main/install.sh | sh
```

Windows users can download `morrow-x86_64-pc-windows-msvc.zip` from GitHub Releases, extract `morrow.exe`, and put it on `PATH`.

From source:

```bash
cargo install --git https://github.com/catDforD/morrow --locked -p agent-cli
```

## Configure

Run:

```bash
morrow init
```

This writes `~/.morrow/config.toml`. By default it stores `[model].OPENAI_API_KEY` inline for the local machine. Treat this file as private and do not commit it.

To generate an editable template without entering a real key:

```bash
morrow init --template
```

To overwrite an existing config:

```bash
morrow init --force
```

Example config:

```toml
[model]
base_url = "https://api.openai.com/v1"
model = "gpt-4.1"
OPENAI_API_KEY = "replace-with-your-openai-api-key"
timeout_secs = 120

[permissions]
mode = "read_only"
shell = "deny"
```

## Use

Run one prompt in the current project:

```bash
morrow "summarize this repository"
```

Start interactive mode:

```bash
morrow
```

Useful REPL commands:

```text
/status
/permissions read-only
/permissions workspace-write
/compact
/reset
/exit
```

Use a named session:

```bash
morrow --session work "continue the refactor"
morrow session list
morrow session show work
morrow session export work --output work-session.json
morrow session rename work backend-refactor
morrow session delete backend-refactor
```

Emit machine-readable JSONL events:

```bash
morrow --jsonl "inspect this crate" > events.jsonl
```

## Permissions

Morrow has three file permission modes:

- `read_only`: file-write tools are denied.
- `workspace_write`: file changes require approval and are limited to the workspace.
- `danger_full_access`: file reads and writes may access paths outside the workspace.

Shell execution is controlled separately with `shell = "deny"`, `shell = "prompt"`, or `shell = "allow"`. The default `morrow init` config uses `read_only` and `shell = "deny"`.

## Development

```bash
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets
```

Release builds are created by tagging `v*` and letting GitHub Actions publish the platform archives plus `SHA256SUMS`.

## Uninstall

Remove the binary and, if desired, local private state:

```bash
rm -f ~/.local/bin/morrow
rm -rf ~/.morrow
```
