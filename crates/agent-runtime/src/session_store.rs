use agent_protocol::{
    SESSION_DOCUMENT_SCHEMA_VERSION, Session, SessionDocument, THREAD_DOCUMENT_SCHEMA_VERSION,
    ThreadDocument,
};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

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
    #[error("target session already exists at {path}")]
    TargetExists { path: PathBuf },
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
    legacy_root: PathBuf,
    scope: String,
    session_name: String,
    path: PathBuf,
    legacy_path: PathBuf,
    archived_path: PathBuf,
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
        if self.path.is_file() {
            return self.load_path(&self.path);
        }
        if self.legacy_path.is_file() {
            return self.load_path(&self.legacy_path);
        }
        if self.is_archived() {
            return Err(SessionStoreError::TargetExists {
                path: self.archived_path.clone(),
            });
        }
        Ok(Session::new())
    }

    pub fn load_existing(&self) -> Result<Session, SessionStoreError> {
        if self.path.is_file() {
            return self.load_path(&self.path);
        }
        if self.legacy_path.is_file() {
            return self.load_path(&self.legacy_path);
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

    pub fn save(&self, session: &Session) -> Result<(), SessionStoreError> {
        if self.is_archived() {
            return Err(SessionStoreError::TargetExists {
                path: self.archived_path.clone(),
            });
        }
        self.save_to_path(&self.path, session)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    fn legacy_path(&self) -> &Path {
        &self.legacy_path
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

    pub fn is_archived(&self) -> bool {
        self.archived_path.is_file()
    }

    pub fn archive(&self) -> Result<(), SessionStoreError> {
        if self.is_archived() {
            return Err(SessionStoreError::TargetExists {
                path: self.archived_path.clone(),
            });
        }

        let session = self.load_existing()?;
        self.save_to_path(&self.archived_path, &session)?;
        let _ = remove_if_exists(&self.path)?;
        let _ = remove_if_exists(&self.legacy_path)?;
        Ok(())
    }

    pub fn restore(&self) -> Result<(), SessionStoreError> {
        if !self.is_archived() {
            return Err(SessionStoreError::SessionNotFound {
                name: self.session_name.clone(),
            });
        }
        if self.path.is_file() {
            return Err(SessionStoreError::TargetExists {
                path: self.path.clone(),
            });
        }
        if self.legacy_path.is_file() {
            return Err(SessionStoreError::TargetExists {
                path: self.legacy_path.clone(),
            });
        }

        let session = self.load_path(&self.archived_path)?;
        self.save_to_path(&self.path, &session)?;
        let _ = remove_if_exists(&self.archived_path)?;
        Ok(())
    }

    pub fn delete(&self) -> Result<(), SessionStoreError> {
        let removed_primary = remove_if_exists(&self.path)?;
        let removed_legacy = remove_if_exists(&self.legacy_path)?;
        let removed_archived = remove_if_exists(&self.archived_path)?;
        if !removed_primary && !removed_legacy && !removed_archived {
            return Err(SessionStoreError::SessionNotFound {
                name: self.session_name.clone(),
            });
        }

        Ok(())
    }

    pub fn rename(&self, target_name: &str) -> Result<SessionStore, SessionStoreError> {
        let target = self.store_for_name(target_name)?;
        if target.path.is_file() {
            return Err(SessionStoreError::TargetExists { path: target.path });
        }
        if target.legacy_path.is_file() {
            return Err(SessionStoreError::TargetExists {
                path: target.legacy_path,
            });
        }
        if target.archived_path.is_file() {
            return Err(SessionStoreError::TargetExists {
                path: target.archived_path,
            });
        }

        let session = self.load_existing()?;
        target.save(&session)?;
        let _ = remove_if_exists(&self.path)?;
        let _ = remove_if_exists(&self.legacy_path)?;

        Ok(target)
    }

    pub fn export_document_bytes(&self) -> Result<Vec<u8>, SessionStoreError> {
        let document = SessionDocument::new(self.load_existing()?);
        serde_json::to_vec_pretty(&document).map_err(|source| SessionStoreError::Serialize {
            path: self.path.clone(),
            source,
        })
    }

    fn new(
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
        let file_name = format!("{session_name}.json");
        let path = root.join(&scope).join(&file_name);
        let legacy_path = legacy_root.join(&scope).join(&file_name);
        let archived_path = root.join(&scope).join("archive").join(&file_name);

        Ok(Self {
            root,
            legacy_root,
            scope,
            session_name: session_name.to_string(),
            path,
            legacy_path,
            archived_path,
        })
    }

    fn load_path(&self, path: &Path) -> Result<Session, SessionStoreError> {
        let content = fs::read_to_string(path).map_err(|source| SessionStoreError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        parse_session_document(path, &content)
    }

    fn save_to_path(&self, path: &Path, session: &Session) -> Result<(), SessionStoreError> {
        let parent = path.parent().expect("session path must have parent");
        fs::create_dir_all(parent).map_err(|source| SessionStoreError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;

        let document = SessionDocument::new(session.clone());
        let content = serde_json::to_vec_pretty(&document).map_err(|source| {
            SessionStoreError::Serialize {
                path: path.to_path_buf(),
                source,
            }
        })?;
        let temp_path = Self::temp_path(path);

        fs::write(&temp_path, content).map_err(|source| SessionStoreError::Write {
            path: temp_path.clone(),
            source,
        })?;
        fs::rename(&temp_path, path).map_err(|source| SessionStoreError::Replace {
            path: path.to_path_buf(),
            source,
        })?;

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
            if !metadata.is_file()
                || path.extension().and_then(|value| value.to_str()) != Some("json")
            {
                continue;
            }
            let Some(name) = path
                .file_stem()
                .and_then(|value| value.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let session = self.load_path(&path)?;
            entries.push(SessionEntry {
                name,
                path,
                turns: session.turns.len(),
                active_messages: session.active_thread.messages.len(),
                summarized_turns: session.context.summarized_turns,
                has_summary: session.context.summary.is_some(),
            });
        }
        Ok(())
    }

    fn temp_path(path: &Path) -> PathBuf {
        let file_name = path
            .file_name()
            .expect("session path must have file name")
            .to_string_lossy();
        path.with_file_name(format!("{file_name}.tmp-{}", std::process::id()))
    }

    fn scope_dir(&self) -> PathBuf {
        self.root.join(&self.scope)
    }

    fn archive_dir(&self) -> PathBuf {
        self.scope_dir().join("archive")
    }

    fn store_for_name(&self, session_name: &str) -> Result<Self, SessionStoreError> {
        validate_session_name(session_name)?;
        let file_name = format!("{session_name}.json");
        Ok(Self {
            root: self.root.clone(),
            legacy_root: self.legacy_root.clone(),
            scope: self.scope.clone(),
            session_name: session_name.to_string(),
            path: self.root.join(&self.scope).join(&file_name),
            legacy_path: self.legacy_root.join(&self.scope).join(&file_name),
            archived_path: self.root.join(&self.scope).join("archive").join(file_name),
        })
    }
}

fn remove_if_exists(path: &Path) -> Result<bool, SessionStoreError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(SessionStoreError::Remove {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn parse_session_document(path: &Path, content: &str) -> Result<Session, SessionStoreError> {
    let value = serde_json::from_str::<serde_json::Value>(content).map_err(|source| {
        SessionStoreError::Parse {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "missing schema_version",
            ))
        })
        .map_err(|source| SessionStoreError::Parse {
            path: path.to_path_buf(),
            source,
        })? as u32;

    match version {
        3 | SESSION_DOCUMENT_SCHEMA_VERSION => {
            let document = serde_json::from_value::<SessionDocument>(value).map_err(|source| {
                SessionStoreError::Parse {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
            Ok(document.session)
        }
        1 | THREAD_DOCUMENT_SCHEMA_VERSION => {
            let document = serde_json::from_value::<ThreadDocument>(value).map_err(|source| {
                SessionStoreError::Parse {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
            Ok(Session::from_thread(document.thread))
        }
        _ => Err(SessionStoreError::UnsupportedSchemaVersion {
            path: path.to_path_buf(),
            version,
            expected: SESSION_DOCUMENT_SCHEMA_VERSION,
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
    use agent_protocol::{Message, Thread};
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

    fn make_store(root: &Path, legacy_root: &Path, cwd: &Path, session_name: &str) -> SessionStore {
        SessionStore::new(root, legacy_root, cwd, session_name).expect("session store")
    }

    fn sample_thread() -> Thread {
        let mut thread = Thread::new();
        thread.push(Message::user("Hello"));
        thread.push(Message::assistant("Hi"));
        thread
    }

    #[test]
    fn missing_file_loads_empty_session() {
        let root = unique_dir("missing-root");
        let legacy_root = unique_dir("missing-legacy-root");
        let cwd = unique_dir("missing-cwd");
        let store = make_store(&root, &legacy_root, &cwd, "default");

        let session = store.load().expect("load session");

        assert_eq!(session, Session::new());
    }

    #[test]
    fn save_then_load_round_trips_across_store_instances() {
        let root = unique_dir("roundtrip-root");
        let legacy_root = unique_dir("roundtrip-legacy-root");
        let cwd = unique_dir("roundtrip-cwd");
        let store = make_store(&root, &legacy_root, &cwd, "default");
        let session = Session::from_thread(sample_thread());

        store.save(&session).expect("save session");
        let loaded = make_store(&root, &legacy_root, &cwd, "default")
            .load()
            .expect("load saved session");

        assert_eq!(loaded, session);
    }

    #[test]
    fn explicit_workspace_scope_is_canonicalized() {
        let workspace = unique_dir("explicit-workspace");
        let direct = SessionStore::for_workspace(&workspace, "default").expect("explicit store");
        let dotted =
            SessionStore::for_workspace(&workspace.join("."), "default").expect("canonical store");

        assert_eq!(direct.path(), dotted.path());
    }

    #[test]
    fn identical_session_names_are_isolated_between_workspaces() {
        let root = unique_dir("workspace-isolation-root");
        let legacy = unique_dir("workspace-isolation-legacy");
        let first_workspace = unique_dir("workspace-isolation-a");
        let second_workspace = unique_dir("workspace-isolation-b");
        let first = SessionStore::new(&root, &legacy, &first_workspace, "work")
            .expect("first workspace store");
        let second = SessionStore::new(&root, &legacy, &second_workspace, "work")
            .expect("second workspace store");

        let mut first_session = Session::new();
        first_session
            .active_thread
            .messages
            .push(Message::user("first workspace"));
        first.save(&first_session).expect("save first session");

        assert_ne!(first.path(), second.path());
        assert_eq!(first.load_existing().expect("load first"), first_session);
        assert!(matches!(
            second.load_existing(),
            Err(SessionStoreError::SessionNotFound { .. })
        ));
    }

    #[test]
    fn loads_v1_thread_documents_for_compatibility_from_legacy_path() {
        let root = unique_dir("v1-root");
        let legacy_root = unique_dir("v1-legacy-root");
        let cwd = unique_dir("v1-cwd");
        let store = make_store(&root, &legacy_root, &cwd, "default");
        let path = store.legacy_path().to_path_buf();
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        let content = json!({
            "schema_version": 1,
            "thread": {
                "messages": [
                    {"role": "user", "content": "Hello"},
                    {"role": "assistant", "content": "Hi"}
                ]
            }
        })
        .to_string();
        fs::write(&path, content).expect("write v1 document");

        let loaded = store.load().expect("load v1 document");

        assert_eq!(loaded, Session::from_thread(sample_thread()));
    }

    #[test]
    fn loads_v3_session_documents_and_upgrades_on_save() {
        let root = unique_dir("v3-root");
        let legacy_root = unique_dir("v3-legacy-root");
        let cwd = unique_dir("v3-cwd");
        let store = make_store(&root, &legacy_root, &cwd, "default");
        let path = store.path().to_path_buf();
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(
            &path,
            json!({
                "schema_version": 3,
                "session": {
                    "active_thread": {"messages": [{"role": "user", "content": "Hello"}]},
                    "turns": [],
                    "context": {"summarized_turns": 0}
                }
            })
            .to_string(),
        )
        .expect("write v3 session");

        let session = store.load().expect("load v3");
        store.save(&session).expect("save v4");

        assert_eq!(session.active_thread.messages, vec![Message::user("Hello")]);
        let saved = fs::read_to_string(path).expect("read upgraded session");
        assert!(saved.contains(r#""schema_version": 4"#));
    }

    #[test]
    fn rejects_invalid_session_names() {
        let root = unique_dir("invalid-root");
        let legacy_root = unique_dir("invalid-legacy-root");
        let cwd = unique_dir("invalid-cwd");

        for name in ["", "../escape", "a/b", "with.dot", "space name", "中文"] {
            assert!(matches!(
                SessionStore::new(&root, &legacy_root, &cwd, name),
                Err(SessionStoreError::InvalidSessionName { .. })
            ));
        }
    }

    #[test]
    fn malformed_json_reports_error_and_keeps_file() {
        let root = unique_dir("malformed-root");
        let legacy_root = unique_dir("malformed-legacy-root");
        let cwd = unique_dir("malformed-cwd");
        let store = make_store(&root, &legacy_root, &cwd, "default");
        let path = store.path().to_path_buf();
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(&path, "{not-json").expect("write malformed json");

        let err = store.load().expect_err("load must fail");

        assert!(matches!(err, SessionStoreError::Parse { .. }));
        assert_eq!(fs::read_to_string(path).expect("read file"), "{not-json");
    }

    #[test]
    fn unsupported_schema_version_reports_error_and_keeps_file() {
        let root = unique_dir("schema-root");
        let legacy_root = unique_dir("schema-legacy-root");
        let cwd = unique_dir("schema-cwd");
        let store = make_store(&root, &legacy_root, &cwd, "default");
        let path = store.path().to_path_buf();
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        let content = json!({
            "schema_version": 999,
            "session": {"active_thread": {"messages": []}, "turns": [], "context": {"summarized_turns": 0}}
        })
        .to_string();
        fs::write(&path, &content).expect("write unsupported schema");

        let err = store.load().expect_err("load must fail");

        assert!(matches!(
            err,
            SessionStoreError::UnsupportedSchemaVersion { version: 999, .. }
        ));
        assert_eq!(fs::read_to_string(path).expect("read file"), content);
    }

    #[test]
    fn reset_style_save_overwrites_existing_session() {
        let root = unique_dir("reset-root");
        let legacy_root = unique_dir("reset-legacy-root");
        let cwd = unique_dir("reset-cwd");
        let store = make_store(&root, &legacy_root, &cwd, "default");
        store
            .save(&Session::from_thread(sample_thread()))
            .expect("save existing session");

        let mut reset_thread = Thread::new();
        reset_thread.push(Message::user("New"));
        reset_thread.push(Message::assistant("History"));
        let reset_session = Session::from_thread(reset_thread);
        store.save(&reset_session).expect("save reset session");

        assert_eq!(store.load().expect("load reset session"), reset_session);
    }

    #[test]
    fn migrated_legacy_thread_saves_to_sessions_path() {
        let root = unique_dir("migrate-root");
        let legacy_root = unique_dir("migrate-legacy-root");
        let cwd = unique_dir("migrate-cwd");
        let store = make_store(&root, &legacy_root, &cwd, "default");
        let legacy_path = store.legacy_path().to_path_buf();
        fs::create_dir_all(legacy_path.parent().expect("parent")).expect("create parent");
        fs::write(
            &legacy_path,
            json!({
                "schema_version": 2,
                "thread": {"messages": [{"role": "user", "content": "Hello"}]}
            })
            .to_string(),
        )
        .expect("write legacy");

        let session = store.load().expect("load legacy");
        store.save(&session).expect("save migrated session");

        assert!(store.path().is_file());
        assert!(store.legacy_path().is_file());
        let saved = fs::read_to_string(store.path()).expect("read saved session");
        assert!(saved.contains(r#""schema_version": 4"#));
    }

    #[test]
    fn lists_primary_sessions_for_current_scope() {
        let root = unique_dir("list-root");
        let legacy_root = unique_dir("list-legacy-root");
        let cwd = unique_dir("list-cwd");
        make_store(&root, &legacy_root, &cwd, "work")
            .save(&Session::from_thread(sample_thread()))
            .expect("save work");
        make_store(&root, &legacy_root, &cwd, "default")
            .save(&Session::new())
            .expect("save default");
        let other_cwd = unique_dir("list-other-cwd");
        make_store(&root, &legacy_root, &other_cwd, "other")
            .save(&Session::new())
            .expect("save other scope");
        let store = make_store(&root, &legacy_root, &cwd, "default");

        let entries = store.list_current_scope().expect("list sessions");

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["default", "work"]
        );
        let work = entries
            .iter()
            .find(|entry| entry.name == "work")
            .expect("work entry");
        assert_eq!(work.active_messages, 2);
        assert_eq!(work.turns, 0);
    }

    #[test]
    fn archive_hides_session_from_active_list_and_restore_recovers_it() {
        let root = unique_dir("archive-root");
        let legacy_root = unique_dir("archive-legacy-root");
        let cwd = unique_dir("archive-cwd");
        let store = make_store(&root, &legacy_root, &cwd, "work");
        let session = Session::from_thread(sample_thread());
        store.save(&session).expect("save session");

        store.archive().expect("archive session");

        assert!(!store.path().exists());
        assert!(store.is_archived());
        assert!(
            store
                .list_current_scope()
                .expect("list active sessions")
                .is_empty()
        );
        let entries = store
            .list_current_scope_with_archived()
            .expect("list all sessions");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session.name, "work");
        assert!(entries[0].archived);

        store.restore().expect("restore session");

        assert!(!store.is_archived());
        assert_eq!(
            store.load_existing().expect("load restored session"),
            session
        );
        assert_eq!(
            store
                .list_current_scope()
                .expect("list restored session")
                .len(),
            1
        );
    }

    #[test]
    fn delete_removes_primary_and_legacy_session_files() {
        let root = unique_dir("delete-root");
        let legacy_root = unique_dir("delete-legacy-root");
        let cwd = unique_dir("delete-cwd");
        let store = make_store(&root, &legacy_root, &cwd, "default");
        store
            .save(&Session::from_thread(sample_thread()))
            .expect("save primary");
        fs::create_dir_all(store.legacy_path().parent().expect("legacy parent"))
            .expect("create legacy parent");
        fs::write(
            store.legacy_path(),
            json!({"schema_version": 2, "thread": {"messages": []}}).to_string(),
        )
        .expect("write legacy");

        store.delete().expect("delete session");

        assert!(!store.path().exists());
        assert!(!store.legacy_path().exists());
        assert!(matches!(
            store.delete(),
            Err(SessionStoreError::SessionNotFound { .. })
        ));
    }

    #[test]
    fn rename_saves_target_and_removes_source_files() {
        let root = unique_dir("rename-root");
        let legacy_root = unique_dir("rename-legacy-root");
        let cwd = unique_dir("rename-cwd");
        let source = make_store(&root, &legacy_root, &cwd, "old");
        let session = Session::from_thread(sample_thread());
        source.save(&session).expect("save source");

        let target = source.rename("new").expect("rename session");

        assert!(!source.path().exists());
        assert_eq!(target.load().expect("load target"), session);
    }

    #[test]
    fn rename_fails_when_target_exists_and_preserves_source() {
        let root = unique_dir("rename-target-root");
        let legacy_root = unique_dir("rename-target-legacy-root");
        let cwd = unique_dir("rename-target-cwd");
        let source = make_store(&root, &legacy_root, &cwd, "old");
        let target = make_store(&root, &legacy_root, &cwd, "new");
        source
            .save(&Session::from_thread(sample_thread()))
            .expect("save source");
        target.save(&Session::new()).expect("save target");

        let err = source.rename("new").expect_err("rename must fail");

        assert!(matches!(err, SessionStoreError::TargetExists { .. }));
        assert!(source.path().exists());
        assert!(target.path().exists());
    }

    #[test]
    fn export_document_bytes_outputs_current_schema_document() {
        let root = unique_dir("export-root");
        let legacy_root = unique_dir("export-legacy-root");
        let cwd = unique_dir("export-cwd");
        let store = make_store(&root, &legacy_root, &cwd, "default");
        let session = Session::from_thread(sample_thread());
        store.save(&session).expect("save session");

        let bytes = store.export_document_bytes().expect("export document");
        let document =
            serde_json::from_slice::<SessionDocument>(&bytes).expect("parse exported document");

        assert_eq!(document.schema_version, SESSION_DOCUMENT_SCHEMA_VERSION);
        assert_eq!(document.session, session);
    }
}
