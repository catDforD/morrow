use crate::types::{
    HOOK_CONFIG_SCHEMA_VERSION, HOOK_TRUST_SCHEMA_VERSION, HookDefinition, HookError,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TRUST_WRITE_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HookTrustStore {
    schema_version: u32,
    #[serde(default)]
    projects: BTreeMap<String, String>,
}

impl Default for HookTrustStore {
    fn default() -> Self {
        Self {
            schema_version: HOOK_TRUST_SCHEMA_VERSION,
            projects: BTreeMap::new(),
        }
    }
}

impl HookTrustStore {
    pub fn is_trusted(&self, workspace_key: &str, fingerprint: &str) -> bool {
        self.projects
            .get(workspace_key)
            .is_some_and(|stored| stored == fingerprint)
    }

    pub fn trust(&mut self, workspace_key: String, fingerprint: String) {
        self.projects.insert(workspace_key, fingerprint);
    }

    pub fn revoke(&mut self, workspace_key: &str) {
        self.projects.remove(workspace_key);
    }
}

pub(crate) fn hook_fingerprint(hooks: &[HookDefinition]) -> Result<String, HookError> {
    #[derive(Serialize)]
    struct FingerprintDocument<'a> {
        schema_version: u32,
        hooks: &'a [HookDefinition],
    }
    let bytes = serde_json::to_vec(&FingerprintDocument {
        schema_version: HOOK_CONFIG_SCHEMA_VERSION,
        hooks,
    })
    .map_err(HookError::TrustSerialize)?;
    let digest = Sha256::digest(bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(crate) fn workspace_key(workspace_root: &Path) -> Result<String, HookError> {
    let path = fs::canonicalize(workspace_root).map_err(|source| HookError::Io {
        path: workspace_root.to_path_buf(),
        source,
    })?;
    Ok(path.to_string_lossy().into_owned())
}

pub(crate) fn load_trust_store(path: &Path) -> Result<HookTrustStore, HookError> {
    if !path.is_file() {
        return Ok(HookTrustStore::default());
    }
    let bytes = fs::read(path).map_err(|source| HookError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let trust = serde_json::from_slice::<HookTrustStore>(&bytes).map_err(|source| {
        HookError::TrustParse {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if trust.schema_version != HOOK_TRUST_SCHEMA_VERSION {
        return Err(HookError::UnsupportedTrustSchema {
            path: path.to_path_buf(),
            version: trust.schema_version,
        });
    }
    Ok(trust)
}

pub(crate) fn save_trust_store(path: &Path, trust: &HookTrustStore) -> Result<(), HookError> {
    let parent = path.parent().expect("trust store has parent");
    fs::create_dir_all(parent).map_err(|source| HookError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let temporary = trust_temporary_path(path);
    let mut bytes = serde_json::to_vec_pretty(trust).map_err(HookError::TrustSerialize)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|source| HookError::Io {
            path: temporary.clone(),
            source,
        })?;
    file.write_all(&bytes)
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
        .map_err(|source| HookError::Io {
            path: temporary.clone(),
            source,
        })?;
    if let Err(source) = replace_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(HookError::Io {
            path: path.to_path_buf(),
            source,
        });
    }
    sync_parent_directory(parent).map_err(|source| HookError::Io {
        path: parent.to_path_buf(),
        source,
    })
}

fn trust_temporary_path(path: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = TRUST_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        ".hook-trust.json.tmp-{}-{stamp}-{sequence}",
        std::process::id()
    ))
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
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> std::io::Result<()> {
    Ok(())
}
