## Morrow v0.3.2-hotfix.1

该预发布版本修复 DeepSeek 长时间思考时模型流被错误中断的问题，并包含最新的 Windows + WSL 远程工作区体验。

### 修复内容

- 将模型流的总请求超时调整为连接和单次读取空闲超时。只要模型持续返回数据，长时间思考不会因为累计耗时超过 `timeout_secs` 而被截断。
- 真正停滞的模型流仍会按配置超时，并返回更明确的错误信息。
- Windows 桌面端包含 WSL 远程工作区支持，不会退回旧的本地文件夹选择启动流程。
- WSL `morrow-remote` 与 Windows 安装包使用同一 hotfix 版本，避免继续复用 `0.3.1` 的旧远程运行时缓存。

### 验证

- `cargo test --workspace --locked`
- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Web 类型检查、25 个前端测试和生产构建
- 长时间活跃 SSE 流与停滞流回归测试

### Windows 更新说明

1. 完全退出当前 Morrow。
2. 下载并运行 `Morrow_0.3.2-hotfix.1_x64-setup.exe`。
3. 首次重新连接 WSL 时，Morrow 会下载并部署同版本的 `morrow-remote`，不再复用旧的 `~/.morrow/server/0.3.1/` 运行时。

模型配置、MCP 设置、命令和 session 会在覆盖安装时保留。

该版本尚未使用商业 Windows 代码签名证书。如遇 SmartScreen 提示，请确认安装包来自本项目 GitHub Release 后选择“仍要运行”。
