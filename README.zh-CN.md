<div align="center">

# Morrow

**本地优先的编码 Agent —— 集 CLI、交互式 REPL、Web 仪表盘和桌面应用于一体，兼容任意 OpenAI 风格 API。**

[![Release](https://img.shields.io/github/v/release/catDforD/morrow?style=flat-square)](https://github.com/catDforD/morrow/releases)
[![License](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange?style=flat-square)](Cargo.toml)

[English](README.md) · **简体中文**

![Morrow Web 仪表盘](web_design/dashboard_v2.png)

</div>

Morrow 以流式方式输出模型结果，按项目持久化会话，可以读写文件、应用补丁、在明确授权的权限下执行 shell 命令，并能输出 JSONL 事件用于自动化。所有能力都运行在你自己的 OpenAI 兼容 Chat Completions 端点上。

## 功能特性

- **一个运行时，三种形态** —— CLI 单次执行、交互式 REPL、本地浏览器仪表盘，以及 Tauri 2 桌面应用。
- **自带模型** —— 通过 `--config`、本地 `morrow.toml` 或 `~/.morrow/config.toml` 配置 OpenAI 兼容模型；Web 端可独立管理模型服务商，并按会话选择模型与推理档位。
- **持久化会话** —— 按项目划分的命名会话，支持列表、重命名、导出和续接。
- **真实工具** —— 文件读取、文件编辑、补丁应用、文本搜索、目录列举和 shell 命令。
- **权限档案** —— 只读、工作区写入、完全访问三档模式，shell 执行单独控制。
- **MCP 支持** —— 通过 TOML 或仪表盘配置 stdio 与 Streamable HTTP MCP 服务器。
- **长会话友好** —— 自动上下文压缩。
- **可脚本化** —— JSONL 事件输出，便于自动化与集成。

## 安装

### 桌面应用（早期体验）

Tauri 2 桌面应用与 `morrow server` 使用同一套仪表盘和本地 Agent 运行时。它是独立的分发版本，不会安装 `morrow` CLI。

从 [GitHub Releases](https://github.com/catDforD/morrow/releases) 下载对应平台的安装包：

| 平台 | 安装包 |
| --- | --- |
| Windows 10 22H2 / Windows 11 x64 | `Morrow_<version>_x64-setup.exe` |
| macOS 14+（Apple Silicon） | `Morrow_<version>_aarch64.dmg` |
| macOS 14+（Intel） | `Morrow_<version>_x64.dmg` |

这些早期构建未做正式的代码签名与公证，请只从本项目的 GitHub Release 页面下载。

- **Windows** —— 运行 NSIS 安装程序。如果 SmartScreen 拦截，选择 **更多信息**，确认安装程序来自本仓库的 Release 页面，然后选择 **仍要运行**。
- **macOS** —— 将 Morrow 拖入「应用程序」。首次启动时在 Finder 中右键 Morrow 并选择 **打开**；若仍被拦截，前往 **系统设置 → 隐私与安全性** 选择 **仍要打开**。

应用会恢复最近一次有效的工作区，并提供 **File → Open Folder** 与 **File → Open Recent**。在 Windows 上关闭窗口即退出；在 macOS 上关闭窗口会隐藏 Morrow，重新打开即可恢复，`Cmd+Q` 退出。

桌面版更新需手动进行，应用不会在后台检查更新。使用 **Help → Download Latest Version** 或访问 GitHub Releases，然后运行更新的 Windows 安装程序，或替换「应用程序」中的 macOS 应用。模型设置、MCP 设置、命令和项目会话在升级与正常卸载后仍保留在 `~/.morrow` 中。如需降级，请手动安装旧版本的 GitHub Release，应用内不提供回滚功能。

### CLI

macOS 和 Linux：

```bash
curl -fsSL https://raw.githubusercontent.com/catDforD/morrow/main/install.sh | sh
morrow init
```

安装指定版本：

```bash
MORROW_VERSION=v0.1.0 curl -fsSL https://raw.githubusercontent.com/catDforD/morrow/main/install.sh | sh
```

安装到自定义目录：

```bash
MORROW_INSTALL_DIR=/usr/local/bin curl -fsSL https://raw.githubusercontent.com/catDforD/morrow/main/install.sh | sh
```

Windows 用户可从 GitHub Releases 下载 `morrow-x86_64-pc-windows-msvc.zip`，将 `morrow.exe` 与 `morrow-rg.exe` 解压到同一目录，并把该目录加入 `PATH`。

从源码安装：

```bash
cargo install --git https://github.com/catDforD/morrow --locked -p agent-cli
```

## 快速上手

在当前项目中执行单条提示：

```bash
morrow "summarize this repository"
```

进入交互模式：

```bash
morrow
```

启动本地 Web 仪表盘：

```bash
morrow server
```

仪表盘默认监听 `127.0.0.1:3000`，使用当前工作区、配置、会话存储和权限档案。它是本地优先且无鉴权的；除非你自行加了网络层防护，否则不要绑定到公网接口。可用 `morrow server --host 127.0.0.1 --port 3000` 自定义监听地址。

仪表盘按 turn 独立选择权限，并记住最近一次浏览器端的选择，默认为 `workspace_write`；`morrow.toml` 中的 `[permissions]` 仅作用于 CLI。侧边栏还支持归档与恢复按项目划分的任务会话。

## 配置

创建用户配置：

```bash
morrow init
```

该命令写入 `~/.morrow/config.toml` 并提示输入 API key。生成的文件将 key 以内联方式存为 `[model].OPENAI_API_KEY`，请将其视为私密数据，不要提交。使用 `morrow init --template` 可生成一份不填写真实 key 的可编辑模板，使用 `morrow init --force` 可覆盖已有配置。

配置查找顺序：

1. `--config` 指定的路径。
2. 当前工作目录下的 `morrow.toml`。
3. `~/.morrow/config.toml`。

使用环境变量而非内联 key 的配置示例：

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

内联的 `[model].OPENAI_API_KEY` 存在时优先使用；否则 Morrow 读取 `api_key_env` 指定的环境变量（默认为 `OPENAI_API_KEY`）。

CLI 命令始终要求解析后的 TOML 配置中包含有效的模型和 API key。未传 `--config` 时，`morrow server` 更为宽松：即使没有配置文件、没有 `[model]` 段或没有模型 key 也能启动，以便在浏览器中配置第一个服务商。但显式指定的配置文件缺失、或 TOML 非法，仍会中止启动。

仪表盘的 **Settings → Models** 页面管理仅 Web 端可用的 OpenAI Chat Completions 兼容服务商，不影响 CLI 使用的模型。服务商数据存储在 `~/.morrow/web-models.json`；API key 以本地明文保存、API 永不返回，且在 Unix 上文件权限为 `0600`。TOML 中配置的有效模型会以只读服务商的形式出现，并在你另选默认之前作为 Web 端初始默认模型。

内置的 DeepSeek 模板提供 `deepseek-v4-flash` 与 `deepseek-v4-pro`，具备 1,000,000 token 上下文窗口、工具支持，以及 **Off / High / Max** 三档推理选择。新的浏览器会话继承全局默认，已有会话各自记住自己的模型与推理档位。

### MCP 工具

Morrow 可以在同一配置文件中注册 stdio 与 Streamable HTTP MCP 服务器。发现后的工具以 `mcp__server__tool` 的形式直接暴露给模型。配置未变化时，已初始化的服务器与已发现的工具会在 CLI/server 生命周期内缓存。

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

带环境变量请求头的 Streamable HTTP 示例见 [`morrow.example.toml`](morrow.example.toml)。OAuth、延迟搜索和按工具审批策略尚未实现。MCP 工具被视为显式配置的受信工具，启用前请检查服务器命令与远程端点。

仪表盘的 **Settings → MCP Servers** 页面可添加仅 Web 端可用的 stdio 与 HTTP 服务器，不影响 CLI 配置。Web 条目存储在 `~/.morrow/web-mcp.json`，并与从 `morrow.toml` 加载的只读服务器合并；重名会被拒绝。变更从下一个 Web turn 开始生效，正在运行的 turn 保持原有服务器快照。该页面支持导入直接的 JSON 服务器映射或 `mcpServers` 包装结构，并可在保存前测试草稿配置。

Web MCP 的环境变量与 HTTP 请求头值以本地明文保存，Unix 上权限为 `0600`，其值永不由设置 API 返回：留空表示保留原值，删除该行表示删除该值。测试或使用已配置的 MCP 服务器可能执行本地程序或访问远程服务。

### Web 自定义命令

仪表盘的 **Settings → Commands** 页面管理 `~/.morrow/commands/*.md` 中的用户命令。这些命令仅在 Web 聊天中可用；CLI 与 JSONL 输入保持原有行为。命令文件名即斜杠命令名，可包含小写 ASCII 字母、数字、`-` 和 `_`。

```md
---
description: "Review a target file"
argument-hint: "<file-path>"
---
Review $ARGUMENTS carefully.
```

在 Web 输入框中键入 `/` 可搜索命令。发送 `/review src/lib.rs` 时，模板中每个 `$ARGUMENTS` 占位符都会被替换为 `src/lib.rs`；若模板没有占位符，参数会追加到提示词末尾。未知的斜杠名会原样发送，`//review` 会发送字面文本 `/review`。展开后的提示词才是模型实际收到的内容，也是会话历史记录的内容。

## 权限

CLI 的文件访问由 `permissions.mode` 控制：

| 模式 | 行为 |
| --- | --- |
| `read_only` | 拒绝写入类工具。 |
| `workspace_write` | 文件修改需要批准，且限制在工作区内。 |
| `danger_full_access` | 文件读写可访问工作区外的路径。 |

Shell 执行由 `permissions.shell` 单独控制：

| 模式 | 行为 |
| --- | --- |
| `deny` | 拒绝 shell 命令。 |
| `prompt` | shell 命令需要批准。 |
| `allow` | shell 命令不经批准直接执行。 |

`morrow init` 生成的默认配置为 `read_only` 加 `shell = "deny"`。

单次运行覆盖权限：

```bash
morrow --permission workspace-write "update the README"
morrow --allow-shell "run the test suite and explain failures"
```

## 会话

Morrow 将按项目划分的会话存储在 `~/.morrow/sessions/` 下。使用命名会话可跨多次调用续接工作：

```bash
morrow --session work "continue the refactor"
morrow --session work
```

管理会话：

```bash
morrow session list
morrow session show work
morrow session export work --output work-session.json
morrow session rename work backend-refactor
morrow session delete backend-refactor
```

兼容别名 `--thread` 与 `--reset-thread` 仍然可用，但新用法请优先使用 `--session` 与 `--reset-session`。

常用 REPL 命令：

```text
/status
/permissions read-only
/permissions workspace-write
/permissions danger-full-access
/compact
/reset
/exit
```

## 自动化

用于自动化时，每个事件输出一个 JSON 对象：

```bash
morrow --jsonl "inspect this crate" > events.jsonl
```

JSONL 模式要求必须提供提示词，且不可用于交互模式或 session 子命令。

## 开发

crate 边界、依赖方向、turn 生命周期、扩展点与取消语义见 [`ARCHITECTURE.md`](ARCHITECTURE.md)。

Morrow 是一个 Rust workspace：

| Crate | 职责 |
| --- | --- |
| `crates/agent-cli` | CLI 入口、REPL、JSONL 输出、server 命令与配置装配。 |
| `crates/agent-desktop/src-tauri` | Tauri 2 桌面外壳、原生菜单、项目切换与本地 server 生命周期。 |
| `crates/agent-config` | 加载 `morrow.toml` 与 `~/.morrow/config.toml`。 |
| `crates/agent-core` | Agent turn 执行与事件流编排。 |
| `crates/agent-model` | OpenAI 兼容 HTTP 客户端与流式响应解析。 |
| `crates/agent-protocol` | 共享协议、会话、权限与事件类型。 |
| `crates/agent-runtime` | 可复用的运行时辅助：会话、压缩、工作区探测与 turn 执行。 |
| `crates/agent-server` | Axum HTTP/WebSocket 服务器与内嵌仪表盘资源。 |
| `crates/agent-sandbox` | 权限判定。 |
| `crates/agent-tools` | 内置文件与 shell 工具。 |

常用 Rust 检查：

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets
```

从源码运行：

```bash
cargo run -p agent-cli -- "hello"
cargo run -p agent-cli -- --session work "continue"
cargo run -p agent-cli -- server
```

### Web 仪表盘

```bash
cd crates/agent-server/web
pnpm install
pnpm dev
```

Vite 开发服务器监听 `127.0.0.1:5173`，并将 `/api` 代理到 `http://127.0.0.1:3000`，因此请在另一个终端运行 `cargo run -p agent-cli -- server`。

构建用于内嵌进 `agent-server` 的仪表盘资源：

```bash
cd crates/agent-server/web
pnpm build
pnpm typecheck
```

### 桌面应用

```bash
pnpm --dir crates/agent-server/web install
pnpm --dir crates/agent-desktop install
pnpm --dir crates/agent-desktop dev
```

桌面开发环境会在 `127.0.0.1:5173` 启动 Vite；原生外壳在 `127.0.0.1:3000` 启动浏览器模式的 Axum 后端，请保持 3000 端口空闲。Release 构建则会在随机回环端口启动带鉴权的后端，并加载内嵌的仪表盘资源。

原生安装包必须在目标操作系统上构建。进行桌面原生开发或 Windows/macOS 安装包构建前，请按 Tauri 的目标后缀命名规则，将对应的 ripgrep 二进制放入 `crates/agent-desktop/src-tauri/binaries/`：

```text
morrow-rg-x86_64-pc-windows-msvc.exe
morrow-rg-aarch64-apple-darwin
morrow-rg-x86_64-apple-darwin
```

然后构建目标安装包：

```bash
pnpm --dir crates/agent-desktop build:windows
pnpm --dir crates/agent-desktop build:macos
```

Release 通过打上与 workspace 版本完全一致的 tag 触发，例如 `v0.3.0`。GitHub Actions 会将 CLI 压缩包、Windows NSIS 安装程序、两个架构的 macOS DMG 以及 `SHA256SUMS` 发布到同一个 Release。早期体验阶段不需要签名或 updater 相关的密钥。

## 卸载

删除 CLI 二进制，或卸载/删除桌面应用即可。本地私有数据会有意保留，如需清理请单独删除：

```bash
rm -f ~/.local/bin/morrow
rm -rf ~/.morrow
```

## 许可证

[MIT](LICENSE) © 2026 Gargantua
