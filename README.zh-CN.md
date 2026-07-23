<div align="center">

# Morrow

**本地优先的编码 Agent —— 集 CLI、交互式 REPL、Web 仪表盘和桌面应用于一体，兼容任意 OpenAI 风格 API。**

[![Release](https://img.shields.io/github/v/release/catDforD/morrow?style=flat-square)](https://github.com/catDforD/morrow/releases)
[![License](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange?style=flat-square)](Cargo.toml)

[English](README.md) · **简体中文**

![Morrow Web 仪表盘](web_design/dashboard_v2.png)

</div>

Morrow 以流式方式输出模型结果，按项目持久化会话，可以读写文件、应用补丁、在明确授权下执行 shell 命令，并能输出 JSONL 事件用于自动化。所有能力都运行在你自己的 OpenAI 兼容 Chat Completions 端点上。

## 功能特性

- **一个运行时，多种形态** —— CLI 单次执行、交互式 REPL、本地浏览器仪表盘，以及 Tauri 2 桌面应用。
- **自带模型** —— 通过 `--config`、本地 `morrow.toml` 或 `~/.morrow/config.toml` 配置 OpenAI 兼容模型；Web 端可独立管理服务商，并按会话选择模型与推理档位。
- **持久化会话** —— 按项目划分的命名会话，支持列表、重命名、导出和续接。
- **真实工具** —— 文件读写、补丁、搜索、目录列举与 shell 命令。
- **权限档案** —— 只读、工作区写入、完全访问；shell 单独控制。
- **MCP 支持** —— 通过 TOML 或仪表盘配置 stdio 与 Streamable HTTP MCP 服务器。
- **只读 Subagent** —— 委派隔离的工作区调研任务，可并行处理相互独立的问题。
- **长会话友好** —— 自动上下文压缩。
- **可脚本化** —— JSONL 事件输出，便于自动化与集成。

## 安装

### 桌面应用（早期体验）

Tauri 2 桌面应用与 `morrow server` 共用同一套仪表盘和本地 Agent 运行时，但不会安装 `morrow` CLI。

从 [GitHub Releases](https://github.com/catDforD/morrow/releases) 下载对应平台的安装包：

| 平台 | 安装包 |
| --- | --- |
| Windows 10 22H2 / Windows 11 x64 | `Morrow_<version>_x64-setup.exe` |
| macOS 14+（Apple Silicon） | `Morrow_<version>_aarch64.dmg` |
| macOS 14+（Intel） | `Morrow_<version>_x64.dmg` |

早期构建未做正式签名与公证，请只从本项目的 GitHub Release 页面下载。Windows 若被 SmartScreen 拦截，确认来源后选择 **仍要运行**；macOS 首次启动可在 Finder 中右键 **打开**，或在 **系统设置 → 隐私与安全性** 中允许。

应用会恢复最近一次工作区，并提供 **File → Open Folder** / **Open Recent**。更新需手动进行（**Help → Download Latest Version** 或访问 Releases）。模型、MCP、命令与会话等数据保留在 `~/.morrow`。

### CLI

macOS 和 Linux：

```bash
curl -fsSL https://raw.githubusercontent.com/catDforD/morrow/main/install.sh | sh
morrow init
```

安装指定版本或自定义目录：

```bash
MORROW_VERSION=v0.1.0 curl -fsSL https://raw.githubusercontent.com/catDforD/morrow/main/install.sh | sh
MORROW_INSTALL_DIR=/usr/local/bin curl -fsSL https://raw.githubusercontent.com/catDforD/morrow/main/install.sh | sh
```

Windows 可从 GitHub Releases 下载 `morrow-x86_64-pc-windows-msvc.zip`，将 `morrow.exe` 与 `morrow-rg.exe` 解压到同一目录并加入 `PATH`。

从源码安装：

```bash
cargo install --git https://github.com/catDforD/morrow --locked -p agent-cli
```

## 快速上手

```bash
morrow "summarize this repository"   # 单次提示
morrow                               # 交互模式
morrow server                        # 本地 Web 仪表盘
```

仪表盘默认监听 `127.0.0.1:3000`，使用当前工作区、配置、会话与权限。它是本地优先且无鉴权的，不要绑定到公网。可用 `morrow server --host 127.0.0.1 --port 3000` 自定义地址。

仪表盘按 turn 独立选择权限（默认 `workspace_write`，并记住浏览器端最近一次选择）；`morrow.toml` 中的 `[permissions]` 仅作用于 CLI。

## 配置

```bash
morrow init
```

写入 `~/.morrow/config.toml` 并提示输入 API key。生成的 key 以内联方式保存在配置中，请勿提交。可用 `morrow init --template` 生成不含真实 key 的模板，或 `morrow init --force` 覆盖已有配置。

配置查找顺序：`--config` → 当前目录 `morrow.toml` → `~/.morrow/config.toml`。

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

内联的 `[model].OPENAI_API_KEY` 优先；否则读取 `api_key_env`（默认 `OPENAI_API_KEY`）。CLI 需要有效的模型与 API key；未传 `--config` 时，`morrow server` 即使没有配置也能启动，以便在浏览器中配置第一个服务商。

Web 端的模型、MCP 服务器与自定义命令分别在 **Settings → Models / MCP Servers / Commands** 中管理，数据保存在 `~/.morrow/` 下，不影响 CLI 的 TOML 配置。更多示例见 [`morrow.example.toml`](morrow.example.toml)。

### MCP 工具

可在配置中注册 stdio 与 Streamable HTTP MCP 服务器，发现后的工具以 `mcp__server__tool` 形式暴露给模型：

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

MCP 工具视为显式配置的受信工具，启用前请检查服务器命令与远程端点。

### 只读 Subagent

支持工具调用的模型会获得 `delegate_task`，用于隔离的工作区调研。Subagent 只能读取/搜索/列举文件，不能改文件、跑 shell、调 MCP 或继续派生。每个父 turn 最多 4 个并行任务，单任务超时 5 分钟。身份名单可在 **Settings → Subagents** 中管理（`~/.morrow/subagents.json`）。

### Web 自定义命令

**Settings → Commands** 管理 `~/.morrow/commands/*.md` 中的斜杠命令（仅 Web 可用）。在输入框键入 `/` 可搜索；`$ARGUMENTS` 会被替换为传入参数。

## 权限

| `permissions.mode` | 行为 |
| --- | --- |
| `read_only` | 拒绝写入类工具 |
| `workspace_write` | 文件修改需批准，且限制在工作区内 |
| `danger_full_access` | 可访问工作区外路径 |

| `permissions.shell` | 行为 |
| --- | --- |
| `deny` | 拒绝 shell |
| `prompt` | shell 需批准 |
| `allow` | shell 直接执行 |

默认配置为 `read_only` + `shell = "deny"`。单次运行可覆盖：

```bash
morrow --permission workspace-write "update the README"
morrow --allow-shell "run the test suite and explain failures"
```

## 会话

会话按项目保存在 `~/.morrow/sessions/`：

```bash
morrow --session work "continue the refactor"
morrow session list
morrow session show work
morrow session export work --output work-session.json
morrow session rename work backend-refactor
morrow session delete backend-refactor
```

REPL 常用命令：`/status`、`/permissions ...`、`/compact`、`/reset`、`/exit`。兼容别名 `--thread` / `--reset-thread` 仍可用，新用法请优先 `--session`。

## 自动化

```bash
morrow --jsonl "inspect this crate" > events.jsonl
```

JSONL 模式要求提供提示词，不可用于交互模式或 session 子命令。

## 开发

crate 边界、turn 生命周期与扩展点见 [`ARCHITECTURE.md`](ARCHITECTURE.md)。

| Crate | 职责 |
| --- | --- |
| `agent-cli` | CLI、REPL、JSONL、server 与配置装配 |
| `agent-desktop` | Tauri 2 桌面外壳与本地 server 生命周期 |
| `agent-config` | 配置加载 |
| `agent-core` | Turn 执行与事件流 |
| `agent-model` | OpenAI 兼容客户端与流式解析 |
| `agent-protocol` | 共享协议类型 |
| `agent-runtime` | 会话、压缩、工作区与 turn 辅助 |
| `agent-server` | HTTP/WebSocket 与内嵌仪表盘 |
| `agent-sandbox` | 权限判定 |
| `agent-tools` | 内置文件与 shell 工具 |

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets

cargo run -p agent-cli -- "hello"
cargo run -p agent-cli -- server
```

Web 前端开发：

```bash
cd crates/agent-server/web && pnpm install && pnpm dev
# 另开终端：cargo run -p agent-cli -- server
```

桌面开发：

```bash
pnpm --dir crates/agent-server/web install
pnpm --dir crates/agent-desktop install
pnpm --dir crates/agent-desktop dev
```

原生安装包需在目标 OS 上构建，并将匹配的 ripgrep 二进制放入 `crates/agent-desktop/src-tauri/binaries/` 后执行 `pnpm --dir crates/agent-desktop build:windows` 或 `build:macos`。打与 workspace 版本一致的 tag（如 `v0.3.0`）会触发 GitHub Actions 发布 CLI 与桌面安装包。

## 卸载

删除 CLI 二进制或卸载桌面应用即可。本地数据会有意保留：

```bash
rm -f ~/.local/bin/morrow
rm -rf ~/.morrow
```

## 许可证

[MIT](LICENSE) © 2026 Gargantua
