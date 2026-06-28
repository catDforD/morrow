use agent_protocol::{THREAD_DOCUMENT_SCHEMA_VERSION, Thread, ThreadDocument};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ThreadStoreError {
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
    #[error("invalid thread name {name:?}; use ASCII letters, digits, '-' or '_'")]
    InvalidThreadName { name: String },
    #[error("failed to read thread file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse thread file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("unsupported thread document schema version {version} in {path}; expected {expected}")]
    UnsupportedSchemaVersion {
        path: PathBuf,
        version: u32,
        expected: u32,
    },
    #[error("failed to create thread directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize thread file {path}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write thread file {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to replace thread file {path}: {source}")]
    Replace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone)]
pub struct ThreadStore {
    path: PathBuf,
}

impl ThreadStore {
    pub fn for_current_dir(thread_name: &str) -> Result<Self, ThreadStoreError> {
        let home = dirs::home_dir().ok_or(ThreadStoreError::HomeDirNotFound)?;
        let cwd = env::current_dir().map_err(ThreadStoreError::CurrentDir)?;
        Self::new(home.join(".morrow").join("threads"), &cwd, thread_name)
    }

    pub fn load(&self) -> Result<Thread, ThreadStoreError> {
        let content = match fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Thread::new()),
            Err(source) => {
                return Err(ThreadStoreError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };

        let document = serde_json::from_str::<ThreadDocument>(&content).map_err(|source| {
            ThreadStoreError::Parse {
                path: self.path.clone(),
                source,
            }
        })?;

        if document.schema_version != THREAD_DOCUMENT_SCHEMA_VERSION {
            return Err(ThreadStoreError::UnsupportedSchemaVersion {
                path: self.path.clone(),
                version: document.schema_version,
                expected: THREAD_DOCUMENT_SCHEMA_VERSION,
            });
        }

        Ok(document.thread)
    }

    pub fn save(&self, thread: &Thread) -> Result<(), ThreadStoreError> {
        let parent = self.path.parent().expect("thread path must have parent");
        fs::create_dir_all(parent).map_err(|source| ThreadStoreError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;

        let document = ThreadDocument::new(thread.clone());
        let content =
            serde_json::to_vec_pretty(&document).map_err(|source| ThreadStoreError::Serialize {
                path: self.path.clone(),
                source,
            })?;
        let temp_path = self.temp_path();

        fs::write(&temp_path, content).map_err(|source| ThreadStoreError::Write {
            path: temp_path.clone(),
            source,
        })?;
        fs::rename(&temp_path, &self.path).map_err(|source| ThreadStoreError::Replace {
            path: self.path.clone(),
            source,
        })?;

        Ok(())
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.path
    }

    fn new(
        root: impl Into<PathBuf>,
        cwd: &Path,
        thread_name: &str,
    ) -> Result<Self, ThreadStoreError> {
        validate_thread_name(thread_name)?;
        let canonical_cwd =
            cwd.canonicalize()
                .map_err(|source| ThreadStoreError::CanonicalizeCwd {
                    path: cwd.to_path_buf(),
                    source,
                })?;
        let scope = hex_encode(canonical_cwd.as_os_str().as_encoded_bytes());
        let path = root.into().join(scope).join(format!("{thread_name}.json"));

        Ok(Self { path })
    }

    fn temp_path(&self) -> PathBuf {
        let file_name = self
            .path
            .file_name()
            .expect("thread path must have file name")
            .to_string_lossy();
        self.path
            .with_file_name(format!("{file_name}.tmp-{}", std::process::id()))
    }
}

fn validate_thread_name(name: &str) -> Result<(), ThreadStoreError> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ThreadStoreError::InvalidThreadName {
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
    use agent_protocol::Message;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = env::temp_dir().join(format!("morrow-thread-store-{name}-{stamp}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn make_store(root: &Path, cwd: &Path, thread_name: &str) -> ThreadStore {
        ThreadStore::new(root, cwd, thread_name).expect("thread store")
    }

    fn sample_thread() -> Thread {
        let mut thread = Thread::new();
        thread.push(Message::user("Hello"));
        thread.push(Message::assistant("Hi"));
        thread
    }

    #[test]
    fn missing_file_loads_empty_thread() {
        let root = unique_dir("missing-root");
        let cwd = unique_dir("missing-cwd");
        let store = make_store(&root, &cwd, "default");

        let thread = store.load().expect("load thread");

        assert_eq!(thread, Thread::new());
    }

    #[test]
    fn save_then_load_round_trips_across_store_instances() {
        let root = unique_dir("roundtrip-root");
        let cwd = unique_dir("roundtrip-cwd");
        let store = make_store(&root, &cwd, "default");
        let thread = sample_thread();

        store.save(&thread).expect("save thread");
        let loaded = make_store(&root, &cwd, "default")
            .load()
            .expect("load saved thread");

        assert_eq!(loaded, thread);
    }

    #[test]
    fn rejects_invalid_thread_names() {
        let root = unique_dir("invalid-root");
        let cwd = unique_dir("invalid-cwd");

        for name in ["", "../escape", "a/b", "with.dot", "space name", "中文"] {
            assert!(matches!(
                ThreadStore::new(&root, &cwd, name),
                Err(ThreadStoreError::InvalidThreadName { .. })
            ));
        }
    }

    #[test]
    fn malformed_json_reports_error_and_keeps_file() {
        let root = unique_dir("malformed-root");
        let cwd = unique_dir("malformed-cwd");
        let store = make_store(&root, &cwd, "default");
        let path = store.path().to_path_buf();
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(&path, "{not-json").expect("write malformed json");

        let err = store.load().expect_err("load must fail");

        assert!(matches!(err, ThreadStoreError::Parse { .. }));
        assert_eq!(fs::read_to_string(path).expect("read file"), "{not-json");
    }

    #[test]
    fn unsupported_schema_version_reports_error_and_keeps_file() {
        let root = unique_dir("schema-root");
        let cwd = unique_dir("schema-cwd");
        let store = make_store(&root, &cwd, "default");
        let path = store.path().to_path_buf();
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        let content = json!({
            "schema_version": 999,
            "thread": {"messages": []}
        })
        .to_string();
        fs::write(&path, &content).expect("write unsupported schema");

        let err = store.load().expect_err("load must fail");

        assert!(matches!(
            err,
            ThreadStoreError::UnsupportedSchemaVersion { version: 999, .. }
        ));
        assert_eq!(fs::read_to_string(path).expect("read file"), content);
    }

    #[test]
    fn reset_style_save_overwrites_existing_history() {
        let root = unique_dir("reset-root");
        let cwd = unique_dir("reset-cwd");
        let store = make_store(&root, &cwd, "default");
        store.save(&sample_thread()).expect("save existing thread");

        let mut reset_thread = Thread::new();
        reset_thread.push(Message::user("New"));
        reset_thread.push(Message::assistant("History"));
        store.save(&reset_thread).expect("save reset thread");

        assert_eq!(store.load().expect("load reset thread"), reset_thread);
    }
}
