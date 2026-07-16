fn main() {
    let attributes =
        tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&[
            "desktop_shell_state",
            "desktop_action",
            "desktop_wsl_distributions",
            "desktop_wsl_probe",
            "desktop_wsl_prepare",
            "desktop_wsl_connect",
            "desktop_remote_request",
            "desktop_remote_subscribe",
            "desktop_remote_unsubscribe",
        ]));
    tauri_build::try_build(attributes).expect("failed to build Tauri desktop application");
}
