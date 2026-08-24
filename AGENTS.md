# Repository Guidelines

## 项目结构与模块组织

这是一个 Rust workspace，源码位于 `crates/`：

- `crates/agent-cli`：CLI 入口、参数解析、REPL、JSONL 输出、`session` / `hooks` 子命令，以及 server 启动装配。
- `crates/agent-core`：agent turn 状态机、`Model` / `ToolRuntime` 端口、中间件链和事件流。
- `crates/agent-eval`：确定性回归评估：脚本化模型与工具驱动真实 turn 循环，断言行为并执行效率预算棘轮。
- `crates/agent-model`：OpenAI-compatible 模型客户端和 SSE 解析。
- `crates/agent-protocol`：共享协议类型，例如 `Message`、`Thread`、`Turn`、Session fact 和事件。
- `crates/agent-runtime`：一次 turn 的应用编排、上下文压缩、SessionStore 与 v6 fact log 持久化、MCP 装配和 Subagent 监督。
- `crates/agent-config`：`morrow.toml` 配置加载与校验。
- `crates/agent-tools`：内置文件与 shell 工具、ToolRegistry、MCP 适配和 `web_fetch`。
- `crates/agent-sandbox`：workspace 路径约束与权限判定。
- `crates/agent-server`：HTTP/WebSocket、内嵌 Web 仪表盘、远程审批与取消、Subagent/MCP/命令设置。
- `crates/agent-hooks`：命令 Hook 与中间件适配器（before_prompt、before_tool、permission_request、after_tool、pre/post compact），含项目 Hook 指纹信任。
- `crates/agent-remote`：Desktop/WSL 远程运行时协议与命令/事件转发。
- `crates/agent-desktop`：Tauri 2 桌面外壳、嵌入式 server 生命周期与 WSL 连接。

测试通常和代码放在同一 crate 的 `#[cfg(test)]` 模块中。GitHub/PR 相关配置放在 `.github/`。

## 构建、测试与本地开发命令

- `cargo build --workspace`：编译全部 workspace crates。
- `cargo test --workspace`：运行全部单元测试和 doc tests。
- `cargo fmt --check`：检查 Rust 格式。
- `cargo clippy --workspace --all-targets -- -D warnings`：lint 零警告门禁（CI 同款）。
- `cargo run -p agent-eval -- run`：运行 agent 循环确定性回归套件；新增或调整场景后用 `--update-baseline` 更新效率基线。
- `cargo run -p agent-cli -- "hello"`：本地运行 CLI。
- `cargo run -p agent-cli -- --session work "continue"`：使用指定持久化 session 运行（`--thread` 是旧别名，新代码请用 `--session`）。

## 代码风格与命名约定

使用 Rust 2024 edition 和 `rustfmt` 默认格式。保持模块职责清晰：协议和数据类型放在 `agent-protocol`，turn 状态机放在 `agent-core`，会话编排与持久化放在 `agent-runtime`，CLI 参数和 REPL 放在 `agent-cli`，Web/HTTP 放在 `agent-server`。公开类型使用 `PascalCase`，函数、变量和模块使用 `snake_case`。

## 测试指南

新增逻辑应添加就近单元测试，测试命名要描述行为，例如 `failed_turn_emits_error_and_does_not_update_thread`。CLI 存储相关测试应使用临时目录，避免读写真实的 `~/.morrow`。涉及 agent 循环、工具结果回灌、审批、轮次上限或消息链的变更，应在 `crates/agent-eval/src/suite.rs` 增加或调整场景，并把效率基线变更一并提交。提交前运行：

```bash
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p agent-eval -- run
```

## Commit 与 Pull Request 规范

- 每个 commit 尽量只包含一个逻辑变更，提交信息使用 Conventional Commits 常用格式：`type(scope): subject` 或 `type: subject`，例如 `feat(cli): persist sessions`、`fix(model): handle empty stream`、`docs: update contributor guide`。
- PR 标题同样使用标准前缀格式，例如 `feat: persistent CLI sessions`、`fix: thread store error handling`。
- 新建分支使用 `feat/xxx`、`fix/xxx` 等形式，名称保持简短并使用小写短横线。
- PR 内容参照 `.github/pull_request_template.md`，包含变更摘要、验证命令和已知限制；涉及 CLI 参数、session 持久化、配置、协议/事件格式或 eval 场景变化时需明确说明。

## 安全与配置提示

不要提交本地密钥。`morrow.toml` 已被忽略，可能包含本地测试用 API key；优先使用 `OPENAI_API_KEY` 等环境变量。持久化 session 保存在 `~/.morrow/sessions/`，持久化 Subagent 保存在 `~/.morrow/subagent-sessions/`，其中可能包含用户输入和模型回复，应视为本地私有数据。项目 Hook（`<workspace>/.morrow/hooks.toml`）在显式 `morrow hooks trust` 前默认禁用；Hook 命令以用户身份执行，审查后再信任。MCP 工具默认纳入审批管线：server 标注 `readOnlyHint` 的只读工具直接执行，其余工具每次调用都需批准，除非在 `[mcp_servers.*]` 里显式 `require_approval = false`。
