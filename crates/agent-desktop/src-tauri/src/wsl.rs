use crate::atomic_file;
use agent_protocol::{
    REMOTE_PROTOCOL_VERSION, RemoteEnvelope, RemoteRequest, RemoteResponse, RemoteRole,
    RemoteWorkspaceConfiguration, RemoteWorkspaceInfo,
};
use agent_remote::client::{RemoteClient, RemoteClientError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::broadcast;

const RELEASES_BASE_URL: &str = "https://github.com/catDforD/morrow/releases/download";
const REMOTE_PLATFORM: &str = "linux-x64";
const REMOTE_BINARY_NAME: &str = "morrow-remote";
const REMOTE_RIPGREP_NAME: &str = "morrow-rg";
const REMOTE_BINARY_ASSET: &str = "morrow-remote-linux-x64";
const REMOTE_RIPGREP_ASSET: &str = "morrow-rg-linux-x64";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WslDistribution {
    pub name: String,
    pub version: u32,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WslProbe {
    pub distro: String,
    pub user: String,
    pub home: String,
    pub arch: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RemoteAssetManifest {
    version: String,
    protocol_version: u32,
    platform: String,
    files: Vec<RemoteAsset>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RemoteAsset {
    name: String,
    sha256: String,
    executable: bool,
}

pub(crate) struct WslConnection {
    child: Child,
    client: RemoteClient,
    heartbeat: tokio::task::JoinHandle<()>,
    pub distro: String,
    pub user: String,
    pub workspace: String,
    pub channel_id: u32,
}

#[derive(Clone)]
pub(crate) struct WslRequestClient {
    client: RemoteClient,
    channel_id: u32,
}

impl WslRequestClient {
    pub(crate) async fn request(&self, request: RemoteRequest) -> Result<RemoteResponse, WslError> {
        let channel_id = request_channel(self.channel_id, &request);
        self.client
            .request(channel_id, request)
            .await
            .map_err(Into::into)
    }
}

impl WslConnection {
    pub(crate) async fn start(distro: String, user: String) -> Result<Self, WslError> {
        validate_identifier("distro", &distro)?;
        if user.chars().any(|character| character == '\0') {
            return Err(WslError::InvalidInput(
                "user must not contain NUL".to_string(),
            ));
        }
        let probe = probe(&distro, &user).await?;
        if probe.user == "root" {
            return Err(WslError::RootUser);
        }
        if probe.arch != "x86_64" {
            return Err(WslError::UnsupportedArchitecture(probe.arch));
        }
        ensure_remote_runtime(&probe).await?;
        let remote_binary = format!(
            "{}/.morrow/server/{}/{}",
            probe.home.trim_end_matches('/'),
            env!("CARGO_PKG_VERSION"),
            REMOTE_BINARY_NAME
        );
        let mut command = wsl_command(&distro, &probe.user);
        command
            .arg("--exec")
            .arg(&remote_binary)
            .arg("host")
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        hide_console(&mut command);
        let mut child = command.spawn().map_err(WslError::Launch)?;
        let stdin = child.stdin.take().ok_or(WslError::MissingPipe("stdin"))?;
        let stdout = child.stdout.take().ok_or(WslError::MissingPipe("stdout"))?;
        let client = RemoteClient::connect(stdout, stdin).await?;
        if client.hello().role != RemoteRole::Host {
            return Err(WslError::Protocol(
                "remote runtime did not identify itself as a host".to_string(),
            ));
        }
        if client.hello().version != env!("CARGO_PKG_VERSION") {
            return Err(WslError::VersionMismatch {
                expected: env!("CARGO_PKG_VERSION").to_string(),
                actual: client.hello().version.clone(),
            });
        }
        let heartbeat_client = client.clone();
        let heartbeat = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await;
            loop {
                interval.tick().await;
                let heartbeat = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    heartbeat_client.request(0, RemoteRequest::Ping),
                )
                .await;
                if !matches!(heartbeat, Ok(Ok(RemoteResponse::Pong))) {
                    break;
                }
            }
        });
        if let Err(error) = prune_remote_runtimes(&probe).await {
            log::warn!("failed to prune old WSL Runtime versions: {error}");
        }
        Ok(Self {
            child,
            client,
            heartbeat,
            distro,
            user: probe.user,
            workspace: String::new(),
            channel_id: 1,
        })
    }

    pub(crate) async fn open_workspace(
        &mut self,
        workspace: String,
    ) -> Result<RemoteWorkspaceConfiguration, WslError> {
        if !workspace.starts_with('/') {
            return Err(WslError::InvalidInput(
                "workspace must be an absolute Linux path".to_string(),
            ));
        }
        let opened = self
            .client
            .request(0, RemoteRequest::OpenWorkspace { path: workspace })
            .await?;
        let RemoteResponse::WorkspaceOpened(RemoteWorkspaceInfo {
            channel_id, path, ..
        }) = opened
        else {
            return Err(WslError::Protocol(
                "remote host did not open the workspace".to_string(),
            ));
        };
        self.workspace = path;
        self.channel_id = channel_id;
        let response = self
            .client
            .request(self.channel_id, RemoteRequest::WorkspaceConfiguration)
            .await?;
        match response {
            RemoteResponse::WorkspaceConfiguration(configuration) => Ok(configuration),
            _ => Err(WslError::Protocol(
                "remote workspace did not return its configuration".to_string(),
            )),
        }
    }

    pub(crate) async fn request(&self, request: RemoteRequest) -> Result<RemoteResponse, WslError> {
        let channel_id = request_channel(self.channel_id, &request);
        self.client
            .request(channel_id, request)
            .await
            .map_err(Into::into)
    }

    pub(crate) fn subscribe_events(&self) -> broadcast::Receiver<RemoteEnvelope> {
        self.client.subscribe_events()
    }

    pub(crate) fn subscribe_closed(&self) -> tokio::sync::watch::Receiver<bool> {
        self.client.subscribe_closed()
    }

    pub(crate) fn request_client(&self) -> WslRequestClient {
        WslRequestClient {
            client: self.client.clone(),
            channel_id: self.channel_id,
        }
    }

    pub(crate) async fn shutdown(mut self, cancel_running: bool) {
        self.heartbeat.abort();
        let _ = self
            .client
            .request(0, RemoteRequest::Shutdown { cancel_running })
            .await;
        match tokio::time::timeout(std::time::Duration::from_secs(3), self.child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                let _ = self.child.start_kill();
                let _ = self.child.wait().await;
            }
        }
    }
}

fn request_channel(workspace_channel: u32, request: &RemoteRequest) -> u32 {
    match request {
        RemoteRequest::Ping
        | RemoteRequest::Activity
        | RemoteRequest::Environment
        | RemoteRequest::ListDirectory { .. }
        | RemoteRequest::OpenWorkspace { .. }
        | RemoteRequest::CloseWorkspace
        | RemoteRequest::Shutdown { .. } => 0,
        _ => workspace_channel.max(1),
    }
}

async fn prune_remote_runtimes(probe: &WslProbe) -> Result<(), WslError> {
    run_wsl_script(
        probe,
        "set -eu; root=\"$HOME/.morrow/server\"; current=\"$1\"; previous=; for entry in \"$root\"/*; do test -d \"$entry\" || continue; name=${entry##*/}; test \"$name\" = \"$current\" && continue; if test -z \"$previous\" || test \"$entry\" -nt \"$previous\"; then previous=\"$entry\"; fi; done; for entry in \"$root\"/*; do test -d \"$entry\" || continue; name=${entry##*/}; test \"$name\" = \"$current\" && continue; test -n \"$previous\" && test \"$entry\" = \"$previous\" && continue; rm -rf \"$entry\"; done",
        &[env!("CARGO_PKG_VERSION")],
        None,
    )
    .await
}

pub(crate) async fn list_distributions() -> Result<Vec<WslDistribution>, WslError> {
    ensure_windows()?;
    let output = Command::new("wsl.exe")
        .arg("--list")
        .arg("--verbose")
        .arg("--all")
        .output()
        .await
        .map_err(WslError::Launch)?;
    if !output.status.success() {
        return Err(WslError::CommandFailed(decode_windows_output(
            &output.stderr,
        )));
    }
    let mut distributions = parse_distribution_list(&decode_windows_output(&output.stdout));
    distributions.sort_by(|left, right| {
        right
            .is_default
            .cmp(&left.is_default)
            .then_with(|| left.name.cmp(&right.name))
    });
    distributions.dedup_by(|left, right| left.name == right.name);
    Ok(distributions)
}

pub(crate) async fn probe(distro: &str, user: &str) -> Result<WslProbe, WslError> {
    validate_identifier("distro", distro)?;
    if user.chars().any(|character| character == '\0') {
        return Err(WslError::InvalidInput(
            "user must not contain NUL".to_string(),
        ));
    }
    ensure_windows()?;
    let version = list_distributions()
        .await?
        .into_iter()
        .find(|distribution| distribution.name == distro)
        .ok_or_else(|| WslError::DistributionNotFound(distro.to_string()))?
        .version;
    if version != 2 {
        return Err(WslError::UnsupportedWslVersion {
            distro: distro.to_string(),
            version,
        });
    }
    let mut command = wsl_command(distro, user);
    command
        .arg("--exec")
        .arg("sh")
        .arg("-c")
        .arg("printf '%s\\n%s\\n%s\\n' \"$(id -un)\" \"$HOME\" \"$(uname -m)\"");
    hide_console(&mut command);
    let output = command.output().await.map_err(WslError::Launch)?;
    if !output.status.success() {
        return Err(WslError::CommandFailed(decode_windows_output(
            &output.stderr,
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let values = stdout.lines().map(str::trim).collect::<Vec<_>>();
    if values.len() < 3 || values[0].is_empty() || values[1].is_empty() {
        return Err(WslError::Protocol(
            "WSL environment probe returned incomplete output".to_string(),
        ));
    }
    Ok(WslProbe {
        distro: distro.to_string(),
        user: values[0].to_string(),
        home: values[1].to_string(),
        arch: values[2].to_string(),
    })
}

async fn ensure_remote_runtime(probe: &WslProbe) -> Result<PathBuf, WslError> {
    let cache = cached_runtime().await?;
    let manifest_digest = cached_manifest_digest(&cache)?;
    if remote_runtime_is_installed(probe, &manifest_digest).await? {
        return Ok(cache);
    }
    deploy_runtime(probe, &cache).await?;
    Ok(cache)
}

fn cached_manifest_digest(cache: &Path) -> Result<String, WslError> {
    let bytes = std::fs::read(cache.join("manifest.json")).map_err(WslError::CacheIo)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

async fn cached_runtime() -> Result<PathBuf, WslError> {
    let cache = dirs::cache_dir()
        .ok_or(WslError::CacheDirectoryNotFound)?
        .join("morrow")
        .join("remote-assets")
        .join(env!("CARGO_PKG_VERSION"))
        .join(REMOTE_PLATFORM);
    let manifest_path = cache.join("manifest.json");
    if let Ok(content) = std::fs::read(&manifest_path)
        && let Ok(manifest) = serde_json::from_slice::<RemoteAssetManifest>(&content)
        && manifest_is_valid(&manifest)
        && verify_cached_files(&cache, &manifest)?
    {
        return Ok(cache);
    }

    std::fs::create_dir_all(&cache).map_err(WslError::CacheIo)?;
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    let manifest_name = format!("morrow-remote-{REMOTE_PLATFORM}-manifest.json");
    let manifest_url = format!("{RELEASES_BASE_URL}/{tag}/{manifest_name}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(WslError::Download)?;
    let manifest_bytes = download(&client, &manifest_url).await?;
    let manifest = serde_json::from_slice::<RemoteAssetManifest>(&manifest_bytes)
        .map_err(WslError::Manifest)?;
    if !manifest_is_valid(&manifest) {
        return Err(WslError::InvalidManifest);
    }
    for file in &manifest.files {
        validate_asset_name(&file.name)?;
        let url = format!("{RELEASES_BASE_URL}/{tag}/{}", file.name);
        let bytes = download(&client, &url).await?;
        verify_sha256(&bytes, &file.sha256)?;
        atomic_write(&cache.join(&file.name), &bytes)?;
    }
    atomic_write(&manifest_path, &manifest_bytes)?;
    Ok(cache)
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, WslError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(WslError::Download)?
        .error_for_status()
        .map_err(WslError::Download)?;
    Ok(response.bytes().await.map_err(WslError::Download)?.to_vec())
}

fn manifest_is_valid(manifest: &RemoteAssetManifest) -> bool {
    manifest.version == env!("CARGO_PKG_VERSION")
        && manifest.protocol_version == REMOTE_PROTOCOL_VERSION
        && manifest.platform == REMOTE_PLATFORM
        && manifest.files.len() == 2
        && manifest.files.iter().all(|file| {
            matches!(
                file.name.as_str(),
                REMOTE_BINARY_ASSET | REMOTE_RIPGREP_ASSET
            )
        })
        && manifest
            .files
            .iter()
            .any(|file| file.name == REMOTE_BINARY_ASSET && file.executable)
        && manifest
            .files
            .iter()
            .any(|file| file.name == REMOTE_RIPGREP_ASSET && file.executable)
}

fn verify_cached_files(cache: &Path, manifest: &RemoteAssetManifest) -> Result<bool, WslError> {
    for file in &manifest.files {
        validate_asset_name(&file.name)?;
        let Ok(bytes) = std::fs::read(cache.join(&file.name)) else {
            return Ok(false);
        };
        if verify_sha256(&bytes, &file.sha256).is_err() {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn remote_runtime_is_installed(
    probe: &WslProbe,
    manifest_digest: &str,
) -> Result<bool, WslError> {
    let mut command = wsl_command(&probe.distro, &probe.user);
    command
        .arg("--exec")
        .arg("sh")
        .arg("-c")
        .arg("test -x \"$HOME/.morrow/server/$1/morrow-remote\" -a -x \"$HOME/.morrow/server/$1/morrow-rg\" -a \"$(cat \"$HOME/.morrow/server/$1/.complete\" 2>/dev/null || true)\" = \"$2\"")
        .arg("sh")
        .arg(env!("CARGO_PKG_VERSION"))
        .arg(manifest_digest);
    hide_console(&mut command);
    let status = command.status().await.map_err(WslError::Launch)?;
    Ok(status.success())
}

async fn deploy_runtime(probe: &WslProbe, cache: &Path) -> Result<(), WslError> {
    let manifest_bytes = std::fs::read(cache.join("manifest.json")).map_err(WslError::CacheIo)?;
    let manifest = serde_json::from_slice::<RemoteAssetManifest>(&manifest_bytes)
        .map_err(WslError::Manifest)?;
    let manifest_digest = format!("{:x}", Sha256::digest(&manifest_bytes));
    run_wsl_script(
        probe,
        "set -eu; rm -rf \"$HOME/.morrow/server/.$1.staging\"; mkdir -p \"$HOME/.morrow/server/.$1.staging\"",
        &[env!("CARGO_PKG_VERSION")],
        None,
    )
    .await?;
    for file in &manifest.files {
        validate_asset_name(&file.name)?;
        let bytes = std::fs::read(cache.join(&file.name)).map_err(WslError::CacheIo)?;
        verify_sha256(&bytes, &file.sha256)?;
        let install_name = remote_install_name(&file.name)?;
        let script = if file.executable {
            "set -eu; cat > \"$HOME/.morrow/server/.$1.staging/$2\"; chmod 755 \"$HOME/.morrow/server/.$1.staging/$2\""
        } else {
            "set -eu; cat > \"$HOME/.morrow/server/.$1.staging/$2\"; chmod 600 \"$HOME/.morrow/server/.$1.staging/$2\""
        };
        run_wsl_script(
            probe,
            script,
            &[env!("CARGO_PKG_VERSION"), install_name],
            Some(&bytes),
        )
        .await?;
    }
    run_wsl_script(
        probe,
        "set -eu; root=\"$HOME/.morrow/server/$1\"; staging=\"$HOME/.morrow/server/.$1.staging\"; previous=\"$HOME/.morrow/server/.$1.previous\"; printf '%s\\n' \"$2\" > \"$staging/.complete\"; rm -rf \"$previous\"; if test -d \"$root\"; then mv \"$root\" \"$previous\"; fi; if mv \"$staging\" \"$root\"; then rm -rf \"$previous\"; else if test -d \"$previous\"; then mv \"$previous\" \"$root\"; fi; exit 1; fi",
        &[env!("CARGO_PKG_VERSION"), &manifest_digest],
        None,
    )
    .await
}

async fn run_wsl_script(
    probe: &WslProbe,
    script: &str,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Result<(), WslError> {
    let mut command = wsl_command(&probe.distro, &probe.user);
    command
        .arg("--exec")
        .arg("sh")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    hide_console(&mut command);
    let mut child = command.spawn().map_err(WslError::Launch)?;
    if let Some(bytes) = stdin {
        let mut pipe = child.stdin.take().ok_or(WslError::MissingPipe("stdin"))?;
        pipe.write_all(bytes).await.map_err(WslError::Upload)?;
        pipe.shutdown().await.map_err(WslError::Upload)?;
    }
    let output = child.wait_with_output().await.map_err(WslError::Launch)?;
    if !output.status.success() {
        return Err(WslError::CommandFailed(decode_windows_output(
            &output.stderr,
        )));
    }
    Ok(())
}

fn wsl_command(distro: &str, user: &str) -> Command {
    let mut command = Command::new("wsl.exe");
    command.arg("--distribution").arg(distro);
    if !user.is_empty() {
        command.arg("--user").arg(user);
    }
    command
}

fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn ensure_windows() -> Result<(), WslError> {
    if cfg!(windows) {
        Ok(())
    } else {
        Err(WslError::UnsupportedPlatform)
    }
}

fn validate_identifier(field: &str, value: &str) -> Result<(), WslError> {
    if value.trim().is_empty() || value.chars().any(|character| character == '\0') {
        return Err(WslError::InvalidInput(format!(
            "{field} must not be empty or contain NUL"
        )));
    }
    Ok(())
}

fn validate_asset_name(name: &str) -> Result<(), WslError> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        return Err(WslError::InvalidManifest);
    }
    Ok(())
}

fn remote_install_name(asset_name: &str) -> Result<&'static str, WslError> {
    match asset_name {
        REMOTE_BINARY_ASSET => Ok(REMOTE_BINARY_NAME),
        REMOTE_RIPGREP_ASSET => Ok(REMOTE_RIPGREP_NAME),
        _ => Err(WslError::InvalidManifest),
    }
}

fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), WslError> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(WslError::Checksum {
            expected: expected.to_string(),
            actual,
        })
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), WslError> {
    let parent = path.parent().ok_or_else(|| {
        WslError::InvalidInput(format!("{} has no parent directory", path.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(WslError::CacheIo)?;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temp, bytes).map_err(WslError::CacheIo)?;
    if let Err(error) = atomic_file::replace(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(WslError::CacheIo(error));
    }
    Ok(())
}

fn decode_windows_output(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes);
    if bytes.iter().skip(1).step_by(2).any(|byte| *byte == 0) {
        let units = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .filter(|unit| *unit != 0)
            .collect::<Vec<_>>();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).replace('\0', "")
    }
}

fn parse_distribution_list(output: &str) -> Vec<WslDistribution> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let is_default = line.starts_with('*');
            let line = line.strip_prefix('*').unwrap_or(line).trim();
            let columns = line.split_whitespace().collect::<Vec<_>>();
            let version = columns.last()?.parse::<u32>().ok()?;
            let name = columns.first()?.to_string();
            Some(WslDistribution {
                name,
                version,
                is_default,
            })
        })
        .collect()
}

#[derive(Debug, Error)]
pub(crate) enum WslError {
    #[error("WSL connections are only available on Windows")]
    UnsupportedPlatform,
    #[error("remote runtime only supports x86_64 WSL; detected {0}")]
    UnsupportedArchitecture(String),
    #[error("WSL distribution {distro:?} uses WSL {version}; Morrow requires WSL 2")]
    UnsupportedWslVersion { distro: String, version: u32 },
    #[error("WSL distribution {0:?} was not found")]
    DistributionNotFound(String),
    #[error("Morrow refuses to run its WSL runtime as root")]
    RootUser,
    #[error("invalid WSL input: {0}")]
    InvalidInput(String),
    #[error("failed to launch WSL: {0}")]
    Launch(#[source] std::io::Error),
    #[error("WSL command failed: {0}")]
    CommandFailed(String),
    #[error("WSL process has no {0} pipe")]
    MissingPipe(&'static str),
    #[error("failed to upload remote runtime: {0}")]
    Upload(#[source] std::io::Error),
    #[error("remote runtime cache directory was not found")]
    CacheDirectoryNotFound,
    #[error("remote runtime cache failed: {0}")]
    CacheIo(#[source] std::io::Error),
    #[error("remote runtime download failed: {0}")]
    Download(#[source] reqwest::Error),
    #[error("remote runtime manifest failed: {0}")]
    Manifest(#[source] serde_json::Error),
    #[error("remote runtime manifest is invalid")]
    InvalidManifest,
    #[error("remote runtime checksum mismatch: expected {expected}, got {actual}")]
    Checksum { expected: String, actual: String },
    #[error("remote protocol failed: {0}")]
    Protocol(String),
    #[error("remote runtime version mismatch: expected {expected}, got {actual}")]
    VersionMismatch { expected: String, actual: String },
    #[error(transparent)]
    Client(#[from] RemoteClientError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_utf16_wsl_distribution_output() {
        let units = "Ubuntu\r\nDebian\r\n".encode_utf16().collect::<Vec<_>>();
        let bytes = units
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        assert_eq!(decode_windows_output(&bytes), "Ubuntu\r\nDebian\r\n");
    }

    #[test]
    fn parses_verbose_distribution_output_and_ignores_the_header() {
        let distributions = parse_distribution_list(
            "  NAME              STATE           VERSION\r\n* Ubuntu           Running         2\r\n  Debian           Stopped         1\r\n",
        );

        assert_eq!(
            distributions,
            [
                WslDistribution {
                    name: "Ubuntu".to_string(),
                    version: 2,
                    is_default: true,
                },
                WslDistribution {
                    name: "Debian".to_string(),
                    version: 1,
                    is_default: false,
                }
            ]
        );
    }

    #[test]
    fn rejects_asset_names_that_can_escape_the_install_directory() {
        assert!(validate_asset_name("morrow-remote").is_ok());
        assert!(validate_asset_name("../morrow-remote").is_err());
        assert!(validate_asset_name("remote;rm").is_err());
    }

    #[test]
    fn verifies_sha256_without_exposing_file_contents() {
        assert!(verify_sha256(b"morrow", &format!("{:x}", Sha256::digest(b"morrow"))).is_ok());
        assert!(verify_sha256(b"morrow", "00").is_err());
    }

    #[test]
    fn manifest_accepts_only_the_two_versioned_release_assets() {
        let manifest = RemoteAssetManifest {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: REMOTE_PROTOCOL_VERSION,
            platform: REMOTE_PLATFORM.to_string(),
            files: vec![
                RemoteAsset {
                    name: REMOTE_BINARY_ASSET.to_string(),
                    sha256: "00".repeat(32),
                    executable: true,
                },
                RemoteAsset {
                    name: REMOTE_RIPGREP_ASSET.to_string(),
                    sha256: "11".repeat(32),
                    executable: true,
                },
            ],
        };

        assert!(manifest_is_valid(&manifest));
        let mut unexpected = manifest.clone();
        unexpected.files.push(RemoteAsset {
            name: "extra".to_string(),
            sha256: "22".repeat(32),
            executable: false,
        });
        assert!(!manifest_is_valid(&unexpected));
        assert_eq!(
            remote_install_name(REMOTE_BINARY_ASSET).expect("binary install name"),
            REMOTE_BINARY_NAME
        );
    }

    #[test]
    fn distro_and_user_are_passed_as_opaque_wsl_arguments() {
        let command = wsl_command("Ubuntu; echo injected", "user $(touch nope)");
        let args = command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            [
                "--distribution",
                "Ubuntu; echo injected",
                "--user",
                "user $(touch nope)"
            ]
        );
    }

    #[test]
    fn host_control_and_workspace_requests_use_separate_channels() {
        assert_eq!(
            request_channel(
                7,
                &RemoteRequest::ListDirectory {
                    path: None,
                    show_hidden: false,
                }
            ),
            0
        );
        assert_eq!(
            request_channel(
                7,
                &RemoteRequest::Http {
                    method: "GET".to_string(),
                    path: "/api/status".to_string(),
                    body: None,
                }
            ),
            7
        );
    }
}
