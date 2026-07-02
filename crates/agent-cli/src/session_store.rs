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
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
    legacy_path: PathBuf,
}

impl SessionStore {
    pub fn for_current_dir(session_name: &str) -> Result<Self, SessionStoreError> {
        let home = dirs::home_dir().ok_or(SessionStoreError::HomeDirNotFound)?;
        let cwd = env::current_dir().map_err(SessionStoreError::CurrentDir)?;
        Self::new(
            home.join(".morrow").join("sessions"),
            home.join(".morrow").join("threads"),
            &cwd,
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
        Ok(Session::new())
    }

    pub fn save(&self, session: &Session) -> Result<(), SessionStoreError> {
        let parent = self.path.parent().expect("session path must have parent");
        fs::create_dir_all(parent).map_err(|source| SessionStoreError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;

        let document = SessionDocument::new(session.clone());
        let content = serde_json::to_vec_pretty(&document).map_err(|source| {
            SessionStoreError::Serialize {
                path: self.path.clone(),
                source,
            }
        })?;
        let temp_path = self.temp_path();

        fs::write(&temp_path, content).map_err(|source| SessionStoreError::Write {
            path: temp_path.clone(),
            source,
        })?;
        fs::rename(&temp_path, &self.path).map_err(|source| SessionStoreError::Replace {
            path: self.path.clone(),
            source,
        })?;

        Ok(())
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    fn legacy_path(&self) -> &Path {
        &self.legacy_path
    }

    fn new(
        root: impl Into<PathBuf>,
        legacy_root: impl Into<PathBuf>,
        cwd: &Path,
        session_name: &str,
    ) -> Result<Self, SessionStoreError> {
        validate_session_name(session_name)?;
        let canonical_cwd =
            cwd.canonicalize()
                .map_err(|source| SessionStoreError::CanonicalizeCwd {
                    path: cwd.to_path_buf(),
                    source,
                })?;
        let scope = hex_encode(canonical_cwd.as_os_str().as_encoded_bytes());
        let file_name = format!("{session_name}.json");
        let path = root.into().join(&scope).join(&file_name);
        let legacy_path = legacy_root.into().join(scope).join(file_name);

        Ok(Self { path, legacy_path })
    }

    fn load_path(&self, path: &Path) -> Result<Session, SessionStoreError> {
        let content = fs::read_to_string(path).map_err(|source| SessionStoreError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        parse_session_document(path, &content)
    }

    fn temp_path(&self) -> PathBuf {
        let file_name = self
            .path
            .file_name()
            .expect("session path must have file name")
            .to_string_lossy();
        self.path
            .with_file_name(format!("{file_name}.tmp-{}", std::process::id()))
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
        SESSION_DOCUMENT_SCHEMA_VERSION => {
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
        assert!(saved.contains(r#""schema_version": 3"#));
    }
}
