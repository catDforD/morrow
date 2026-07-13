## Morrow v0.3.0

Morrow v0.3.0 introduces the first installable desktop release for Windows and macOS while preserving the existing CLI and browser dashboard workflows.

### Desktop app

- Added a Tauri 2 desktop application using the existing React/Vite dashboard and Rust agent runtime.
- Added a native folder picker and restoration of the last valid workspace.
- Added **File → Open Folder** and **File → Open Recent** with up to 10 recent projects.
- Added native application menus, single-instance handling, window state restoration, local rotating logs, and external-link handling.
- Windows closes the application with the main window; macOS hides it until reopened and exits with `Cmd+Q`.

### Workspace and local server

- Sessions and model selections are explicitly scoped to the selected workspace.
- Switching projects no longer modifies the process working directory.
- The desktop app starts Axum on a random loopback port in production.
- Running turns can be cancelled safely when switching projects or quitting.
- MCP caches are cleaned up during server shutdown.

### Local API security

- Added a random per-launch desktop token and `HttpOnly; SameSite=Strict` session cookie.
- Added loopback Host, WebSocket Origin and cookie validation.
- Added CSP, frame protection, `nosniff` and `no-referrer` security headers.
- Browser dashboard mode remains compatible with its previous behavior.

### Downloads

- Windows 10 22H2/Windows 11 x64: `Morrow_0.3.0_x64-setup.exe`
- macOS 14+ Apple Silicon: `Morrow_0.3.0_aarch64.dmg`
- macOS 14+ Intel: `Morrow_0.3.0_x64.dmg`

The desktop app does not bundle or replace the standalone `morrow` CLI.

### Installation notes

These early desktop builds do not use a commercial Windows signing certificate or Apple Developer notarization.

- **Windows:** If SmartScreen appears, select **More info**, verify that the installer came from this GitHub Release, then choose **Run anyway**.
- **macOS:** Drag Morrow into Applications, then right-click **Morrow → Open**. If it is still blocked, use **System Settings → Privacy & Security → Open Anyway**.

### Updating

Morrow does not perform automatic update checks. Download and install newer releases manually. Configuration, MCP settings, commands and sessions under `~/.morrow` remain available after upgrading or uninstalling the desktop app.