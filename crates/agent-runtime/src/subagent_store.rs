use crate::{AgentEventEnvelope, timestamp_ms};
use agent_protocol::{
    ModelInvocation, PermissionProfile, Session, SubagentInstanceSnapshot, SubagentInstanceStatus,
    SubagentRoleOverride, SubagentRunStatus,
};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const SUBAGENT_SESSION_SCHEMA_VERSION: u32 = 1;
pub const MAX_SUBAGENT_EVENT_LOG_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentInstanceDocument {
    pub schema_version: u32,
    pub snapshot: SubagentInstanceSnapshot,
    pub role_config: SubagentRoleOverride,
    pub permission_ceiling: PermissionProfile,
    pub model: ModelInvocation,
    pub system_prompt: String,
    pub session: Session,
    pub runs: Vec<agent_protocol::SubagentRunRecord>,
}

impl SubagentInstanceDocument {
    pub fn new(
        snapshot: SubagentInstanceSnapshot,
        role_config: SubagentRoleOverride,
        permission_ceiling: PermissionProfile,
        model: ModelInvocation,
        system_prompt: String,
    ) -> Self {
        Self {
            schema_version: SUBAGENT_SESSION_SCHEMA_VERSION,
            snapshot,
            role_config,
            permission_ceiling,
            model,
            system_prompt,
            session: Session::new(),
            runs: Vec::new(),
        }
    }

    fn interrupt_active_run(&mut self) -> bool {
        if !self.snapshot.status.is_active() {
            return false;
        }
        let now = timestamp_ms();
        self.snapshot.status = SubagentInstanceStatus::Interrupted;
        self.snapshot.queue_reason = None;
        self.snapshot.updated_at_ms = now;
        if let Some(run) = self.runs.last_mut()
            && !run.status.is_terminal()
        {
            run.status = SubagentRunStatus::Interrupted;
            run.completed_at_ms = Some(now);
            if let Some(summary) = run.summary.as_mut() {
                summary.status = SubagentRunStatus::Interrupted;
                summary.completed_at_ms = Some(now);
                summary.error = Some("subagent run was interrupted by process restart".to_string());
            }
        }
        true
    }
}

#[derive(Debug, Error)]
pub enum SubagentStoreError {
    #[error("home directory was not found")]
    HomeDirNotFound,
    #[error("failed to canonicalize workspace root {path}: {source}")]
    CanonicalizeWorkspace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid session name {0:?}")]
    InvalidSessionName(String),
    #[error("invalid subagent instance id {0:?}")]
    InvalidInstanceId(String),
    #[error("failed to read subagent data {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse subagent data {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("unsupported subagent document schema {version} in {path}; expected {expected}")]
    UnsupportedSchema {
        path: PathBuf,
        version: u32,
        expected: u32,
    },
    #[error("failed to create subagent directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize subagent data: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("failed to write subagent data {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to replace subagent data {path}: {source}")]
    Replace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("subagent instance {0:?} was not found")]
    NotFound(String),
    #[error("subagent target already exists at {0}")]
    TargetExists(PathBuf),
    #[error("failed to remove subagent data {path}: {source}")]
    Remove {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone)]
pub struct SubagentSessionStore {
    root: PathBuf,
    scope: String,
    directory: PathBuf,
    archived_directory: PathBuf,
}

impl SubagentSessionStore {
    pub fn for_workspace(workspace: &Path, session_name: &str) -> Result<Self, SubagentStoreError> {
        let home = dirs::home_dir().ok_or(SubagentStoreError::HomeDirNotFound)?;
        Self::new(
            home.join(".morrow").join("subagent-sessions"),
            workspace,
            session_name,
        )
    }

    pub fn new(
        root: impl Into<PathBuf>,
        workspace: &Path,
        session_name: &str,
    ) -> Result<Self, SubagentStoreError> {
        validate_session_name(session_name)?;
        let canonical = workspace.canonicalize().map_err(|source| {
            SubagentStoreError::CanonicalizeWorkspace {
                path: workspace.to_path_buf(),
                source,
            }
        })?;
        let root = root.into();
        let scope = hex_encode(canonical.as_os_str().as_encoded_bytes());
        let directory = root.join(&scope).join(session_name);
        let archived_directory = root.join(&scope).join("archive").join(session_name);
        Ok(Self {
            root,
            scope,
            directory,
            archived_directory,
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn list(&self) -> Result<Vec<SubagentInstanceDocument>, SubagentStoreError> {
        if !self.directory.is_dir() {
            return Ok(Vec::new());
        }
        let mut documents = Vec::new();
        let entries = fs::read_dir(&self.directory).map_err(|source| SubagentStoreError::Read {
            path: self.directory.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| SubagentStoreError::Read {
                path: self.directory.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            documents.push(self.load_path(&path)?);
        }
        documents.sort_by_key(|document| document.snapshot.created_at_ms);
        Ok(documents)
    }

    pub fn load(&self, instance_id: &str) -> Result<SubagentInstanceDocument, SubagentStoreError> {
        validate_instance_id(instance_id)?;
        let path = self.document_path(instance_id);
        if !path.is_file() {
            return Err(SubagentStoreError::NotFound(instance_id.to_string()));
        }
        self.load_path(&path)
    }

    pub fn load_recovered(&self) -> Result<Vec<SubagentInstanceDocument>, SubagentStoreError> {
        let mut documents = self.list()?;
        for document in &mut documents {
            if document.interrupt_active_run() {
                self.save(document)?;
            }
        }
        Ok(documents)
    }

    pub fn save(&self, document: &SubagentInstanceDocument) -> Result<(), SubagentStoreError> {
        validate_instance_id(&document.snapshot.id)?;
        let path = self.document_path(&document.snapshot.id);
        let parent = path.parent().expect("subagent document path has parent");
        fs::create_dir_all(parent).map_err(|source| SubagentStoreError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
        let bytes = serde_json::to_vec_pretty(document).map_err(SubagentStoreError::Serialize)?;
        let temp = path.with_extension(format!("json.tmp-{}", std::process::id()));
        fs::write(&temp, bytes).map_err(|source| SubagentStoreError::Write {
            path: temp.clone(),
            source,
        })?;
        replace_file(&temp, &path).map_err(|source| SubagentStoreError::Replace {
            path: path.clone(),
            source,
        })?;
        Ok(())
    }

    pub fn append_event(
        &self,
        instance_id: &str,
        envelope: &AgentEventEnvelope,
    ) -> Result<bool, SubagentStoreError> {
        validate_instance_id(instance_id)?;
        fs::create_dir_all(&self.directory).map_err(|source| SubagentStoreError::CreateDir {
            path: self.directory.clone(),
            source,
        })?;
        let path = self.event_path(instance_id);
        let mut bytes = serde_json::to_vec(envelope).map_err(SubagentStoreError::Serialize)?;
        bytes.push(b'\n');
        let current_len = path.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        let is_stream_delta = matches!(
            &envelope.event,
            agent_protocol::AgentEvent::ReasoningDelta(_)
                | agent_protocol::AgentEvent::TextDelta(_)
        );
        if is_stream_delta
            && current_len.saturating_add(bytes.len() as u64) > MAX_SUBAGENT_EVENT_LOG_BYTES
        {
            return Ok(false);
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| SubagentStoreError::Write {
                path: path.clone(),
                source,
            })?;
        file.write_all(&bytes)
            .map_err(|source| SubagentStoreError::Write { path, source })?;
        Ok(true)
    }

    pub fn events(&self, instance_id: &str) -> Result<Vec<AgentEventEnvelope>, SubagentStoreError> {
        validate_instance_id(instance_id)?;
        let path = self.event_path(instance_id);
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(&path).map_err(|source| SubagentStoreError::Read {
            path: path.clone(),
            source,
        })?;
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line).map_err(|source| SubagentStoreError::Parse {
                    path: path.clone(),
                    source,
                })
            })
            .collect()
    }

    pub fn delete(&self, instance_id: &str) -> Result<(), SubagentStoreError> {
        validate_instance_id(instance_id)?;
        let document = self.document_path(instance_id);
        if !document.is_file() {
            return Err(SubagentStoreError::NotFound(instance_id.to_string()));
        }
        remove_file(&document)?;
        let event = self.event_path(instance_id);
        if event.is_file() {
            remove_file(&event)?;
        }
        Ok(())
    }

    pub fn reset(&self) -> Result<(), SubagentStoreError> {
        remove_directory_if_exists(&self.directory)
    }

    pub fn archive(&self) -> Result<(), SubagentStoreError> {
        if !self.directory.exists() {
            return Ok(());
        }
        if self.archived_directory.exists() {
            return Err(SubagentStoreError::TargetExists(
                self.archived_directory.clone(),
            ));
        }
        if let Some(parent) = self.archived_directory.parent() {
            fs::create_dir_all(parent).map_err(|source| SubagentStoreError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::rename(&self.directory, &self.archived_directory).map_err(|source| {
            SubagentStoreError::Replace {
                path: self.archived_directory.clone(),
                source,
            }
        })
    }

    pub fn restore(&self) -> Result<(), SubagentStoreError> {
        if !self.archived_directory.exists() {
            return Ok(());
        }
        if self.directory.exists() {
            return Err(SubagentStoreError::TargetExists(self.directory.clone()));
        }
        if let Some(parent) = self.directory.parent() {
            fs::create_dir_all(parent).map_err(|source| SubagentStoreError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::rename(&self.archived_directory, &self.directory).map_err(|source| {
            SubagentStoreError::Replace {
                path: self.directory.clone(),
                source,
            }
        })
    }

    pub fn rename(&self, target_name: &str) -> Result<Self, SubagentStoreError> {
        let target = Self {
            root: self.root.clone(),
            scope: self.scope.clone(),
            directory: self.root.join(&self.scope).join(target_name),
            archived_directory: self
                .root
                .join(&self.scope)
                .join("archive")
                .join(target_name),
        };
        validate_session_name(target_name)?;
        if target.directory.exists() || target.archived_directory.exists() {
            return Err(SubagentStoreError::TargetExists(target.directory.clone()));
        }
        if self.directory.exists() {
            fs::rename(&self.directory, &target.directory).map_err(|source| {
                SubagentStoreError::Replace {
                    path: target.directory.clone(),
                    source,
                }
            })?;
        }
        Ok(target)
    }

    fn load_path(&self, path: &Path) -> Result<SubagentInstanceDocument, SubagentStoreError> {
        let content = fs::read_to_string(path).map_err(|source| SubagentStoreError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let document =
            serde_json::from_str::<SubagentInstanceDocument>(&content).map_err(|source| {
                SubagentStoreError::Parse {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
        if document.schema_version != SUBAGENT_SESSION_SCHEMA_VERSION {
            return Err(SubagentStoreError::UnsupportedSchema {
                path: path.to_path_buf(),
                version: document.schema_version,
                expected: SUBAGENT_SESSION_SCHEMA_VERSION,
            });
        }
        Ok(document)
    }

    fn document_path(&self, instance_id: &str) -> PathBuf {
        self.directory.join(format!("{instance_id}.json"))
    }

    fn event_path(&self, instance_id: &str) -> PathBuf {
        self.directory.join(format!("{instance_id}.events.jsonl"))
    }
}

fn validate_session_name(name: &str) -> Result<(), SubagentStoreError> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(SubagentStoreError::InvalidSessionName(name.to_string()));
    }
    Ok(())
}

fn validate_instance_id(id: &str) -> Result<(), SubagentStoreError> {
    if id.is_empty()
        || id.len() > 96
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(SubagentStoreError::InvalidInstanceId(id.to_string()));
    }
    Ok(())
}

fn remove_file(path: &Path) -> Result<(), SubagentStoreError> {
    fs::remove_file(path).map_err(|source| SubagentStoreError::Remove {
        path: path.to_path_buf(),
        source,
    })
}

fn remove_directory_if_exists(path: &Path) -> Result<(), SubagentStoreError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SubagentStoreError::Remove {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(temporary, target)
}

#[cfg(windows)]
fn replace_file(temporary: &Path, target: &Path) -> std::io::Result<()> {
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
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::{
        SubagentIdentity, SubagentInstanceStatus, SubagentRole, SubagentRoleOverride,
    };
    use std::env;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(label: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "morrow-subagent-store-{label}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        path
    }

    fn document(status: SubagentInstanceStatus) -> SubagentInstanceDocument {
        let now = timestamp_ms();
        SubagentInstanceDocument::new(
            SubagentInstanceSnapshot {
                id: "subagent-1".to_string(),
                role: SubagentRole::Explore,
                identity: SubagentIdentity {
                    id: "identity-1".to_string(),
                    name: "Researcher".to_string(),
                },
                status,
                created_at_ms: now,
                updated_at_ms: now,
                latest_run_id: None,
                latest_task: None,
                queue_reason: None,
                latest_summary: None,
                event_log_truncated: false,
            },
            SubagentRoleOverride::default(),
            PermissionProfile::default(),
            ModelInvocation {
                provider_id: "provider".to_string(),
                provider_name: "Provider".to_string(),
                model_id: "model".to_string(),
                model_name: "Model".to_string(),
                reasoning: agent_protocol::ReasoningLevel::Off,
            },
            "system".to_string(),
        )
    }

    #[test]
    fn documents_round_trip_and_active_runs_recover_as_interrupted() {
        let root = unique_dir("round-trip-root");
        let workspace = unique_dir("round-trip-workspace");
        let store = SubagentSessionStore::new(&root, &workspace, "default").expect("store");
        let mut active = document(SubagentInstanceStatus::WaitingApproval);
        active.snapshot.latest_run_id = Some("subrun-1".to_string());
        active.runs.push(agent_protocol::SubagentRunRecord {
            id: "subrun-1".to_string(),
            task: "make a change".to_string(),
            status: SubagentRunStatus::WaitingApproval,
            turn_index: 0,
            started_at_ms: timestamp_ms(),
            completed_at_ms: None,
            summary: None,
        });
        store.save(&active).expect("save");

        let recovered = store.load_recovered().expect("recover");

        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].snapshot.status,
            SubagentInstanceStatus::Interrupted
        );
        assert_eq!(recovered[0].runs[0].status, SubagentRunStatus::Interrupted);
        assert!(recovered[0].runs[0].completed_at_ms.is_some());
        assert!(recovered[0].runs[0].summary.is_none());
        assert_eq!(
            store.load("subagent-1").expect("reload").snapshot.status,
            SubagentInstanceStatus::Interrupted
        );
    }

    #[test]
    fn rename_archive_restore_and_reset_follow_parent_session_lifecycle() {
        let root = unique_dir("lifecycle-root");
        let workspace = unique_dir("lifecycle-workspace");
        let store = SubagentSessionStore::new(&root, &workspace, "default").expect("store");
        store
            .save(&document(SubagentInstanceStatus::Idle))
            .expect("save");
        let renamed = store.rename("work").expect("rename");
        assert!(renamed.load("subagent-1").is_ok());
        renamed.archive().expect("archive");
        assert!(renamed.list().expect("list archived").is_empty());
        renamed.restore().expect("restore");
        assert!(renamed.load("subagent-1").is_ok());
        renamed.reset().expect("reset");
        assert!(renamed.list().expect("list reset").is_empty());
    }

    #[test]
    fn instance_documents_persist_model_identity_but_not_remote_credentials() {
        let document = document(SubagentInstanceStatus::Idle);
        let value = serde_json::to_string(&document).expect("serialize document");
        let parsed: serde_json::Value = serde_json::from_str(&value).expect("parse document");

        assert_eq!(parsed["model"]["provider_id"], "provider");
        assert!(!value.contains("api_key"));
        assert!(!value.contains("base_url"));
        assert!(!value.contains("remote-model-secret"));
    }
}
