use crate::{project_session, projection_to_legacy_session, timestamp_ms};
use agent_protocol::{
    ModelInvocation, PermissionProfile, ReasoningLevel, Role, SESSION_DOCUMENT_SCHEMA_VERSION,
    Session, SessionDocument, SessionFact, SessionFactEnvelope, SessionLogHeader,
    SessionProjection, SessionTurnStatus, THREAD_DOCUMENT_SCHEMA_VERSION, ThreadDocument,
    TurnRecord, TurnStatus,
};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

static SESSION_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum SessionStoreError {
    #[error("home directory was not found")]
    HomeDirNotFound,
    #[error("failed to read current working directory: {0}")]
    CurrentDir(#[source] std::io::Error),
    #[error("failed to canonicalize current working directory {path}: {source}")]
    CanonicalizeCwd {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid session name {name:?}; use ASCII letters, digits, '-' or '_'")]
    InvalidSessionName { name: String },
    #[error("failed to read session file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse session file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid session fact log {path}: {message}")]
    InvalidLog { path: PathBuf, message: String },
    #[error("session {name:?} was not found")]
    SessionNotFound { name: String },
    #[error("unsupported session document schema version {version} in {path}; expected {expected}")]
    UnsupportedSchemaVersion {
        path: PathBuf,
        version: u32,
        expected: u32,
    },
    #[error("failed to create session directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize session file {path}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write session file {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to replace session file {path}: {source}")]
    Replace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to list session directory {path}: {source}")]
    List {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read session metadata {path}: {source}")]
    Metadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove session file {path}: {source}")]
    Remove {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove session artifact directory {path}: {source}")]
    RemoveArtifact {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("target session already exists at {path}")]
    TargetExists { path: PathBuf },
    #[error("session {name:?} is already owned by another process")]
    WriterBusy { name: String },
    #[error("session {name:?} handle is no longer writable: {reason}")]
    HandleInvalidated { name: String, reason: String },
    #[error("session {name:?} already has a running operation")]
    OperationActive { name: String },
    #[error(transparent)]
    Projection(#[from] crate::SessionProjectionError),
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
    legacy_root: PathBuf,
    scope: String,
    session_name: String,
    path: PathBuf,
    legacy_session_path: PathBuf,
    legacy_path: PathBuf,
    lock_path: PathBuf,
    archived_path: PathBuf,
}

#[derive(Debug)]
pub struct SessionWriterLease {
    file: File,
    lock_path: PathBuf,
}

impl SessionWriterLease {
    pub fn path(&self) -> &Path {
        &self.lock_path
    }
}

impl Drop for SessionWriterLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub name: String,
    pub path: PathBuf,
    pub turns: usize,
    pub active_messages: usize,
    pub summarized_turns: usize,
    pub has_summary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionListingEntry {
    pub session: SessionEntry,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionListingDiagnostic {
    pub name: Option<String>,
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionDirectoryListing {
    pub entries: Vec<SessionListingEntry>,
    pub diagnostics: Vec<SessionListingDiagnostic>,
}

impl SessionStore {
    pub fn for_current_dir(session_name: &str) -> Result<Self, SessionStoreError> {
        let cwd = env::current_dir().map_err(SessionStoreError::CurrentDir)?;
        Self::for_workspace(&cwd, session_name)
    }

    pub fn for_workspace(workspace: &Path, session_name: &str) -> Result<Self, SessionStoreError> {
        let home = dirs::home_dir().ok_or(SessionStoreError::HomeDirNotFound)?;
        Self::new(
            home.join(".morrow").join("sessions"),
            home.join(".morrow").join("threads"),
            workspace,
            session_name,
        )
    }

    pub fn load(&self) -> Result<Session, SessionStoreError> {
        match self.load_projection() {
            Ok(projection) => Ok(projection_to_legacy_session(&projection)),
            Err(SessionStoreError::SessionNotFound { .. }) => Ok(Session::new()),
            Err(error) => Err(error),
        }
    }

    pub fn load_existing(&self) -> Result<Session, SessionStoreError> {
        self.load_projection()
            .map(|projection| projection_to_legacy_session(&projection))
    }

    pub fn load_projection(&self) -> Result<SessionProjection, SessionStoreError> {
        if self.path.is_file() {
            let (header, facts) = read_log_path(&self.path)?;
            return project_session(&header, &facts).map_err(Into::into);
        }
        if self.legacy_session_path.is_file() {
            return self.project_legacy_path(&self.legacy_session_path);
        }
        if self.legacy_path.is_file() {
            return self.project_legacy_path(&self.legacy_path);
        }
        if self.is_archived() {
            return Err(SessionStoreError::TargetExists {
                path: self.archived_path.clone(),
            });
        }
        Err(SessionStoreError::SessionNotFound {
            name: self.session_name.clone(),
        })
    }

    pub fn load_log(
        &self,
    ) -> Result<(SessionLogHeader, Vec<SessionFactEnvelope>), SessionStoreError> {
        if self.path.is_file() {
            return read_log_path(&self.path);
        }
        let legacy = if self.legacy_session_path.is_file() {
            Some(self.legacy_session_path.as_path())
        } else if self.legacy_path.is_file() {
            Some(self.legacy_path.as_path())
        } else {
            None
        };
        let Some(path) = legacy else {
            return Err(SessionStoreError::SessionNotFound {
                name: self.session_name.clone(),
            });
        };
        let (session, version) = parse_legacy_document(path)?;
        Ok(facts_from_legacy_session(&session, version))
    }

    pub fn acquire_writer(&self) -> Result<SessionWriterLease, SessionStoreError> {
        if self.is_archived() {
            return Err(SessionStoreError::TargetExists {
                path: self.archived_path.clone(),
            });
        }
        self.acquire_lock()
    }

    fn acquire_lock(&self) -> Result<SessionWriterLease, SessionStoreError> {
        let parent = self
            .lock_path
            .parent()
            .expect("session lock path must have parent");
        fs::create_dir_all(parent).map_err(|source| SessionStoreError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .map_err(|source| SessionStoreError::Write {
                path: self.lock_path.clone(),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(SessionStoreError::WriterBusy {
                    name: self.session_name.clone(),
                });
            }
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(SessionStoreError::Write {
                    path: self.lock_path.clone(),
                    source,
                });
            }
        }
        file.set_len(0).map_err(|source| SessionStoreError::Write {
            path: self.lock_path.clone(),
            source,
        })?;
        writeln!(
            file,
            "pid={} session={}",
            std::process::id(),
            self.session_name
        )
        .map_err(|source| SessionStoreError::Write {
            path: self.lock_path.clone(),
            source,
        })?;
        file.flush().map_err(|source| SessionStoreError::Write {
            path: self.lock_path.clone(),
            source,
        })?;
        Ok(SessionWriterLease {
            file,
            lock_path: self.lock_path.clone(),
        })
    }

    pub fn ensure_v5(&self, lease: &SessionWriterLease) -> Result<(), SessionStoreError> {
        self.validate_lease(lease)?;
        if self.path.is_file() {
            repair_log_tail(&self.path)?;
            upgrade_log_schema(&self.path)?;
            read_log_path(&self.path)?;
            return Ok(());
        }
        if self.legacy_session_path.is_file() {
            return self.migrate_legacy_path(&self.legacy_session_path, lease);
        }
        if self.legacy_path.is_file() {
            return self.migrate_legacy_path(&self.legacy_path, lease);
        }
        let header = new_header();
        write_log_atomic(&self.path, &header, &[])
    }

    pub fn ensure_v5_existing(&self, lease: &SessionWriterLease) -> Result<(), SessionStoreError> {
        self.validate_lease(lease)?;
        if self.path.is_file() {
            repair_log_tail(&self.path)?;
            upgrade_log_schema(&self.path)?;
            read_log_path(&self.path)?;
            return Ok(());
        }
        if self.legacy_session_path.is_file() {
            return self.migrate_legacy_path(&self.legacy_session_path, lease);
        }
        if self.legacy_path.is_file() {
            return self.migrate_legacy_path(&self.legacy_path, lease);
        }
        Err(SessionStoreError::SessionNotFound {
            name: self.session_name.clone(),
        })
    }

    pub fn has_active_document(&self) -> bool {
        self.path.is_file() || self.legacy_session_path.is_file() || self.legacy_path.is_file()
    }

    pub fn append_fact(
        &self,
        lease: &SessionWriterLease,
        expected_revision: u64,
        operation_id: Option<String>,
        turn_id: Option<String>,
        fact: SessionFact,
    ) -> Result<SessionFactEnvelope, SessionStoreError> {
        self.ensure_v5(lease)?;
        let (header, facts) = read_log_path(&self.path)?;
        let actual_revision = facts.last().map_or(0, |fact| fact.revision);
        if actual_revision != expected_revision {
            return Err(SessionStoreError::InvalidLog {
                path: self.path.clone(),
                message: format!("expected revision {expected_revision}, found {actual_revision}"),
            });
        }
        let envelope = SessionFactEnvelope {
            revision: expected_revision + 1,
            timestamp_ms: timestamp_ms(),
            operation_id,
            turn_id,
            fact,
        };
        let mut projected = facts.clone();
        projected.push(envelope.clone());
        project_session(&header, &projected)?;
        let mut bytes =
            serde_json::to_vec(&envelope).map_err(|source| SessionStoreError::Serialize {
                path: self.path.clone(),
                source,
            })?;
        bytes.push(b'\n');
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|source| SessionStoreError::Write {
                path: self.path.clone(),
                source,
            })?;
        file.write_all(&bytes)
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_data())
            .map_err(|source| SessionStoreError::Write {
                path: self.path.clone(),
                source,
            })?;
        Ok(envelope)
    }

    pub fn recover_interrupted(
        &self,
        lease: &SessionWriterLease,
    ) -> Result<Option<SessionFactEnvelope>, SessionStoreError> {
        self.ensure_v5(lease)?;
        let projection = self.load_projection()?;
        let Some(turn) = projection
            .turns
            .iter()
            .rev()
            .find(|turn| turn.status == SessionTurnStatus::Running)
        else {
            return Ok(None);
        };
        self.append_fact(
            lease,
            projection.revision,
            Some(turn.operation_id.clone()),
            Some(turn.id.clone()),
            SessionFact::TurnInterrupted {
                reason: "turn was interrupted by process restart".to_string(),
            },
        )
        .map(Some)
    }

    /// Compatibility adapter used while callers migrate to append_fact. It performs a hard
    /// replacement and preserves the existing session id when possible.
    pub fn save(&self, session: &Session) -> Result<(), SessionStoreError> {
        if self.is_archived() {
            return Err(SessionStoreError::TargetExists {
                path: self.archived_path.clone(),
            });
        }
        let lease = self.acquire_writer()?;
        let session_id = if self.path.is_file() {
            read_log_path(&self.path)?.0.session_id
        } else {
            new_session_id()
        };
        let (mut header, facts) = facts_from_legacy_session(session, 4);
        header.session_id = session_id;
        write_log_atomic(&self.path, &header, &facts)?;
        self.validate_lease(&lease)
    }

    pub fn reset(&self) -> Result<SessionProjection, SessionStoreError> {
        let lease = self.acquire_writer()?;
        self.reset_with_lease(&lease)
    }

    pub fn reset_with_lease(
        &self,
        lease: &SessionWriterLease,
    ) -> Result<SessionProjection, SessionStoreError> {
        self.validate_lease(lease)?;
        if let Some(artifact_root) = self.artifact_root_for_log(&self.path) {
            remove_artifact_root(&artifact_root)?;
        }
        let header = new_header();
        write_log_atomic(&self.path, &header, &[])?;
        let _ = remove_if_exists(&self.legacy_session_path)?;
        let _ = remove_if_exists(&self.legacy_path)?;
        for backup in self.backup_paths(false)? {
            let _ = remove_if_exists(&backup)?;
        }
        project_session(&header, &[]).map_err(Into::into)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn artifact_root(&self) -> Result<PathBuf, SessionStoreError> {
        let path = if self.path.is_file() {
            &self.path
        } else if self.archived_path.is_file() {
            &self.archived_path
        } else {
            return Err(SessionStoreError::SessionNotFound {
                name: self.session_name.clone(),
            });
        };
        let header = read_log_header(path)?;
        Ok(self.artifact_root_for_session_id(&header.session_id))
    }

    #[cfg(test)]
    fn legacy_path(&self) -> &Path {
        &self.legacy_path
    }

    #[cfg(test)]
    fn legacy_session_path(&self) -> &Path {
        &self.legacy_session_path
    }

    pub fn list_current_scope(&self) -> Result<Vec<SessionEntry>, SessionStoreError> {
        let scope_dir = self.scope_dir();
        if !scope_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        self.append_entries(&scope_dir, &mut entries)?;
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    pub fn list_current_scope_with_archived(
        &self,
    ) -> Result<Vec<SessionListingEntry>, SessionStoreError> {
        let mut entries = self
            .list_current_scope()?
            .into_iter()
            .map(|session| SessionListingEntry {
                session,
                archived: false,
            })
            .collect::<Vec<_>>();
        let mut archived = Vec::new();
        self.append_entries(&self.archive_dir(), &mut archived)?;
        entries.extend(archived.into_iter().map(|session| SessionListingEntry {
            session,
            archived: true,
        }));
        entries.sort_by(|left, right| {
            left.archived
                .cmp(&right.archived)
                .then_with(|| left.session.name.cmp(&right.session.name))
        });
        Ok(entries)
    }

    pub fn list_current_scope_with_diagnostics(
        &self,
    ) -> Result<SessionDirectoryListing, SessionStoreError> {
        let mut listing = SessionDirectoryListing::default();
        self.append_entries_with_diagnostics(
            &self.scope_dir(),
            false,
            &mut listing.entries,
            &mut listing.diagnostics,
        )?;
        self.append_entries_with_diagnostics(
            &self.archive_dir(),
            true,
            &mut listing.entries,
            &mut listing.diagnostics,
        )?;
        listing.entries.sort_by(|left, right| {
            left.archived
                .cmp(&right.archived)
                .then_with(|| left.session.name.cmp(&right.session.name))
        });
        listing.diagnostics.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.path.cmp(&right.path))
        });
        Ok(listing)
    }

    pub fn is_archived(&self) -> bool {
        self.archived_path.is_file()
    }

    pub fn archive(&self) -> Result<(), SessionStoreError> {
        if self.is_archived() {
            return Err(SessionStoreError::TargetExists {
                path: self.archived_path.clone(),
            });
        }
        let lease = self.acquire_writer()?;
        self.archive_with_lease(&lease)
    }

    pub fn archive_with_lease(&self, lease: &SessionWriterLease) -> Result<(), SessionStoreError> {
        if self.is_archived() {
            return Err(SessionStoreError::TargetExists {
                path: self.archived_path.clone(),
            });
        }
        self.validate_lease(lease)?;
        self.ensure_v5(lease)?;
        if let Some(parent) = self.archived_path.parent() {
            fs::create_dir_all(parent).map_err(|source| SessionStoreError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let backup_moves = self
            .backup_paths(false)?
            .into_iter()
            .map(|source| {
                let target = self.archive_dir().join(
                    source
                        .file_name()
                        .expect("session backup must have file name"),
                );
                (source, target)
            })
            .collect::<Vec<_>>();
        if let Some((_, target)) = backup_moves.iter().find(|(_, target)| target.exists()) {
            return Err(SessionStoreError::TargetExists {
                path: target.clone(),
            });
        }
        fs::rename(&self.path, &self.archived_path).map_err(|source| {
            SessionStoreError::Replace {
                path: self.archived_path.clone(),
                source,
            }
        })?;
        let mut moved_backups = Vec::new();
        for (source_path, target) in backup_moves {
            if let Err(source) = fs::rename(&source_path, &target) {
                for (moved_source, moved_target) in moved_backups.into_iter().rev() {
                    let _ = fs::rename(moved_target, moved_source);
                }
                let _ = fs::rename(&self.archived_path, &self.path);
                return Err(SessionStoreError::Replace {
                    path: target,
                    source,
                });
            }
            moved_backups.push((source_path, target));
        }
        Ok(())
    }

    pub fn restore(&self) -> Result<(), SessionStoreError> {
        if !self.is_archived() {
            return Err(SessionStoreError::SessionNotFound {
                name: self.session_name.clone(),
            });
        }
        let _lease = self.acquire_lock()?;
        if self.path.exists() || self.legacy_session_path.exists() || self.legacy_path.exists() {
            return Err(SessionStoreError::TargetExists {
                path: self.path.clone(),
            });
        }
        let parent = self.path.parent().expect("session path has parent");
        fs::create_dir_all(parent).map_err(|source| SessionStoreError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
        let backup_moves = self
            .backup_paths(true)?
            .into_iter()
            .map(|source| {
                let target = self.scope_dir().join(
                    source
                        .file_name()
                        .expect("archived session backup must have file name"),
                );
                (source, target)
            })
            .collect::<Vec<_>>();
        if let Some((_, target)) = backup_moves.iter().find(|(_, target)| target.exists()) {
            return Err(SessionStoreError::TargetExists {
                path: target.clone(),
            });
        }
        fs::rename(&self.archived_path, &self.path).map_err(|source| {
            SessionStoreError::Replace {
                path: self.path.clone(),
                source,
            }
        })?;
        let mut moved_backups = Vec::new();
        for (source_path, target) in backup_moves {
            if let Err(source) = fs::rename(&source_path, &target) {
                for (moved_source, moved_target) in moved_backups.into_iter().rev() {
                    let _ = fs::rename(moved_target, moved_source);
                }
                let _ = fs::rename(&self.path, &self.archived_path);
                return Err(SessionStoreError::Replace {
                    path: target,
                    source,
                });
            }
            moved_backups.push((source_path, target));
        }
        Ok(())
    }

    pub fn delete(&self) -> Result<(), SessionStoreError> {
        let _lease = self.acquire_lock()?;
        let artifact_roots = [&self.path, &self.archived_path]
            .into_iter()
            .filter_map(|path| self.artifact_root_for_log(path))
            .collect::<std::collections::BTreeSet<_>>();
        for artifact_root in artifact_roots {
            remove_artifact_root(&artifact_root)?;
        }
        let mut targets = vec![
            &self.path,
            &self.legacy_session_path,
            &self.legacy_path,
            &self.archived_path,
        ];
        let backups = self
            .backup_paths(false)?
            .into_iter()
            .chain(self.backup_paths(true)?)
            .collect::<Vec<_>>();
        let removed = targets
            .drain(..)
            .map(|path| remove_if_exists(path))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .chain(
                backups
                    .iter()
                    .map(|path| remove_if_exists(path))
                    .collect::<Result<Vec<_>, _>>()?,
            )
            .any(|removed| removed);
        if !removed {
            return Err(SessionStoreError::SessionNotFound {
                name: self.session_name.clone(),
            });
        }
        Ok(())
    }

    pub fn rename(&self, target_name: &str) -> Result<SessionStore, SessionStoreError> {
        let target = self.store_for_name(target_name)?;
        if target.path.exists()
            || target.legacy_session_path.exists()
            || target.legacy_path.exists()
            || target.archived_path.exists()
        {
            return Err(SessionStoreError::TargetExists { path: target.path });
        }
        let lease = self.acquire_writer()?;
        self.ensure_v5(&lease)?;
        let backup_moves = self
            .backup_paths(false)?
            .into_iter()
            .map(|source| {
                let file_name = source
                    .file_name()
                    .and_then(|name| name.to_str())
                    .expect("session backup must have UTF-8 file name");
                let suffix = file_name
                    .strip_prefix(&self.session_name)
                    .expect("session backup must begin with session name");
                let target_path = target.scope_dir().join(format!("{target_name}{suffix}"));
                (source, target_path)
            })
            .collect::<Vec<_>>();
        if let Some((_, target_path)) = backup_moves
            .iter()
            .find(|(_, target_path)| target_path.exists())
        {
            return Err(SessionStoreError::TargetExists {
                path: target_path.clone(),
            });
        }
        fs::rename(&self.path, &target.path).map_err(|source| SessionStoreError::Replace {
            path: target.path.clone(),
            source,
        })?;
        let mut moved_backups = Vec::new();
        for (source_path, target_backup) in backup_moves {
            if let Err(source) = fs::rename(&source_path, &target_backup) {
                for (moved_source, moved_target) in moved_backups.into_iter().rev() {
                    let _ = fs::rename(moved_target, moved_source);
                }
                let _ = fs::rename(&target.path, &self.path);
                return Err(SessionStoreError::Replace {
                    path: target_backup,
                    source,
                });
            }
            moved_backups.push((source_path, target_backup));
        }
        Ok(target)
    }

    pub fn export_document_bytes(&self) -> Result<Vec<u8>, SessionStoreError> {
        let lease = self.acquire_writer()?;
        self.export_document_bytes_with_lease(&lease)
    }

    pub fn export_document_bytes_with_lease(
        &self,
        lease: &SessionWriterLease,
    ) -> Result<Vec<u8>, SessionStoreError> {
        self.validate_lease(lease)?;
        self.ensure_v5(lease)?;
        fs::read(&self.path).map_err(|source| SessionStoreError::Read {
            path: self.path.clone(),
            source,
        })
    }

    pub(crate) fn new(
        root: impl Into<PathBuf>,
        legacy_root: impl Into<PathBuf>,
        cwd: &Path,
        session_name: &str,
    ) -> Result<Self, SessionStoreError> {
        validate_session_name(session_name)?;
        let root = root.into();
        let legacy_root = legacy_root.into();
        let canonical_cwd =
            cwd.canonicalize()
                .map_err(|source| SessionStoreError::CanonicalizeCwd {
                    path: cwd.to_path_buf(),
                    source,
                })?;
        let scope = hex_encode(canonical_cwd.as_os_str().as_encoded_bytes());
        Self::from_parts(root, legacy_root, scope, session_name)
    }

    pub(crate) fn from_parts(
        root: PathBuf,
        legacy_root: PathBuf,
        scope: String,
        session_name: &str,
    ) -> Result<Self, SessionStoreError> {
        validate_session_name(session_name)?;
        let scope_dir = root.join(&scope);
        let archive_dir = scope_dir.join("archive");
        Ok(Self {
            root: root.clone(),
            legacy_root: legacy_root.clone(),
            scope: scope.clone(),
            session_name: session_name.to_string(),
            path: scope_dir.join(format!("{session_name}.jsonl")),
            legacy_session_path: scope_dir.join(format!("{session_name}.json")),
            legacy_path: legacy_root
                .join(&scope)
                .join(format!("{session_name}.json")),
            lock_path: scope_dir.join(format!("{session_name}.lock")),
            archived_path: archive_dir.join(format!("{session_name}.jsonl")),
        })
    }

    fn project_legacy_path(&self, path: &Path) -> Result<SessionProjection, SessionStoreError> {
        let (session, version) = parse_legacy_document(path)?;
        let (header, facts) = facts_from_legacy_session(&session, version);
        project_session(&header, &facts).map_err(Into::into)
    }

    fn migrate_legacy_path(
        &self,
        path: &Path,
        lease: &SessionWriterLease,
    ) -> Result<(), SessionStoreError> {
        self.validate_lease(lease)?;
        let (session, version) = parse_legacy_document(path)?;
        let (header, facts) = facts_from_legacy_session(&session, version);
        let backup_path = self
            .scope_dir()
            .join(format!("{}.legacy-v{version}.bak", self.session_name));
        if backup_path.exists() {
            return Err(SessionStoreError::TargetExists { path: backup_path });
        }
        let staging_path = self.path.with_file_name(format!(
            "{}.migration-{}-{}",
            self.path
                .file_name()
                .expect("session path has file name")
                .to_string_lossy(),
            std::process::id(),
            timestamp_ms(),
        ));
        write_log_atomic(&staging_path, &header, &facts)?;
        read_log_path(&staging_path)?;

        if let Err(source) = fs::rename(path, &backup_path) {
            let _ = remove_if_exists(&staging_path);
            return Err(SessionStoreError::Replace {
                path: backup_path,
                source,
            });
        }
        let old_parent = path.parent().expect("legacy session path has parent");
        let backup_parent = backup_path.parent().expect("backup path has parent");
        if let Err(source) =
            sync_parent_directory(old_parent).and_then(|_| sync_parent_directory(backup_parent))
        {
            let _ = fs::rename(&backup_path, path);
            let _ = remove_if_exists(&staging_path);
            return Err(SessionStoreError::Write {
                path: backup_parent.to_path_buf(),
                source,
            });
        }

        if let Err(source) = replace_file(&staging_path, &self.path) {
            let install_error = source.to_string();
            return match fs::rename(&backup_path, path) {
                Ok(()) => Err(SessionStoreError::Replace {
                    path: self.path.clone(),
                    source,
                }),
                Err(rollback) => Err(SessionStoreError::InvalidLog {
                    path: self.path.clone(),
                    message: format!(
                        "failed to install migrated v5 log ({install_error}) and failed to restore legacy source {} ({rollback}); original data remains at {}",
                        path.display(),
                        backup_path.display(),
                    ),
                }),
            };
        }
        sync_parent_directory(self.path.parent().expect("session path must have parent")).map_err(
            |source| SessionStoreError::Write {
                path: self.path.clone(),
                source,
            },
        )?;
        Ok(())
    }

    fn append_entries(
        &self,
        directory: &Path,
        entries: &mut Vec<SessionEntry>,
    ) -> Result<(), SessionStoreError> {
        if !directory.is_dir() {
            return Ok(());
        }
        let read_dir = fs::read_dir(directory).map_err(|source| SessionStoreError::List {
            path: directory.to_path_buf(),
            source,
        })?;
        let mut names = std::collections::BTreeSet::new();
        for entry in read_dir {
            let entry = entry.map_err(|source| SessionStoreError::List {
                path: directory.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            let metadata = entry
                .metadata()
                .map_err(|source| SessionStoreError::Metadata {
                    path: path.clone(),
                    source,
                })?;
            if !metadata.is_file() {
                continue;
            }
            let extension = path.extension().and_then(|value| value.to_str());
            if !matches!(extension, Some("jsonl" | "json")) {
                continue;
            }
            if let Some(name) = path.file_stem().and_then(|value| value.to_str()) {
                names.insert(name.to_string());
            }
        }
        for name in names {
            let store = Self::from_parts(
                self.root.clone(),
                self.legacy_root.clone(),
                self.scope.clone(),
                &name,
            )?;
            let path = if directory == self.archive_dir() {
                store.archived_path.clone()
            } else if store.path.is_file() {
                store.path.clone()
            } else {
                store.legacy_session_path.clone()
            };
            let projection = if directory == self.archive_dir() {
                let (header, facts) = read_log_path(&path)?;
                project_session(&header, &facts)?
            } else {
                store.load_projection()?
            };
            let summarized_turns = projection
                .context
                .covered_through_turn_id
                .as_ref()
                .and_then(|id| projection.turns.iter().position(|turn| turn.id == *id))
                .map_or(0, |index| index + 1);
            entries.push(SessionEntry {
                name,
                path,
                turns: projection.turns.len(),
                active_messages: projection.context.messages.len(),
                summarized_turns,
                has_summary: projection.context.summary.is_some(),
            });
        }
        Ok(())
    }

    fn append_entries_with_diagnostics(
        &self,
        directory: &Path,
        archived: bool,
        entries: &mut Vec<SessionListingEntry>,
        diagnostics: &mut Vec<SessionListingDiagnostic>,
    ) -> Result<(), SessionStoreError> {
        if !directory.is_dir() {
            return Ok(());
        }
        let read_dir = fs::read_dir(directory).map_err(|source| SessionStoreError::List {
            path: directory.to_path_buf(),
            source,
        })?;
        let mut candidates = std::collections::BTreeMap::<String, PathBuf>::new();
        for entry in read_dir {
            let entry = entry.map_err(|source| SessionStoreError::List {
                path: directory.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            let metadata = entry
                .metadata()
                .map_err(|source| SessionStoreError::Metadata {
                    path: path.clone(),
                    source,
                })?;
            if !metadata.is_file() {
                continue;
            }
            let extension = path.extension().and_then(|value| value.to_str());
            if !matches!(extension, Some("jsonl" | "json")) {
                continue;
            }
            if let Some(name) = path.file_stem().and_then(|value| value.to_str()) {
                candidates.entry(name.to_string()).or_insert(path);
            }
        }

        for (name, candidate_path) in candidates {
            let result = (|| {
                let store = Self::from_parts(
                    self.root.clone(),
                    self.legacy_root.clone(),
                    self.scope.clone(),
                    &name,
                )?;
                let path = if archived {
                    store.archived_path.clone()
                } else if store.path.is_file() {
                    store.path.clone()
                } else {
                    store.legacy_session_path.clone()
                };
                let projection = if archived {
                    let (header, facts) = read_log_path(&path)?;
                    project_session(&header, &facts)?
                } else {
                    store.load_projection()?
                };
                let summarized_turns = projection
                    .context
                    .covered_through_turn_id
                    .as_ref()
                    .and_then(|id| projection.turns.iter().position(|turn| turn.id == *id))
                    .map_or(0, |index| index + 1);
                Ok::<_, SessionStoreError>(SessionListingEntry {
                    session: SessionEntry {
                        name: name.clone(),
                        path,
                        turns: projection.turns.len(),
                        active_messages: projection.context.messages.len(),
                        summarized_turns,
                        has_summary: projection.context.summary.is_some(),
                    },
                    archived,
                })
            })();

            match result {
                Ok(entry) => entries.push(entry),
                Err(error) => diagnostics.push(SessionListingDiagnostic {
                    name: Some(name),
                    path: candidate_path,
                    message: error.to_string(),
                }),
            }
        }
        Ok(())
    }

    fn validate_lease(&self, lease: &SessionWriterLease) -> Result<(), SessionStoreError> {
        if lease.lock_path != self.lock_path {
            return Err(SessionStoreError::WriterBusy {
                name: self.session_name.clone(),
            });
        }
        Ok(())
    }

    fn scope_dir(&self) -> PathBuf {
        self.root.join(&self.scope)
    }

    fn archive_dir(&self) -> PathBuf {
        self.scope_dir().join("archive")
    }

    fn artifact_root_for_session_id(&self, session_id: &str) -> PathBuf {
        self.scope_dir()
            .join("artifacts")
            .join(hex_encode(session_id.as_bytes()))
    }

    fn artifact_root_for_log(&self, path: &Path) -> Option<PathBuf> {
        path.is_file()
            .then(|| read_log_header(path).ok())
            .flatten()
            .map(|header| self.artifact_root_for_session_id(&header.session_id))
    }

    fn store_for_name(&self, session_name: &str) -> Result<Self, SessionStoreError> {
        Self::from_parts(
            self.root.clone(),
            self.legacy_root.clone(),
            self.scope.clone(),
            session_name,
        )
    }

    fn backup_paths(&self, archived: bool) -> Result<Vec<PathBuf>, SessionStoreError> {
        let directory = if archived {
            self.archive_dir()
        } else {
            self.scope_dir()
        };
        if !directory.is_dir() {
            return Ok(Vec::new());
        }
        let prefix = format!("{}.legacy", self.session_name);
        let entries = fs::read_dir(&directory).map_err(|source| SessionStoreError::List {
            path: directory.clone(),
            source,
        })?;
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| SessionStoreError::List {
                path: directory.clone(),
                source,
            })?;
            let path = entry.path();
            let matches = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".bak"));
            if matches && path.is_file() {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    }
}

fn new_header() -> SessionLogHeader {
    SessionLogHeader {
        schema_version: SESSION_DOCUMENT_SCHEMA_VERSION,
        session_id: new_session_id(),
        created_at_ms: timestamp_ms(),
    }
}

fn remove_artifact_root(path: &Path) -> Result<(), SessionStoreError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(SessionStoreError::RemoveArtifact {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let result = if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(path)
    } else {
        fs::remove_dir_all(path)
    };
    result.map_err(|source| SessionStoreError::RemoveArtifact {
        path: path.to_path_buf(),
        source,
    })
}

fn new_session_id() -> String {
    let counter = SESSION_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "session-{:016x}-{:08x}-{counter:04x}",
        timestamp_ms(),
        std::process::id()
    )
}

fn read_log_path(
    path: &Path,
) -> Result<(SessionLogHeader, Vec<SessionFactEnvelope>), SessionStoreError> {
    let bytes = fs::read(path).map_err(|source| SessionStoreError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let ends_with_newline = bytes.last().is_some_and(|byte| *byte == b'\n');
    let content = String::from_utf8(bytes).map_err(|source| SessionStoreError::InvalidLog {
        path: path.to_path_buf(),
        message: source.to_string(),
    })?;
    let mut lines = content.split('\n').enumerate();
    let (_, header_line) = lines
        .next()
        .filter(|(_, line)| !line.trim().is_empty())
        .ok_or_else(|| SessionStoreError::InvalidLog {
            path: path.to_path_buf(),
            message: "missing log header".to_string(),
        })?;
    let header = serde_json::from_str::<SessionLogHeader>(header_line).map_err(|source| {
        SessionStoreError::Parse {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if !(5..=SESSION_DOCUMENT_SCHEMA_VERSION).contains(&header.schema_version) {
        return Err(SessionStoreError::UnsupportedSchemaVersion {
            path: path.to_path_buf(),
            version: header.schema_version,
            expected: SESSION_DOCUMENT_SCHEMA_VERSION,
        });
    }
    let mut facts = Vec::new();
    for (index, line) in lines {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionFactEnvelope>(line) {
            Ok(fact) => facts.push(fact),
            Err(_) if !ends_with_newline && index + 1 == content.lines().count() => break,
            Err(source) => {
                return Err(SessionStoreError::Parse {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
    project_session(&header, &facts)?;
    Ok((header, facts))
}

fn read_log_header(path: &Path) -> Result<SessionLogHeader, SessionStoreError> {
    let file = File::open(path).map_err(|source| SessionStoreError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|source| SessionStoreError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if line.trim().is_empty() {
        return Err(SessionStoreError::InvalidLog {
            path: path.to_path_buf(),
            message: "missing log header".to_string(),
        });
    }
    let header = serde_json::from_str::<SessionLogHeader>(line.trim_end()).map_err(|source| {
        SessionStoreError::Parse {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if !(5..=SESSION_DOCUMENT_SCHEMA_VERSION).contains(&header.schema_version) {
        return Err(SessionStoreError::UnsupportedSchemaVersion {
            path: path.to_path_buf(),
            version: header.schema_version,
            expected: SESSION_DOCUMENT_SCHEMA_VERSION,
        });
    }
    Ok(header)
}

fn write_log_atomic(
    path: &Path,
    header: &SessionLogHeader,
    facts: &[SessionFactEnvelope],
) -> Result<(), SessionStoreError> {
    project_session(header, facts)?;
    let parent = path.parent().expect("session path must have parent");
    fs::create_dir_all(parent).map_err(|source| SessionStoreError::CreateDir {
        path: parent.to_path_buf(),
        source,
    })?;
    let temp = path.with_file_name(format!(
        "{}.tmp-{}",
        path.file_name()
            .expect("session path has file name")
            .to_string_lossy(),
        std::process::id()
    ));
    let mut file = File::create(&temp).map_err(|source| SessionStoreError::Write {
        path: temp.clone(),
        source,
    })?;
    serde_json::to_writer(&mut file, header).map_err(|source| SessionStoreError::Serialize {
        path: temp.clone(),
        source,
    })?;
    file.write_all(b"\n")
        .map_err(|source| SessionStoreError::Write {
            path: temp.clone(),
            source,
        })?;
    for fact in facts {
        serde_json::to_writer(&mut file, fact).map_err(|source| SessionStoreError::Serialize {
            path: temp.clone(),
            source,
        })?;
        file.write_all(b"\n")
            .map_err(|source| SessionStoreError::Write {
                path: temp.clone(),
                source,
            })?;
    }
    file.flush()
        .and_then(|_| file.sync_all())
        .map_err(|source| SessionStoreError::Write {
            path: temp.clone(),
            source,
        })?;
    read_log_path(&temp)?;
    replace_file(&temp, path).map_err(|source| SessionStoreError::Replace {
        path: path.to_path_buf(),
        source,
    })?;
    sync_parent_directory(parent).map_err(|source| SessionStoreError::Write {
        path: parent.to_path_buf(),
        source,
    })
}

fn upgrade_log_schema(path: &Path) -> Result<(), SessionStoreError> {
    let bytes = fs::read(path).map_err(|source| SessionStoreError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') else {
        return Err(SessionStoreError::InvalidLog {
            path: path.to_path_buf(),
            message: "missing log header terminator".to_string(),
        });
    };
    let mut header =
        serde_json::from_slice::<SessionLogHeader>(&bytes[..newline]).map_err(|source| {
            SessionStoreError::Parse {
                path: path.to_path_buf(),
                source,
            }
        })?;
    if header.schema_version == SESSION_DOCUMENT_SCHEMA_VERSION {
        return Ok(());
    }
    // v5 之后的变更全部是 additive（新字段带 serde default），旧 facts 逐字节保留，
    // 只就地重写 header 版本号。
    if !(5..SESSION_DOCUMENT_SCHEMA_VERSION).contains(&header.schema_version) {
        return Err(SessionStoreError::UnsupportedSchemaVersion {
            path: path.to_path_buf(),
            version: header.schema_version,
            expected: SESSION_DOCUMENT_SCHEMA_VERSION,
        });
    }
    header.schema_version = SESSION_DOCUMENT_SCHEMA_VERSION;
    let mut upgraded =
        serde_json::to_vec(&header).map_err(|source| SessionStoreError::Serialize {
            path: path.to_path_buf(),
            source,
        })?;
    upgraded.push(b'\n');
    upgraded.extend_from_slice(&bytes[newline + 1..]);
    let temp = path.with_file_name(format!(
        "{}.schema-upgrade-{}",
        path.file_name()
            .expect("session path has file name")
            .to_string_lossy(),
        std::process::id()
    ));
    let mut file = File::create(&temp).map_err(|source| SessionStoreError::Write {
        path: temp.clone(),
        source,
    })?;
    file.write_all(&upgraded)
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
        .map_err(|source| SessionStoreError::Write {
            path: temp.clone(),
            source,
        })?;
    replace_file(&temp, path).map_err(|source| SessionStoreError::Replace {
        path: path.to_path_buf(),
        source,
    })?;
    sync_parent_directory(path.parent().expect("session path must have parent")).map_err(|source| {
        SessionStoreError::Write {
            path: path.to_path_buf(),
            source,
        }
    })
}

fn repair_log_tail(path: &Path) -> Result<(), SessionStoreError> {
    let bytes = fs::read(path).map_err(|source| SessionStoreError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if bytes.is_empty() || bytes.ends_with(b"\n") {
        return Ok(());
    }
    let last_newline = bytes.iter().rposition(|byte| *byte == b'\n');
    let tail_start = last_newline.map_or(0, |index| index + 1);
    let tail = &bytes[tail_start..];
    let complete = if last_newline.is_some() {
        serde_json::from_slice::<SessionFactEnvelope>(tail).is_ok()
    } else {
        serde_json::from_slice::<SessionLogHeader>(tail).is_ok()
    };
    if complete {
        let mut file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|source| SessionStoreError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        file.write_all(b"\n")
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_data())
            .map_err(|source| SessionStoreError::Write {
                path: path.to_path_buf(),
                source,
            })
    } else if let Some(last_newline) = last_newline {
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|source| SessionStoreError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        file.set_len((last_newline + 1) as u64)
            .and_then(|_| file.sync_data())
            .map_err(|source| SessionStoreError::Write {
                path: path.to_path_buf(),
                source,
            })
    } else {
        read_log_path(path).map(|_| ())
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

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> std::io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> std::io::Result<()> {
    Ok(())
}

fn parse_legacy_document(path: &Path) -> Result<(Session, u32), SessionStoreError> {
    let content = fs::read_to_string(path).map_err(|source| SessionStoreError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let value = serde_json::from_str::<serde_json::Value>(&content).map_err(|source| {
        SessionStoreError::Parse {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| SessionStoreError::InvalidLog {
            path: path.to_path_buf(),
            message: "missing schema_version".to_string(),
        })? as u32;
    match version {
        3 | 4 => {
            let document = serde_json::from_value::<SessionDocument>(value).map_err(|source| {
                SessionStoreError::Parse {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
            Ok((document.session, version))
        }
        1 | THREAD_DOCUMENT_SCHEMA_VERSION => {
            let document = serde_json::from_value::<ThreadDocument>(value).map_err(|source| {
                SessionStoreError::Parse {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
            Ok((Session::from_thread(document.thread), version))
        }
        _ => Err(SessionStoreError::UnsupportedSchemaVersion {
            path: path.to_path_buf(),
            version,
            expected: SESSION_DOCUMENT_SCHEMA_VERSION,
        }),
    }
}

fn facts_from_legacy_session(
    session: &Session,
    source_schema: u32,
) -> (SessionLogHeader, Vec<SessionFactEnvelope>) {
    let header = new_header();
    let mut facts = Vec::new();
    let mut revision = 0u64;
    for (index, record) in session.turns.iter().enumerate() {
        append_legacy_turn(&mut facts, &mut revision, index, record);
    }
    if let Some(summary) = session.context.summary.as_ref()
        && session.context.summarized_turns > 0
        && session.context.summarized_turns <= session.turns.len()
    {
        push_fact(
            &mut facts,
            &mut revision,
            None,
            None,
            SessionFact::ContextCompacted {
                summary: summary.clone(),
                covered_through_turn_id: format!(
                    "legacy-turn-{}",
                    session.context.summarized_turns - 1
                ),
            },
        );
    }
    let projected_context = project_session(&header, &facts)
        .map(|projection| projection.context.messages)
        .unwrap_or_default();
    if session.turns.is_empty() || projected_context != session.active_thread.messages {
        push_fact(
            &mut facts,
            &mut revision,
            None,
            None,
            SessionFact::LegacyContextCheckpoint {
                source_schema,
                messages: session.active_thread.messages.clone(),
                diagnostic: (projected_context != session.active_thread.messages).then(|| {
                    "legacy active_thread differed from turns + context; exact model context was preserved"
                        .to_string()
                }),
            },
        );
    }
    (header, facts)
}

fn append_legacy_turn(
    facts: &mut Vec<SessionFactEnvelope>,
    revision: &mut u64,
    index: usize,
    record: &TurnRecord,
) {
    let operation_id = format!("legacy-operation-{index}");
    let turn_id = format!("legacy-turn-{index}");
    let model = record.turn.model.clone().unwrap_or_else(imported_model);
    push_fact(
        facts,
        revision,
        Some(operation_id.clone()),
        Some(turn_id.clone()),
        SessionFact::TurnStarted {
            user_message: record.turn.user_message.clone(),
            model,
            permissions: PermissionProfile::default(),
            // 旧聚合文档没有记录 system prompt，迁移时留空。
            system_prompt: String::new(),
        },
    );
    let mut model_index = 0usize;
    for message in record.messages.iter().skip(1) {
        match message.role {
            Role::Assistant => {
                let model_call_id = format!("legacy-model-{index}-{model_index}");
                model_index += 1;
                push_fact(
                    facts,
                    revision,
                    Some(operation_id.clone()),
                    Some(turn_id.clone()),
                    SessionFact::ModelCallStarted {
                        model_call_id: model_call_id.clone(),
                    },
                );
                push_fact(
                    facts,
                    revision,
                    Some(operation_id.clone()),
                    Some(turn_id.clone()),
                    SessionFact::ModelMessageCommitted {
                        model_call_id,
                        message: message.clone(),
                    },
                );
                for tool_call in message.tool_calls.iter().flatten() {
                    push_fact(
                        facts,
                        revision,
                        Some(operation_id.clone()),
                        Some(turn_id.clone()),
                        SessionFact::ToolCallStarted {
                            tool_call: tool_call.clone(),
                        },
                    );
                }
            }
            Role::Tool => {
                if let Some(tool_call_id) = message.tool_call_id.as_ref() {
                    let step =
                        record.turn.steps.iter().find(|step| {
                            step.tool_call_id.as_deref() == Some(tool_call_id.as_str())
                        });
                    let ok = step.is_none_or(|step| step.status == TurnStatus::Completed);
                    push_fact(
                        facts,
                        revision,
                        Some(operation_id.clone()),
                        Some(turn_id.clone()),
                        SessionFact::ToolCallFinished {
                            tool_call_id: tool_call_id.clone(),
                            result: message.clone(),
                            ok,
                            summary: None,
                        },
                    );
                }
            }
            Role::System | Role::User => {}
        }
    }
    let terminal = match record.turn.status {
        TurnStatus::Completed => SessionFact::TurnCompleted,
        TurnStatus::Failed => SessionFact::TurnFailed {
            error: record
                .turn
                .error
                .clone()
                .unwrap_or_else(|| "legacy turn failed".to_string()),
        },
        TurnStatus::Running => SessionFact::TurnInterrupted {
            reason: "legacy running turn was interrupted during migration".to_string(),
        },
    };
    push_fact(facts, revision, Some(operation_id), Some(turn_id), terminal);
}

fn push_fact(
    facts: &mut Vec<SessionFactEnvelope>,
    revision: &mut u64,
    operation_id: Option<String>,
    turn_id: Option<String>,
    fact: SessionFact,
) {
    *revision += 1;
    facts.push(SessionFactEnvelope {
        revision: *revision,
        timestamp_ms: timestamp_ms().saturating_add(*revision),
        operation_id,
        turn_id,
        fact,
    });
}

fn imported_model() -> ModelInvocation {
    ModelInvocation {
        provider_id: "legacy".to_string(),
        provider_name: "Legacy import".to_string(),
        model_id: "unknown".to_string(),
        model_name: "Unknown model".to_string(),
        reasoning: ReasoningLevel::Off,
    }
}

fn remove_if_exists(path: &Path) -> Result<bool, SessionStoreError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(SessionStoreError::Remove {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn validate_session_name(name: &str) -> Result<(), SessionStoreError> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(SessionStoreError::InvalidSessionName {
            name: name.to_string(),
        });
    }
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::{Message, SessionContext, Thread};
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = env::temp_dir().join(format!("morrow-session-store-{name}-{stamp}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn make_store(root: &Path, legacy: &Path, cwd: &Path, name: &str) -> SessionStore {
        SessionStore::new(root, legacy, cwd, name).expect("store")
    }

    fn sample_session() -> Session {
        Session::from_thread(Thread {
            messages: vec![Message::user("Hello"), Message::assistant("Hi")],
        })
    }

    #[test]
    fn saves_v7_jsonl_and_loads_projected_context() {
        let root = unique_dir("save");
        let legacy = unique_dir("save-legacy");
        let cwd = unique_dir("save-cwd");
        let store = make_store(&root, &legacy, &cwd, "default");

        store.save(&sample_session()).expect("save");

        assert_eq!(
            store.path().extension().and_then(|value| value.to_str()),
            Some("jsonl")
        );
        let (header, facts) = store.load_log().expect("load log");
        assert_eq!(header.schema_version, 7);
        assert!(!facts.is_empty());
        assert_eq!(store.load().expect("load"), sample_session());
    }

    fn assert_header_upgrade_preserves_facts(name: &str, from_version: u32) {
        let root = unique_dir(name);
        let legacy = unique_dir(&format!("{name}-legacy"));
        let cwd = unique_dir(&format!("{name}-cwd"));
        let store = make_store(&root, &legacy, &cwd, "default");
        store.save(&sample_session()).expect("save");

        let original = fs::read(store.path()).expect("read saved log");
        let newline = original
            .iter()
            .position(|byte| *byte == b'\n')
            .expect("header newline");
        let mut header: SessionLogHeader =
            serde_json::from_slice(&original[..newline]).expect("parse header");
        header.schema_version = from_version;
        let mut legacy_bytes = serde_json::to_vec(&header).expect("serialize old header");
        legacy_bytes.push(b'\n');
        legacy_bytes.extend_from_slice(&original[newline + 1..]);
        fs::write(store.path(), &legacy_bytes).expect("write old log");

        assert_eq!(
            store
                .load_log()
                .expect("read compatible old log")
                .0
                .schema_version,
            from_version
        );
        let lease = store.acquire_writer().expect("lease");
        store.ensure_v5(&lease).expect("upgrade");

        let upgraded = fs::read(store.path()).expect("read upgraded log");
        let upgraded_newline = upgraded
            .iter()
            .position(|byte| *byte == b'\n')
            .expect("upgraded header newline");
        let upgraded_header: SessionLogHeader =
            serde_json::from_slice(&upgraded[..upgraded_newline]).expect("parse upgraded header");
        assert_eq!(
            upgraded_header.schema_version,
            SESSION_DOCUMENT_SCHEMA_VERSION
        );
        assert_eq!(
            &upgraded[upgraded_newline + 1..],
            &legacy_bytes[newline + 1..]
        );
        assert_eq!(store.load().expect("load upgraded"), sample_session());
    }

    #[test]
    fn upgrades_v5_jsonl_header_without_rewriting_facts() {
        assert_header_upgrade_preserves_facts("upgrade-v5", 5);
    }

    #[test]
    fn upgrades_v6_jsonl_header_without_rewriting_facts() {
        assert_header_upgrade_preserves_facts("upgrade-v6", 6);
    }

    #[test]
    fn migrates_v4_and_preserves_mismatched_active_context() {
        let root = unique_dir("migrate");
        let legacy = unique_dir("migrate-legacy");
        let cwd = unique_dir("migrate-cwd");
        let store = make_store(&root, &legacy, &cwd, "default");
        fs::create_dir_all(store.legacy_session_path().parent().expect("parent"))
            .expect("create parent");
        fs::write(
            store.legacy_session_path(),
            json!({
                "schema_version": 4,
                "session": {
                    "active_thread": {"messages": [{"role": "user", "content": "exact"}]},
                    "turns": [],
                    "context": {"summarized_turns": 0}
                }
            })
            .to_string(),
        )
        .expect("write legacy");

        let lease = store.acquire_writer().expect("lease");
        store.ensure_v5(&lease).expect("migrate");
        let projection = store.load_projection().expect("projection");

        assert!(projection.context.legacy_checkpoint);
        assert_eq!(projection.context.messages, vec![Message::user("exact")]);
        assert!(store.scope_dir().join("default.legacy-v4.bak").is_file());
        assert!(!store.legacy_session_path().exists());
    }

    #[test]
    fn append_requires_contiguous_revision() {
        let root = unique_dir("revision");
        let legacy = unique_dir("revision-legacy");
        let cwd = unique_dir("revision-cwd");
        let store = make_store(&root, &legacy, &cwd, "default");
        let lease = store.acquire_writer().expect("lease");
        store.ensure_v5(&lease).expect("initialize");

        let error = store
            .append_fact(
                &lease,
                1,
                None,
                None,
                SessionFact::LegacyContextCheckpoint {
                    source_schema: 2,
                    messages: Vec::new(),
                    diagnostic: None,
                },
            )
            .expect_err("revision conflict");

        assert!(matches!(error, SessionStoreError::InvalidLog { .. }));
    }

    #[test]
    fn second_writer_is_rejected_until_lease_is_dropped() {
        let root = unique_dir("lease");
        let legacy = unique_dir("lease-legacy");
        let cwd = unique_dir("lease-cwd");
        let store = make_store(&root, &legacy, &cwd, "default");
        let lease = store.acquire_writer().expect("first lease");

        assert!(matches!(
            store.acquire_writer(),
            Err(SessionStoreError::WriterBusy { .. })
        ));
        drop(lease);
        store.acquire_writer().expect("lease after drop");
    }

    #[test]
    fn torn_final_line_is_ignored_but_interior_corruption_fails() {
        let root = unique_dir("torn");
        let legacy = unique_dir("torn-legacy");
        let cwd = unique_dir("torn-cwd");
        let store = make_store(&root, &legacy, &cwd, "default");
        store.save(&Session::new()).expect("save");
        let mut file = OpenOptions::new()
            .append(true)
            .open(store.path())
            .expect("open");
        file.write_all(b"{\"revision\":").expect("write torn");
        assert!(store.load_projection().is_ok());
        drop(file);

        let lease = store.acquire_writer().expect("lease");
        store.ensure_v5(&lease).expect("repair torn tail");
        let revision = store
            .load_projection()
            .expect("repaired projection")
            .revision;
        store
            .append_fact(
                &lease,
                revision,
                None,
                None,
                SessionFact::LegacyContextCheckpoint {
                    source_schema: 4,
                    messages: vec![Message::user("after repair")],
                    diagnostic: None,
                },
            )
            .expect("append after repair");
        assert_eq!(
            store.load_projection().expect("load repaired log").revision,
            revision + 1
        );
        drop(lease);

        let header = new_header();
        let bytes = format!(
            "{}\nnot-json\n",
            serde_json::to_string(&header).expect("header")
        );
        fs::write(store.path(), bytes).expect("write corrupt");
        assert!(matches!(
            store.load_projection(),
            Err(SessionStoreError::Parse { .. })
        ));
    }

    #[test]
    fn reset_creates_new_session_incarnation() {
        let root = unique_dir("reset");
        let legacy = unique_dir("reset-legacy");
        let cwd = unique_dir("reset-cwd");
        let store = make_store(&root, &legacy, &cwd, "default");
        store.save(&sample_session()).expect("save");
        let before = store.load_projection().expect("before").session_id;

        let after = store.reset().expect("reset");

        assert_ne!(before, after.session_id);
        assert_eq!(after.revision, 0);
        assert!(after.turns.is_empty());
    }

    #[test]
    fn reset_removes_old_session_artifacts_and_uses_a_new_root() {
        let root = unique_dir("reset-artifacts");
        let legacy = unique_dir("reset-artifacts-legacy");
        let cwd = unique_dir("reset-artifacts-cwd");
        let store = make_store(&root, &legacy, &cwd, "default");
        store.save(&sample_session()).expect("save");
        let old_artifact_root = store.artifact_root().expect("old artifact root");
        fs::create_dir_all(old_artifact_root.join("web_fetch")).expect("create artifacts");
        fs::write(
            old_artifact_root.join("web_fetch").join("page.md"),
            "private artifact",
        )
        .expect("write artifact");

        store.reset().expect("reset");

        let new_artifact_root = store.artifact_root().expect("new artifact root");
        assert_ne!(old_artifact_root, new_artifact_root);
        assert!(!old_artifact_root.exists());
        assert!(!new_artifact_root.exists());
    }

    #[test]
    fn artifacts_follow_session_identity_without_entering_exports() {
        let root = unique_dir("artifact-lifecycle");
        let legacy = unique_dir("artifact-lifecycle-legacy");
        let cwd = unique_dir("artifact-lifecycle-cwd");
        let store = make_store(&root, &legacy, &cwd, "default");
        store.save(&sample_session()).expect("save");
        let artifact_root = store.artifact_root().expect("artifact root");
        let session_id = store.load_projection().expect("projection").session_id;
        assert_eq!(
            artifact_root,
            store
                .scope_dir()
                .join("artifacts")
                .join(hex_encode(session_id.as_bytes()))
        );
        fs::create_dir_all(artifact_root.join("web_fetch")).expect("create artifacts");
        fs::write(
            artifact_root.join("web_fetch").join("page.md"),
            "artifact-only-secret",
        )
        .expect("write artifact");

        let renamed = store.rename("renamed").expect("rename");
        assert_eq!(
            renamed.artifact_root().expect("renamed root"),
            artifact_root
        );
        assert!(artifact_root.exists());
        renamed.archive().expect("archive");
        assert_eq!(
            renamed.artifact_root().expect("archived root"),
            artifact_root
        );
        assert!(artifact_root.exists());
        renamed.restore().expect("restore");
        assert_eq!(
            renamed.artifact_root().expect("restored root"),
            artifact_root
        );
        let export = String::from_utf8(renamed.export_document_bytes().expect("export"))
            .expect("UTF-8 JSONL");
        assert!(!export.contains("artifact-only-secret"));

        renamed.delete().expect("delete");
        assert!(!artifact_root.exists());
    }

    #[test]
    fn old_thread_documents_load_without_mutating_until_writer_open() {
        let root = unique_dir("thread");
        let legacy = unique_dir("thread-legacy");
        let cwd = unique_dir("thread-cwd");
        let store = make_store(&root, &legacy, &cwd, "default");
        fs::create_dir_all(store.legacy_path().parent().expect("parent")).expect("create parent");
        let legacy_bytes = json!({
            "schema_version": 2,
            "thread": {"messages": [{"role": "user", "content": "Hello"}]}
        })
        .to_string();
        fs::write(store.legacy_path(), &legacy_bytes).expect("write legacy thread");

        assert_eq!(
            store.load().expect("load").active_thread.messages,
            vec![Message::user("Hello")]
        );
        assert!(!store.path().exists());
        let lease = store.acquire_writer().expect("lease");
        store.ensure_v5(&lease).expect("migrate");
        assert!(store.path().exists());
        assert!(!store.legacy_path().exists());
        assert_eq!(
            fs::read_to_string(store.scope_dir().join("default.legacy-v2.bak"))
                .expect("read migration backup"),
            legacy_bytes,
        );
    }

    #[test]
    fn migration_backup_conflict_leaves_legacy_source_unchanged() {
        let root = unique_dir("migration-conflict");
        let legacy = unique_dir("migration-conflict-legacy");
        let cwd = unique_dir("migration-conflict-cwd");
        let store = make_store(&root, &legacy, &cwd, "default");
        fs::create_dir_all(store.legacy_path().parent().expect("parent")).expect("create parent");
        let legacy_bytes = json!({
            "schema_version": 2,
            "thread": {"messages": [{"role": "user", "content": "keep me"}]}
        })
        .to_string();
        fs::write(store.legacy_path(), &legacy_bytes).expect("write legacy thread");
        fs::create_dir_all(store.scope_dir()).expect("create scope");
        fs::write(
            store.scope_dir().join("default.legacy-v2.bak"),
            "existing backup",
        )
        .expect("write conflicting backup");

        let lease = store.acquire_writer().expect("lease");
        assert!(matches!(
            store.ensure_v5(&lease),
            Err(SessionStoreError::TargetExists { .. })
        ));
        assert!(!store.path().exists());
        assert_eq!(
            fs::read_to_string(store.legacy_path()).expect("read legacy source"),
            legacy_bytes,
        );
    }

    #[test]
    fn compatibility_save_preserves_compaction_context() {
        let root = unique_dir("compact");
        let legacy = unique_dir("compact-legacy");
        let cwd = unique_dir("compact-cwd");
        let store = make_store(&root, &legacy, &cwd, "default");
        let session = Session {
            active_thread: Thread {
                messages: vec![Message::system("Session summary:\nsummary")],
            },
            turns: Vec::new(),
            context: SessionContext {
                summary: Some("summary".to_string()),
                summarized_turns: 0,
            },
        };

        store.save(&session).expect("save");

        assert_eq!(
            store.load().expect("load").active_thread,
            session.active_thread
        );
    }
}
