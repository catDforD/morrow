use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

const DESKTOP_STATE_SCHEMA: u32 = 1;
const MAX_RECENT_WORKSPACES: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DesktopState {
    schema: u32,
    #[serde(default)]
    last_workspace: Option<PathBuf>,
    #[serde(default)]
    recent_workspaces: Vec<PathBuf>,
}

impl Default for DesktopState {
    fn default() -> Self {
        Self {
            schema: DESKTOP_STATE_SCHEMA,
            last_workspace: None,
            recent_workspaces: Vec::new(),
        }
    }
}

impl DesktopState {
    pub(crate) fn validate_workspace(workspace: &Path) -> Result<PathBuf, DesktopStateError> {
        canonical_workspace(workspace)
    }

    pub(crate) fn load(path: &Path) -> Result<Self, DesktopStateError> {
        let content = match fs::read(path) {
            Ok(content) => content,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(DesktopStateError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let state: Self =
            serde_json::from_slice(&content).map_err(|source| DesktopStateError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        if state.schema != DESKTOP_STATE_SCHEMA {
            return Err(DesktopStateError::UnsupportedSchema(state.schema));
        }
        Ok(state)
    }

    pub(crate) fn save(&self, path: &Path) -> Result<(), DesktopStateError> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| DesktopStateError::MissingParent(path.to_path_buf()))?;
        fs::create_dir_all(parent).map_err(|source| DesktopStateError::CreateDirectory {
            path: parent.to_path_buf(),
            source,
        })?;

        let contents = serde_json::to_vec_pretty(self).map_err(DesktopStateError::Serialize)?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("desktop.json");
        let temporary_path = parent.join(format!(".{file_name}.tmp-{}", std::process::id()));

        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file =
            options
                .open(&temporary_path)
                .map_err(|source| DesktopStateError::Write {
                    path: temporary_path.clone(),
                    source,
                })?;
        let write_result = (|| -> Result<(), std::io::Error> {
            file.write_all(&contents)?;
            file.write_all(b"\n")?;
            file.sync_all()
        })();
        if let Err(source) = write_result {
            let _ = fs::remove_file(&temporary_path);
            return Err(DesktopStateError::Write {
                path: temporary_path,
                source,
            });
        }

        fs::rename(&temporary_path, path).map_err(|source| {
            let _ = fs::remove_file(&temporary_path);
            DesktopStateError::Replace {
                path: path.to_path_buf(),
                source,
            }
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
                DesktopStateError::Permissions {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
            let directory = fs::File::open(parent).map_err(|source| DesktopStateError::Read {
                path: parent.to_path_buf(),
                source,
            })?;
            directory
                .sync_all()
                .map_err(|source| DesktopStateError::Write {
                    path: parent.to_path_buf(),
                    source,
                })?;
        }

        Ok(())
    }

    pub(crate) fn last_workspace(&self) -> Option<&Path> {
        self.last_workspace.as_deref()
    }

    pub(crate) fn recent_workspaces(&self) -> &[PathBuf] {
        &self.recent_workspaces
    }

    pub(crate) fn record_workspace(
        &mut self,
        workspace: &Path,
    ) -> Result<PathBuf, DesktopStateError> {
        let workspace = canonical_workspace(workspace)?;
        self.last_workspace = Some(workspace.clone());
        self.recent_workspaces.retain(|recent| recent != &workspace);
        self.recent_workspaces.insert(0, workspace.clone());
        self.recent_workspaces.truncate(MAX_RECENT_WORKSPACES);
        Ok(workspace)
    }

    pub(crate) fn prune_invalid_workspaces(&mut self) -> bool {
        let original = self.clone();
        self.last_workspace = self
            .last_workspace
            .as_deref()
            .and_then(|workspace| canonical_workspace(workspace).ok());

        let mut recent = Vec::with_capacity(self.recent_workspaces.len());
        for workspace in &self.recent_workspaces {
            let Ok(workspace) = canonical_workspace(workspace) else {
                continue;
            };
            if !recent.contains(&workspace) {
                recent.push(workspace);
            }
            if recent.len() == MAX_RECENT_WORKSPACES {
                break;
            }
        }
        self.recent_workspaces = recent;
        *self != original
    }
}

fn canonical_workspace(workspace: &Path) -> Result<PathBuf, DesktopStateError> {
    let canonical =
        fs::canonicalize(workspace).map_err(|source| DesktopStateError::InvalidWorkspace {
            path: workspace.to_path_buf(),
            source,
        })?;
    if !canonical.is_dir() {
        return Err(DesktopStateError::WorkspaceIsNotDirectory(canonical));
    }
    Ok(canonical)
}

#[derive(Debug, Error)]
pub(crate) enum DesktopStateError {
    #[error("desktop state path has no parent: {0}")]
    MissingParent(PathBuf),
    #[error("unsupported desktop state schema {0}")]
    UnsupportedSchema(u32),
    #[error("failed to read desktop state from {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse desktop state from {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize desktop state: {0}")]
    Serialize(serde_json::Error),
    #[error("failed to create desktop state directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write desktop state at {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to replace desktop state at {path}: {source}")]
    Replace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[cfg(unix)]
    #[error("failed to secure desktop state at {path}: {source}")]
    Permissions {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid workspace {path}: {source}")]
    InvalidWorkspace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("workspace is not a directory: {0}")]
    WorkspaceIsNotDirectory(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "morrow-desktop-{name}-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn records_canonical_recent_workspaces_and_limits_them_to_ten() {
        let root = TestDirectory::new("recent");
        let mut state = DesktopState::default();
        let mut workspaces = Vec::new();
        for index in 0..12 {
            let workspace = root.0.join(format!("workspace-{index}"));
            fs::create_dir(&workspace).expect("create workspace");
            workspaces.push(fs::canonicalize(&workspace).expect("canonical workspace"));
            state
                .record_workspace(&workspace)
                .expect("record workspace");
        }

        assert_eq!(state.recent_workspaces().len(), 10);
        assert_eq!(state.last_workspace(), Some(workspaces[11].as_path()));
        assert_eq!(state.recent_workspaces()[0], workspaces[11]);
        assert_eq!(state.recent_workspaces()[9], workspaces[2]);
    }

    #[test]
    fn prunes_missing_workspaces_and_clears_an_invalid_last_workspace() {
        let root = TestDirectory::new("invalid");
        let valid = root.0.join("valid");
        let missing = root.0.join("missing");
        fs::create_dir(&valid).expect("create workspace");
        let mut state = DesktopState {
            schema: DESKTOP_STATE_SCHEMA,
            last_workspace: Some(missing.clone()),
            recent_workspaces: vec![missing, valid.clone(), valid.clone()],
        };

        assert!(state.prune_invalid_workspaces());
        assert_eq!(state.last_workspace(), None);
        assert_eq!(
            state.recent_workspaces(),
            &[fs::canonicalize(valid).expect("canonical workspace")]
        );
    }

    #[test]
    fn saves_and_loads_schema_v1_state_atomically() {
        let root = TestDirectory::new("save");
        let workspace = root.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let path = root.0.join("state").join("desktop.json");
        let mut state = DesktopState::default();
        state
            .record_workspace(&workspace)
            .expect("record workspace");

        state.save(&path).expect("save state");
        assert_eq!(DesktopState::load(&path).expect("load state"), state);
        assert!(
            !root
                .0
                .join("state")
                .join(format!(".desktop.json.tmp-{}", std::process::id()))
                .exists()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path)
                    .expect("state metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn rejects_unknown_schema_versions() {
        let root = TestDirectory::new("schema");
        let path = root.0.join("desktop.json");
        fs::write(&path, r#"{"schema":2}"#).expect("write state");

        assert!(matches!(
            DesktopState::load(&path),
            Err(DesktopStateError::UnsupportedSchema(2))
        ));
    }
}
