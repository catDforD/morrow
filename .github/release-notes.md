## Morrow v0.3.1

Morrow v0.3.1 重点优化桌面端的一体化体验：Windows 启动时不再伴随终端窗口，菜单栏、标题栏、侧栏和会话界面现在能够更自然地融合在同一个应用窗口中。

### 桌面窗口体验

- Windows Release 构建改为 GUI 子系统，启动 Morrow 时不再额外弹出终端窗口，关闭应用也不再依赖终端进程。
- Windows 使用 Morrow 自绘标题栏，将 `File / Edit / Window / Help`、拖动区域和窗口控制按钮融入应用界面。
- 支持拖动窗口、双击最大化、最小化、最大化/还原和关闭窗口。
- macOS 保留系统全局菜单，并使用 Overlay 标题栏，让交通灯与 Morrow 窗口背景自然融合。
- Windows 窗口增加圆角、黑色外框和更轻量的内部分隔线。
- Linux/WSL 开发模式使用与 Windows 接近的自绘窗口，便于在开发阶段预览桌面布局。

### 菜单与键盘操作

- `File` 菜单支持 Open Folder、Open Recent、Close Window 和 Quit Morrow。
- `Edit` 菜单支持 Undo、Redo、Cut、Copy、Paste 和 Select All。
- `Window` 菜单支持 Minimize、Maximize/Restore 和 Close Window。
- `Help` 菜单支持 Download Latest Version、Open Logs 和 About Morrow。
- 支持 `Alt`、`F10`、方向键、Enter、Space 和 Escape 操作菜单。
- 保留 `Ctrl+O`、`Ctrl+W`、`Ctrl+Z/Y/X/C/V/A` 和系统 `Alt+F4` 行为。
- Edit 命令可以正确处理输入框、文本域和普通页面选区。

### 界面与侧栏

- 顶栏与侧栏使用统一背景，Morrow 品牌、会话标题和主内容区对齐更加自然。
- 精简品牌区和会话标题，只保留 `Morrow` 与当前会话名称。
- 增加桌面侧栏收起按钮；收起后聊天区自动扩展，并可从会话标题栏重新打开侧栏。
- 消息滚动区域延伸至聊天区底部，输入框保持底部悬浮，同时避免遮挡最后一条消息。
- 优化会话卡片阴影、归档按钮、状态标签和侧栏操作按钮的间距与对齐。
- 浏览器 Dashboard 的原有布局和响应式侧栏行为保持不变。

### 桌面 IPC 与安全

- 新增最小化的 `desktop_shell_state` 和 `desktop_action` 桌面命令。
- 桌面能力仅绑定 `main` 窗口和当前精确 loopback origin。
- Rust 侧校验窗口 label、动作白名单和最近项目索引，不接受任意路径、URL 或命令名。
- 不向前端开放 dialog、fs、shell 等额外桌面权限。
- 保持现有 bootstrap cookie、Host、Origin、CORS 和 CSP 安全约束。

### 测试与兼容性

- 新增桌面菜单状态机、键盘导航、编辑命令、IPC 白名单和 capability 测试。
- CI 现在会运行前端 Vitest 测试、类型检查和生产构建。
- 独立 CLI、浏览器 Dashboard、模型配置、MCP 配置和已有 session 保持兼容。

### 下载

- Windows 10 22H2/Windows 11 x64：`Morrow_0.3.1_x64-setup.exe`
- macOS 14+ Apple Silicon：`Morrow_0.3.1_aarch64.dmg`
- macOS 14+ Intel：`Morrow_0.3.1_x64.dmg`

桌面应用不会捆绑或替代独立的 `morrow` CLI。GitHub Release 中仍会同时提供各平台 CLI 压缩包和 `SHA256SUMS`。

### 安装说明

当前桌面安装包尚未使用商业 Windows 代码签名证书或 Apple Developer 公证。

- **Windows：** 如果出现 SmartScreen 提示，请点击“更多信息”，确认安装包来自本项目 GitHub Release，然后选择“仍要运行”。
- **macOS：** 将 Morrow 拖入 Applications，然后在 Finder 中右键 Morrow → Open。若仍被阻止，请前往“系统设置 → 隐私与安全性 → 仍要打开”。

Windows 自绘标题栏暂不支持悬停最大化按钮显示 Snap Layout，但拖动吸附和 `Win + 方向键` 仍可正常使用。

### 从 v0.3.0 更新

Morrow 暂不主动检查或安装更新，请从 GitHub Release 手动下载并覆盖安装：

- Windows：运行新版 NSIS 安装程序完成覆盖升级。
- macOS：将新版 Morrow 拖入 Applications，并确认替换旧版本。

配置、MCP 设置、命令和 session 均保存在 `~/.morrow`，覆盖安装或卸载桌面应用默认不会删除这些数据。
