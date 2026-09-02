## Morrow v0.4.0

Morrow v0.4.0 收敛为 CLI 与浏览器 Web Dashboard 两种产品形态。旧平台运行时及对应发布物已移除。

### Web + CLI

- `morrow` 支持单次执行、交互式 REPL、JSONL 输出，以及 `session` / `hooks` 子命令。
- `morrow server` 在当前 workspace 启动本地 HTTP/WebSocket 服务，浏览器直接连接该服务使用 Dashboard。
- Dashboard 保留模型、MCP、命令、Subagent、审批、取消和 session 恢复能力。
- Session、配置与 Web 设置继续保存在 `~/.morrow`，现有数据无需迁移。

### 发布物

GitHub Release 仅提供各平台 CLI 压缩包和 `SHA256SUMS`：

- Linux x86_64 / aarch64
- macOS x86_64 / aarch64
- Windows x86_64

每个压缩包都包含 `morrow` CLI 及运行所需的 `morrow-rg`，可直接解压并加入 `PATH`。从源码安装：

```bash
cargo install --git https://github.com/catDforD/morrow --locked -p agent-cli
```

### 兼容性

0.4.0 是产品形态和部分公开 API 的 breaking change：平台专属运行时 API 不再提供。CLI 参数、session 路径、`morrow server` 的浏览器 bootstrap cookie、认证和 WebSocket session 协议保持兼容。
