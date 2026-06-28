# Repository Guidelines

## 项目结构与模块组织

这是一个 Rust workspace，源码位于 `crates/`：

- `crates/agent-cli`：CLI 入口、参数解析和 thread 持久化。
- `crates/agent-core`：agent turn 执行逻辑与事件流。
- `crates/agent-model`：OpenAI-compatible 模型客户端和 SSE 解析。
- `crates/agent-protocol`：共享协议类型，例如 `Message`、`Thread`、`Turn`。
- `crates/agent-config`：`morrow.toml` 配置加载。
- `crates/agent-tools`、`crates/agent-sandbox`：预留给后续工具和沙箱能力。

测试通常和代码放在同一 crate 的 `#[cfg(test)]` 模块中。GitHub/PR 相关配置放在 `.github/`。

## 构建、测试与本地开发命令

- `cargo build --workspace`：编译全部 workspace crates。
- `cargo test --workspace`：运行全部单元测试和 doc tests。
- `cargo fmt --check`：检查 Rust 格式。
- `cargo clippy --workspace --all-targets`：检查库、二进制和测试代码的 lint。
- `cargo run -p agent-cli -- "hello"`：本地运行 CLI。
- `cargo run -p agent-cli -- --thread work "continue"`：使用指定持久化 thread 运行。

## 代码风格与命名约定

使用 Rust 2024 edition 和 `rustfmt` 默认格式。保持模块职责清晰：协议和数据类型放在 `agent-protocol`，运行时逻辑放在 `agent-core`，CLI 参数和本地持久化放在 `agent-cli`。公开类型使用 `PascalCase`，函数、变量和模块使用 `snake_case`。

## 测试指南

新增逻辑应添加就近单元测试，测试命名要描述行为，例如 `failed_turn_emits_error_and_does_not_update_thread`。CLI 存储相关测试应使用临时目录，避免读写真实的 `~/.morrow`。提交前运行：

```bash
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets
```

## Commit 与 Pull Request 规范

- 每个 commit 尽量只包含一个逻辑变更，提交信息使用 Conventional Commits 常用格式：`type(scope): subject` 或 `type: subject`，例如 `feat(cli): persist threads`、`fix(model): handle empty stream`、`docs: update contributor guide`。  
- PR 标题同样使用标准前缀格式，例如 `feat: persistent CLI threads`、`fix: thread store error handling`。  
- 新建分支使用 `feat/xxx`、`fix/xxx` 等形式，名称保持简短并使用小写短横线。  
- PR 内容参照 `.github/pull_request_template.md`，包含变更摘要、验证命令和已知限制；涉及 CLI 参数、thread 持久化、配置或协议格式变化时需明确说明。  

## 安全与配置提示

不要提交本地密钥。`morrow.toml` 已被忽略，可能包含本地测试用 API key；优先使用 `OPENAI_API_KEY` 等环境变量。持久化 thread 保存在 `~/.morrow/threads/`，其中可能包含用户输入和模型回复，应视为本地私有数据。
