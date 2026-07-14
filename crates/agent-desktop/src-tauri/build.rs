fn main() {
    let attributes = tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&["desktop_shell_state", "desktop_action"]),
    );
    tauri_build::try_build(attributes).expect("failed to build Tauri desktop application");
}
