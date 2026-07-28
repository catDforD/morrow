use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use agent_protocol::{PermissionProfile, ReasoningLevel};
use serde::{Deserialize, Serialize};

pub const TUI_STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTuiState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_session: Option<String>,
    #[serde(default)]
    pub permissions: PermissionProfile,
    #[serde(default)]
    pub reasoning: ReasoningLevel,
    #[serde(default)]
    pub reasoning_expanded: bool,
    #[serde(default = "default_true")]
    pub sessions_visible: bool,
    #[serde(default = "default_true")]
    pub inspector_visible: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiStateFile {
    pub schema_version: u32,
    #[serde(default)]
    pub workspaces: BTreeMap<String, WorkspaceTuiState>,
}

impl Default for TuiStateFile {
    fn default() -> Self {
        Self {
            schema_version: TUI_STATE_SCHEMA_VERSION,
            workspaces: BTreeMap::new(),
        }
    }
}

impl TuiStateFile {
    pub fn load(path: &Path) -> io::Result<Self> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let state: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if state.schema_version != TUI_STATE_SCHEMA_VERSION {
            return Ok(Self::default());
        }
        Ok(state)
    }

    pub fn save_atomic(&self, path: &Path) -> io::Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let temporary = temporary_path(path);
        let serialized = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;

        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&serialized)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        replace_file(&temporary, path)?;
        restrict_permissions(path)?;
        Ok(())
    }

    pub fn workspace(&self, root: &Path) -> Option<&WorkspaceTuiState> {
        self.workspaces.get(&workspace_key(root))
    }

    pub fn set_workspace(&mut self, root: &Path, state: WorkspaceTuiState) {
        self.workspaces.insert(workspace_key(root), state);
    }
}

pub fn default_state_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".morrow")
        .join("tui-state.json")
}

fn workspace_key(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn temporary_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()))
}

fn restrict_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, target: &Path) -> io::Result<()> {
    fs::rename(temporary, target)
}

#[cfg(windows)]
fn replace_file(temporary: &Path, target: &Path) -> io::Result<()> {
    if !target.exists() {
        return fs::rename(temporary, target);
    }

    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    const REPLACEFILE_WRITE_THROUGH: u32 = 0x0000_0001;
    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn ReplaceFileW(
            replaced_file_name: *const u16,
            replacement_file_name: *const u16,
            backup_file_name: *const u16,
            replace_flags: u32,
            exclude: *mut c_void,
            reserved: *mut c_void,
        ) -> i32;
    }

    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let temporary = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let replaced = unsafe {
        ReplaceFileW(
            target.as_ptr(),
            temporary.as_ptr(),
            std::ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("morrow-tui-{name}-{}", std::process::id()))
    }

    #[test]
    fn state_round_trips_without_secrets_or_drafts() {
        let path = temp_path("state.json");
        let _ = fs::remove_file(&path);
        let mut state = TuiStateFile::default();
        state.set_workspace(
            Path::new("/tmp/project"),
            WorkspaceTuiState {
                recent_session: Some("work".to_string()),
                reasoning_expanded: true,
                ..WorkspaceTuiState::default()
            },
        );
        state.save_atomic(&path).unwrap();
        let serialized = fs::read_to_string(&path).unwrap();
        assert!(!serialized.contains("draft"));
        assert!(!serialized.contains("api_key"));
        assert_eq!(TuiStateFile::load(&path).unwrap(), state);
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("permissions.json");
        let _ = fs::remove_file(&path);
        TuiStateFile::default().save_atomic(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(path).unwrap();
    }
}
