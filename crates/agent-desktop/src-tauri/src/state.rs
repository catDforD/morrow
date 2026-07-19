use crate::atomic_file;
use agent_protocol::WorkspaceLocation;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

const DESKTOP_STATE_SCHEMA: u32 = 2;
const MAX_RECENT_WORKSPACES: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DesktopState {
    schema: u32,
    #[serde(default)]
    last_workspace: Option<WorkspaceLocation>,
    #[serde(default)]
    recent_workspaces: Vec<WorkspaceLocation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyDesktopState {
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
        let value: serde_json::Value =
            serde_json::from_slice(&content).map_err(|source| DesktopStateError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        let schema = value
            .get("schema")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(DesktopStateError::MissingSchema)?;
        match schema {
            1 => {
                let legacy =
                    serde_json::from_value::<LegacyDesktopState>(value).map_err(|source| {
                        DesktopStateError::Parse {
                            path: path.to_path_buf(),
                            source,
                        }
                    })?;
                debug_assert_eq!(legacy.schema, 1);
                let state = Self {
                    schema: DESKTOP_STATE_SCHEMA,
                    last_workspace: legacy
                        .last_workspace
                        .map(|path| WorkspaceLocation::Local { path }),
                    recent_workspaces: legacy
                        .recent_workspaces
                        .into_iter()
                        .map(|path| WorkspaceLocation::Local { path })
                        .collect(),
                };
                state.save(path)?;
                Ok(state)
            }
            DESKTOP_STATE_SCHEMA => {
                serde_json::from_value::<Self>(value).map_err(|source| DesktopStateError::Parse {
                    path: path.to_path_buf(),
                    source,
                })
            }
            other => Err(DesktopStateError::UnsupportedSchema(other)),
        }
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

        atomic_file::replace(&temporary_path, path).map_err(|source| {
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

    pub(crate) fn last_workspace(&self) -> Option<&WorkspaceLocation> {
        self.last_workspace.as_ref()
    }

    pub(crate) fn recent_workspaces(&self) -> &[WorkspaceLocation] {
        &self.recent_workspaces
    }

    pub(crate) fn record_local_workspace(
        &mut self,
        workspace: &Path,
    ) -> Result<WorkspaceLocation, DesktopStateError> {
        self.record_workspace(WorkspaceLocation::Local {
            path: workspace.to_path_buf(),
        })
    }

    pub(crate) fn record_workspace(
        &mut self,
        workspace: WorkspaceLocation,
    ) -> Result<WorkspaceLocation, DesktopStateError> {
        let workspace = validate_workspace_location(workspace)?;
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
            .take()
            .and_then(|workspace| validate_workspace_location(workspace).ok());

        let mut recent = Vec::with_capacity(self.recent_workspaces.len());
        for workspace in &self.recent_workspaces {
            let Ok(workspace) = validate_workspace_location(workspace.clone()) else {
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

fn validate_workspace_location(
    workspace: WorkspaceLocation,
) -> Result<WorkspaceLocation, DesktopStateError> {
    match workspace {
        WorkspaceLocation::Local { path } => {
            canonical_workspace(&path).map(|path| WorkspaceLocation::Local { path })
        }
        WorkspaceLocation::Wsl { distro, user, path } => {
            let distro = distro.trim().to_string();
            let user = user.trim().to_string();
            let path = path.trim().to_string();
            if distro.is_empty() || user.is_empty() || !path.starts_with('/') {
                return Err(DesktopStateError::InvalidRemoteWorkspace);
            }
            Ok(WorkspaceLocation::Wsl { distro, user, path })
        }
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
    #[error("desktop state is missing its schema")]
    MissingSchema,
    #[error("remote workspace requires a distro, user, and absolute Linux path")]
    InvalidRemoteWorkspace,
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
                .record_local_workspace(&workspace)
                .expect("record workspace");
        }

        assert_eq!(state.recent_workspaces().len(), 10);
        assert_eq!(
            state.last_workspace(),
            Some(&WorkspaceLocation::Local {
                path: workspaces[11].clone()
            })
        );
        assert_eq!(
            state.recent_workspaces()[0],
            WorkspaceLocation::Local {
                path: workspaces[11].clone()
            }
        );
        assert_eq!(
            state.recent_workspaces()[9],
            WorkspaceLocation::Local {
                path: workspaces[2].clone()
            }
        );
    }

    #[test]
    fn prunes_missing_workspaces_and_clears_an_invalid_last_workspace() {
        let root = TestDirectory::new("invalid");
        let valid = root.0.join("valid");
        let missing = root.0.join("missing");
        fs::create_dir(&valid).expect("create workspace");
        let mut state = DesktopState {
            schema: DESKTOP_STATE_SCHEMA,
            last_workspace: Some(WorkspaceLocation::Local {
                path: missing.clone(),
            }),
            recent_workspaces: vec![
                WorkspaceLocation::Local { path: missing },
                WorkspaceLocation::Local {
                    path: valid.clone(),
                },
                WorkspaceLocation::Local {
                    path: valid.clone(),
                },
            ],
        };

        assert!(state.prune_invalid_workspaces());
        assert_eq!(state.last_workspace(), None);
        assert_eq!(
            state.recent_workspaces(),
            &[WorkspaceLocation::Local {
                path: fs::canonicalize(valid).expect("canonical workspace")
            }]
        );
    }

    #[test]
    fn saves_and_loads_schema_v2_state_atomically() {
        let root = TestDirectory::new("save");
        let workspace = root.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let path = root.0.join("state").join("desktop.json");
        let mut state = DesktopState::default();
        state
            .record_local_workspace(&workspace)
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
    fn migrates_schema_v1_local_workspaces() {
        let root = TestDirectory::new("migrate");
        let workspace = root.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        let path = root.0.join("desktop.json");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schema": 1,
                "last_workspace": workspace,
                "recent_workspaces": [workspace]
            }))
            .expect("serialize legacy state"),
        )
        .expect("write legacy state");

        let state = DesktopState::load(&path).expect("migrate state");
        assert_eq!(state.schema, DESKTOP_STATE_SCHEMA);
        assert!(matches!(
            state.last_workspace(),
            Some(WorkspaceLocation::Local { .. })
        ));
        let migrated: serde_json::Value =
            serde_json::from_slice(&fs::read(path).expect("read migrated state"))
                .expect("parse migrated state");
        assert_eq!(migrated["schema"], DESKTOP_STATE_SCHEMA);
    }

    #[test]
    fn rejects_unknown_schema_versions() {
        let root = TestDirectory::new("schema");
        let path = root.0.join("desktop.json");
        fs::write(&path, r#"{"schema":3}"#).expect("write state");

        assert!(matches!(
            DesktopState::load(&path),
            Err(DesktopStateError::UnsupportedSchema(3))
        ));
    }
}
