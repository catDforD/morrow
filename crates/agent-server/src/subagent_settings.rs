use crate::secrets::replace_file;
use agent_protocol::{
    MAX_SUBAGENT_PROMPT_SUFFIX_CHARS, MAX_SUBAGENT_TIMEOUT_SECS, MAX_SUBAGENT_TOOL_ROUNDS,
    MIN_SUBAGENT_TIMEOUT_SECS, MIN_SUBAGENT_TOOL_ROUNDS, ModelSelection, PermissionMode,
    ShellPolicy, SubagentIdentity, SubagentRole, SubagentRoleOverride, default_subagent_identities,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::RwLock;

const LEGACY_SUBAGENT_STORE_SCHEMA_VERSION: u32 = 1;
const SUBAGENT_STORE_SCHEMA_VERSION: u32 = 2;
const MAX_STORE_BYTES: u64 = 32 * 1024 * 1024;
pub const MIN_SUBAGENT_PROFILES: usize = 4;
pub const MAX_SUBAGENT_PROFILES: usize = 64;
pub const MAX_SUBAGENT_NAME_CHARS: usize = 40;
pub const MAX_SUBAGENT_AVATAR_BYTES: usize = 256 * 1024;
static PROFILE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentProfileResponse {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub avatar_data_url: Option<String>,
}

impl SubagentProfileResponse {
    fn identity(&self) -> SubagentIdentity {
        SubagentIdentity {
            id: self.id.clone(),
            name: self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubagentSettingsResponse {
    pub profiles: Vec<SubagentProfileResponse>,
    pub roles: Vec<SubagentRoleSettingsResponse>,
    pub store_path: String,
    pub min_profiles: usize,
    pub max_profiles: usize,
    pub max_avatar_bytes: usize,
    pub accepted_avatar_types: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubagentRoleSettingsResponse {
    pub role: SubagentRole,
    pub display_name: &'static str,
    pub description: &'static str,
    pub tools: Vec<&'static str>,
    pub permission_mode: PermissionMode,
    pub shell_policy: ShellPolicy,
    #[serde(flatten)]
    pub overrides: SubagentRoleOverride,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SubagentRoleWriteRequest {
    #[serde(default)]
    pub model_selection: Option<ModelSelection>,
    #[serde(default)]
    pub prompt_suffix: String,
    pub timeout_secs: u64,
    pub max_tool_rounds: usize,
}

impl From<SubagentRoleWriteRequest> for SubagentRoleOverride {
    fn from(value: SubagentRoleWriteRequest) -> Self {
        Self {
            model_selection: value.model_selection,
            prompt_suffix: value.prompt_suffix,
            timeout_secs: value.timeout_secs,
            max_tool_rounds: value.max_tool_rounds,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubagentProfileWriteRequest {
    pub name: String,
    #[serde(default)]
    pub avatar_data_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSubagentStore {
    schema_version: u32,
    profiles: Vec<SubagentProfileResponse>,
    roles: BTreeMap<SubagentRole, SubagentRoleOverride>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyPersistedSubagentStore {
    schema_version: u32,
    profiles: Vec<SubagentProfileResponse>,
}

impl Default for PersistedSubagentStore {
    fn default() -> Self {
        Self {
            schema_version: SUBAGENT_STORE_SCHEMA_VERSION,
            profiles: default_profiles(),
            roles: default_role_overrides(),
        }
    }
}

#[derive(Debug, Error)]
pub enum SubagentRegistryError {
    #[error("failed to read subagent settings {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse subagent settings {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("subagent settings {path} uses schema {version}; expected {expected}")]
    UnsupportedSchema {
        path: PathBuf,
        version: u32,
        expected: u32,
    },
    #[error("invalid subagent settings {path}: {message}")]
    InvalidStore { path: PathBuf, message: String },
    #[error("invalid subagent settings: {0}")]
    Validation(String),
    #[error("failed to serialize subagent settings: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("failed to create subagent settings directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write subagent settings {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to replace subagent settings {path}: {source}")]
    Replace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("subagent profile {0:?} already exists")]
    Conflict(String),
    #[error("subagent profile {0:?} was not found")]
    NotFound(String),
}

pub struct SubagentRegistry {
    path: PathBuf,
    persistent: bool,
    store: RwLock<PersistedSubagentStore>,
}

impl SubagentRegistry {
    pub fn load(path: PathBuf) -> Result<Self, SubagentRegistryError> {
        let store = load_store(&path)?;
        Ok(Self {
            path,
            persistent: true,
            store: RwLock::new(store),
        })
    }

    pub fn in_memory(path: PathBuf) -> Self {
        Self {
            path,
            persistent: false,
            store: RwLock::new(PersistedSubagentStore::default()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn settings(&self) -> SubagentSettingsResponse {
        let store = self.store.read().await;
        settings_response(&self.path, store.profiles.clone(), &store.roles)
    }

    pub async fn identities(&self) -> Vec<SubagentIdentity> {
        self.store
            .read()
            .await
            .profiles
            .iter()
            .map(SubagentProfileResponse::identity)
            .collect()
    }

    pub async fn role_overrides(&self) -> BTreeMap<SubagentRole, SubagentRoleOverride> {
        self.store.read().await.roles.clone()
    }

    pub async fn update_role(
        &self,
        role: SubagentRole,
        request: SubagentRoleWriteRequest,
    ) -> Result<SubagentRoleSettingsResponse, SubagentRegistryError> {
        let overrides = SubagentRoleOverride::from(request);
        validate_role_override(role, &overrides)?;
        let mut guard = self.store.write().await;
        let mut next = guard.clone();
        next.roles.insert(role, overrides.clone());
        validate_store(&next)?;
        commit_store(&self.path, self.persistent, &next)?;
        *guard = next;
        Ok(role_settings_response(role, overrides))
    }

    pub async fn reset_roles(
        &self,
    ) -> Result<Vec<SubagentRoleSettingsResponse>, SubagentRegistryError> {
        let mut guard = self.store.write().await;
        let mut next = guard.clone();
        next.roles = default_role_overrides();
        validate_store(&next)?;
        commit_store(&self.path, self.persistent, &next)?;
        let roles = role_settings_responses(&next.roles);
        *guard = next;
        Ok(roles)
    }

    pub async fn create(
        &self,
        request: SubagentProfileWriteRequest,
    ) -> Result<SubagentProfileResponse, SubagentRegistryError> {
        let mut guard = self.store.write().await;
        let mut next = guard.clone();
        if next.profiles.len() >= MAX_SUBAGENT_PROFILES {
            return Err(SubagentRegistryError::Validation(format!(
                "at most {MAX_SUBAGENT_PROFILES} subagent profiles are allowed"
            )));
        }
        let mut profile = normalize_request(request)?;
        ensure_unique_name(&next.profiles, None, &profile.name)?;
        profile.id = next_profile_id(&next.profiles);
        next.profiles.push(profile.clone());
        validate_store(&next)?;
        commit_store(&self.path, self.persistent, &next)?;
        *guard = next;
        Ok(profile)
    }

    pub async fn update(
        &self,
        id: &str,
        request: SubagentProfileWriteRequest,
    ) -> Result<SubagentProfileResponse, SubagentRegistryError> {
        validate_profile_id(id)?;
        let mut guard = self.store.write().await;
        let mut next = guard.clone();
        let index = next
            .profiles
            .iter()
            .position(|profile| profile.id == id)
            .ok_or_else(|| SubagentRegistryError::NotFound(id.to_string()))?;
        let mut profile = normalize_request(request)?;
        ensure_unique_name(&next.profiles, Some(id), &profile.name)?;
        profile.id = id.to_string();
        next.profiles[index] = profile.clone();
        validate_store(&next)?;
        commit_store(&self.path, self.persistent, &next)?;
        *guard = next;
        Ok(profile)
    }

    pub async fn delete(&self, id: &str) -> Result<(), SubagentRegistryError> {
        validate_profile_id(id)?;
        let mut guard = self.store.write().await;
        let mut next = guard.clone();
        let original_len = next.profiles.len();
        next.profiles.retain(|profile| profile.id != id);
        if next.profiles.len() == original_len {
            return Err(SubagentRegistryError::NotFound(id.to_string()));
        }
        validate_store(&next)?;
        commit_store(&self.path, self.persistent, &next)?;
        *guard = next;
        Ok(())
    }

    pub async fn reset(&self) -> Result<SubagentSettingsResponse, SubagentRegistryError> {
        let mut guard = self.store.write().await;
        let mut next = guard.clone();
        next.profiles = default_profiles();
        validate_store(&next)?;
        commit_store(&self.path, self.persistent, &next)?;
        let response = settings_response(&self.path, next.profiles.clone(), &next.roles);
        *guard = next;
        Ok(response)
    }
}

pub fn load_subagent_identities(
    path: &Path,
) -> Result<Vec<SubagentIdentity>, SubagentRegistryError> {
    Ok(load_store(path)?
        .profiles
        .iter()
        .map(SubagentProfileResponse::identity)
        .collect())
}

fn settings_response(
    path: &Path,
    profiles: Vec<SubagentProfileResponse>,
    roles: &BTreeMap<SubagentRole, SubagentRoleOverride>,
) -> SubagentSettingsResponse {
    SubagentSettingsResponse {
        profiles,
        roles: role_settings_responses(roles),
        store_path: path.display().to_string(),
        min_profiles: MIN_SUBAGENT_PROFILES,
        max_profiles: MAX_SUBAGENT_PROFILES,
        max_avatar_bytes: MAX_SUBAGENT_AVATAR_BYTES,
        accepted_avatar_types: vec!["image/png", "image/jpeg", "image/webp"],
    }
}

fn role_settings_responses(
    roles: &BTreeMap<SubagentRole, SubagentRoleOverride>,
) -> Vec<SubagentRoleSettingsResponse> {
    SubagentRole::ALL
        .into_iter()
        .map(|role| role_settings_response(role, roles.get(&role).cloned().unwrap_or_default()))
        .collect()
}

fn role_settings_response(
    role: SubagentRole,
    overrides: SubagentRoleOverride,
) -> SubagentRoleSettingsResponse {
    let (display_name, description, tools, permission_mode, shell_policy) = match role {
        SubagentRole::Explore => (
            "Explore",
            "Read-only workspace investigation",
            vec!["read_file", "list_files", "search_text"],
            PermissionMode::ReadOnly,
            ShellPolicy::Deny,
        ),
        SubagentRole::Plan => (
            "Plan",
            "Read-only implementation planning",
            vec!["read_file", "list_files", "search_text"],
            PermissionMode::ReadOnly,
            ShellPolicy::Deny,
        ),
        SubagentRole::Worker => (
            "Worker",
            "Approval-controlled workspace implementation",
            vec![
                "read_file",
                "list_files",
                "search_text",
                "edit_file",
                "write_file",
                "apply_patch",
                "shell_command",
            ],
            PermissionMode::WorkspaceWrite,
            ShellPolicy::Prompt,
        ),
        SubagentRole::Reviewer => (
            "Reviewer",
            "Read-only review with approval-controlled shell commands",
            vec!["read_file", "list_files", "search_text", "shell_command"],
            PermissionMode::ReadOnly,
            ShellPolicy::Prompt,
        ),
    };
    SubagentRoleSettingsResponse {
        role,
        display_name,
        description,
        tools,
        permission_mode,
        shell_policy,
        overrides,
    }
}

fn default_role_overrides() -> BTreeMap<SubagentRole, SubagentRoleOverride> {
    SubagentRole::ALL
        .into_iter()
        .map(|role| (role, SubagentRoleOverride::default()))
        .collect()
}

fn default_profiles() -> Vec<SubagentProfileResponse> {
    default_subagent_identities()
        .into_iter()
        .map(|identity| SubagentProfileResponse {
            id: identity.id,
            name: identity.name,
            avatar_data_url: None,
        })
        .collect()
}

fn normalize_request(
    request: SubagentProfileWriteRequest,
) -> Result<SubagentProfileResponse, SubagentRegistryError> {
    let name = request.name.trim().to_string();
    validate_profile_name(&name)?;
    if let Some(avatar) = request.avatar_data_url.as_deref() {
        validate_avatar_data_url(avatar)?;
    }
    Ok(SubagentProfileResponse {
        id: String::new(),
        name,
        avatar_data_url: request.avatar_data_url,
    })
}

fn ensure_unique_name(
    profiles: &[SubagentProfileResponse],
    current_id: Option<&str>,
    name: &str,
) -> Result<(), SubagentRegistryError> {
    let normalized = name.to_lowercase();
    if profiles.iter().any(|profile| {
        Some(profile.id.as_str()) != current_id && profile.name.to_lowercase() == normalized
    }) {
        return Err(SubagentRegistryError::Conflict(name.to_string()));
    }
    Ok(())
}

fn validate_store(store: &PersistedSubagentStore) -> Result<(), SubagentRegistryError> {
    if !(MIN_SUBAGENT_PROFILES..=MAX_SUBAGENT_PROFILES).contains(&store.profiles.len()) {
        return Err(SubagentRegistryError::Validation(format!(
            "subagent profile count must be between {MIN_SUBAGENT_PROFILES} and {MAX_SUBAGENT_PROFILES}"
        )));
    }
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    for profile in &store.profiles {
        validate_profile_id(&profile.id)?;
        validate_profile_name(&profile.name)?;
        if !ids.insert(profile.id.clone()) {
            return Err(SubagentRegistryError::Validation(format!(
                "duplicate subagent profile id {:?}",
                profile.id
            )));
        }
        if !names.insert(profile.name.to_lowercase()) {
            return Err(SubagentRegistryError::Validation(format!(
                "duplicate subagent profile name {:?}",
                profile.name
            )));
        }
        if let Some(avatar) = profile.avatar_data_url.as_deref() {
            validate_avatar_data_url(avatar)?;
        }
    }
    if store.roles.len() != SubagentRole::ALL.len()
        || SubagentRole::ALL
            .into_iter()
            .any(|role| !store.roles.contains_key(&role))
    {
        return Err(SubagentRegistryError::Validation(
            "subagent settings must contain all four built-in roles".to_string(),
        ));
    }
    for (role, overrides) in &store.roles {
        validate_role_override(*role, overrides)?;
    }
    Ok(())
}

fn validate_role_override(
    role: SubagentRole,
    overrides: &SubagentRoleOverride,
) -> Result<(), SubagentRegistryError> {
    let suffix_chars = overrides.prompt_suffix.chars().count();
    if suffix_chars > MAX_SUBAGENT_PROMPT_SUFFIX_CHARS {
        return Err(SubagentRegistryError::Validation(format!(
            "{} prompt suffix must not exceed {MAX_SUBAGENT_PROMPT_SUFFIX_CHARS} characters",
            role.as_str()
        )));
    }
    if !(MIN_SUBAGENT_TIMEOUT_SECS..=MAX_SUBAGENT_TIMEOUT_SECS).contains(&overrides.timeout_secs) {
        return Err(SubagentRegistryError::Validation(format!(
            "{} timeout must be between {MIN_SUBAGENT_TIMEOUT_SECS} and {MAX_SUBAGENT_TIMEOUT_SECS} seconds",
            role.as_str()
        )));
    }
    if !(MIN_SUBAGENT_TOOL_ROUNDS..=MAX_SUBAGENT_TOOL_ROUNDS).contains(&overrides.max_tool_rounds) {
        return Err(SubagentRegistryError::Validation(format!(
            "{} max tool rounds must be between {MIN_SUBAGENT_TOOL_ROUNDS} and {MAX_SUBAGENT_TOOL_ROUNDS}",
            role.as_str()
        )));
    }
    Ok(())
}

fn validate_profile_id(id: &str) -> Result<(), SubagentRegistryError> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(SubagentRegistryError::Validation(format!(
            "invalid subagent profile id {id:?}"
        )));
    }
    Ok(())
}

fn validate_profile_name(name: &str) -> Result<(), SubagentRegistryError> {
    let chars = name.chars().count();
    if chars == 0 || chars > MAX_SUBAGENT_NAME_CHARS || name.trim() != name {
        return Err(SubagentRegistryError::Validation(format!(
            "subagent name must contain 1 to {MAX_SUBAGENT_NAME_CHARS} characters without surrounding whitespace"
        )));
    }
    Ok(())
}

fn validate_avatar_data_url(value: &str) -> Result<(), SubagentRegistryError> {
    let (mime, encoded) = value
        .strip_prefix("data:")
        .and_then(|value| value.split_once(";base64,"))
        .ok_or_else(|| {
            SubagentRegistryError::Validation("avatar must be a base64 data URL".to_string())
        })?;
    if !matches!(mime, "image/png" | "image/jpeg" | "image/webp") {
        return Err(SubagentRegistryError::Validation(format!(
            "avatar MIME type {mime:?} is not supported"
        )));
    }
    let max_encoded_len = MAX_SUBAGENT_AVATAR_BYTES.div_ceil(3) * 4;
    if encoded.len() > max_encoded_len {
        return Err(SubagentRegistryError::Validation(format!(
            "avatar must not exceed {MAX_SUBAGENT_AVATAR_BYTES} decoded bytes"
        )));
    }
    let bytes = STANDARD.decode(encoded).map_err(|error| {
        SubagentRegistryError::Validation(format!("avatar is not valid base64: {error}"))
    })?;
    if bytes.is_empty() || bytes.len() > MAX_SUBAGENT_AVATAR_BYTES {
        return Err(SubagentRegistryError::Validation(format!(
            "avatar must contain between 1 and {MAX_SUBAGENT_AVATAR_BYTES} decoded bytes"
        )));
    }
    let signature_matches = match mime {
        "image/png" => bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    };
    if !signature_matches {
        return Err(SubagentRegistryError::Validation(format!(
            "avatar bytes do not match MIME type {mime:?}"
        )));
    }
    Ok(())
}

fn next_profile_id(profiles: &[SubagentProfileResponse]) -> String {
    loop {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let counter = PROFILE_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let id = format!("user-{timestamp:016x}-{counter:04x}");
        if profiles.iter().all(|profile| profile.id != id) {
            return id;
        }
    }
}

fn load_store(path: &Path) -> Result<PersistedSubagentStore, SubagentRegistryError> {
    if !path.exists() {
        return Ok(PersistedSubagentStore::default());
    }
    let metadata = fs::metadata(path).map_err(|source| SubagentRegistryError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_STORE_BYTES {
        return Err(SubagentRegistryError::Validation(format!(
            "subagent settings {} exceed {MAX_STORE_BYTES} bytes",
            path.display()
        )));
    }
    let content = fs::read_to_string(path).map_err(|source| SubagentRegistryError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let value = serde_json::from_str::<serde_json::Value>(&content).map_err(|source| {
        SubagentRegistryError::Parse {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default() as u32;
    let (store, migrated) = match version {
        SUBAGENT_STORE_SCHEMA_VERSION => (
            serde_json::from_value::<PersistedSubagentStore>(value).map_err(|source| {
                SubagentRegistryError::Parse {
                    path: path.to_path_buf(),
                    source,
                }
            })?,
            false,
        ),
        LEGACY_SUBAGENT_STORE_SCHEMA_VERSION => {
            let legacy = serde_json::from_value::<LegacyPersistedSubagentStore>(value).map_err(
                |source| SubagentRegistryError::Parse {
                    path: path.to_path_buf(),
                    source,
                },
            )?;
            (
                PersistedSubagentStore {
                    schema_version: SUBAGENT_STORE_SCHEMA_VERSION,
                    profiles: legacy.profiles,
                    roles: default_role_overrides(),
                },
                true,
            )
        }
        _ => {
            return Err(SubagentRegistryError::UnsupportedSchema {
                path: path.to_path_buf(),
                version,
                expected: SUBAGENT_STORE_SCHEMA_VERSION,
            });
        }
    };
    validate_store(&store).map_err(|error| SubagentRegistryError::InvalidStore {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    if migrated {
        save_store(path, &store)?;
    }
    Ok(store)
}

fn commit_store(
    path: &Path,
    persistent: bool,
    store: &PersistedSubagentStore,
) -> Result<(), SubagentRegistryError> {
    if persistent {
        save_store(path, store)
    } else {
        validate_store(store)
    }
}

fn save_store(path: &Path, store: &PersistedSubagentStore) -> Result<(), SubagentRegistryError> {
    validate_store(store)?;
    let parent = path.parent().ok_or_else(|| {
        SubagentRegistryError::Validation("subagent settings path has no parent".to_string())
    })?;
    fs::create_dir_all(parent).map_err(|source| SubagentRegistryError::CreateDir {
        path: parent.to_path_buf(),
        source,
    })?;
    let content = serde_json::to_vec_pretty(store).map_err(SubagentRegistryError::Serialize)?;
    let temp = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("subagents.json"),
        std::process::id()
    ));
    fs::write(&temp, content).map_err(|source| SubagentRegistryError::Write {
        path: temp.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).map_err(|source| {
            SubagentRegistryError::Write {
                path: temp.clone(),
                source,
            }
        })?;
    }
    if let Err(source) = replace_file(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(SubagentRegistryError::Replace {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "morrow-{label}-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    fn tiny_png() -> String {
        let bytes = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0];
        format!("data:image/png;base64,{}", STANDARD.encode(bytes))
    }

    #[tokio::test]
    async fn missing_store_uses_defaults_and_crud_persists() {
        let path = unique_path("subagents-crud");
        let registry = SubagentRegistry::load(path.clone()).expect("load defaults");
        assert!(!path.exists());
        let defaults = registry.identities().await;
        assert_eq!(defaults.len(), 22);
        assert_eq!(defaults[0].id, "builtin-01");
        assert_eq!(defaults[0].name, "后藤一里");
        assert_eq!(defaults[21].id, "builtin-22");
        assert_eq!(defaults[21].name, "三角初华");

        let created = registry
            .create(SubagentProfileWriteRequest {
                name: "测试成员".to_string(),
                avatar_data_url: Some(tiny_png()),
            })
            .await
            .expect("create profile");
        assert!(path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path)
                    .expect("settings metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(created.id.starts_with("user-"));

        let updated = registry
            .update(
                &created.id,
                SubagentProfileWriteRequest {
                    name: "更新成员".to_string(),
                    avatar_data_url: None,
                },
            )
            .await
            .expect("update profile");
        assert_eq!(updated.name, "更新成员");
        assert!(updated.avatar_data_url.is_none());

        registry.delete(&created.id).await.expect("delete profile");
        let restored = SubagentRegistry::load(path.clone()).expect("reload store");
        assert_eq!(restored.identities().await.len(), 22);
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn schema_v1_store_migrates_to_v2_without_changing_identities() {
        let path = unique_path("subagents-v1-migration");
        let mut profiles = default_profiles();
        profiles[0].name = "自定义波奇".to_string();
        profiles[0].avatar_data_url = Some(tiny_png());
        profiles[1].id = "custom-stable-id".to_string();
        let legacy = LegacyPersistedSubagentStore {
            schema_version: LEGACY_SUBAGENT_STORE_SCHEMA_VERSION,
            profiles: profiles.clone(),
        };
        fs::write(
            &path,
            serde_json::to_vec_pretty(&legacy).expect("serialize legacy store"),
        )
        .expect("write legacy store");

        let registry = SubagentRegistry::load(path.clone()).expect("migrate legacy store");
        let settings = registry.settings().await;

        assert_eq!(settings.profiles, profiles);
        assert_eq!(settings.roles.len(), SubagentRole::ALL.len());
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read migrated store"))
                .expect("parse migrated store");
        assert_eq!(persisted["schema_version"], SUBAGENT_STORE_SCHEMA_VERSION);
        assert_eq!(persisted["profiles"][0]["name"], "自定义波奇");
        assert_eq!(persisted["profiles"][1]["id"], "custom-stable-id");
        assert_eq!(
            persisted["roles"].as_object().map(serde_json::Map::len),
            Some(SubagentRole::ALL.len())
        );
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn role_overrides_validate_and_persist_model_prompt_and_limits() {
        let path = unique_path("subagent-role-overrides");
        let registry = SubagentRegistry::load(path.clone()).expect("load defaults");
        let model_selection = ModelSelection {
            provider_id: "provider-1".to_string(),
            model_id: "model-1".to_string(),
            reasoning: agent_protocol::ReasoningLevel::High,
        };

        let updated = registry
            .update_role(
                SubagentRole::Worker,
                SubagentRoleWriteRequest {
                    model_selection: Some(model_selection.clone()),
                    prompt_suffix: "Only touch the requested files.".to_string(),
                    timeout_secs: 600,
                    max_tool_rounds: 12,
                },
            )
            .await
            .expect("update worker role");
        assert_eq!(
            updated.overrides.model_selection,
            Some(model_selection.clone())
        );
        assert_eq!(updated.overrides.timeout_secs, 600);
        assert_eq!(updated.overrides.max_tool_rounds, 12);

        let reloaded = SubagentRegistry::load(path.clone()).expect("reload role settings");
        assert_eq!(
            reloaded
                .role_overrides()
                .await
                .get(&SubagentRole::Worker)
                .and_then(|overrides| overrides.model_selection.clone()),
            Some(model_selection)
        );

        let invalid = registry
            .update_role(
                SubagentRole::Explore,
                SubagentRoleWriteRequest {
                    model_selection: None,
                    prompt_suffix: String::new(),
                    timeout_secs: MIN_SUBAGENT_TIMEOUT_SECS - 1,
                    max_tool_rounds: 1,
                },
            )
            .await
            .expect_err("short timeout must fail");
        assert!(matches!(invalid, SubagentRegistryError::Validation(_)));
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn registry_enforces_unique_names_and_minimum_profiles() {
        let path = unique_path("subagents-validation");
        let registry = SubagentRegistry::load(path.clone()).expect("load defaults");
        let duplicate = registry
            .create(SubagentProfileWriteRequest {
                name: "后藤一里".to_string(),
                avatar_data_url: None,
            })
            .await
            .expect_err("duplicate must fail");
        assert!(matches!(duplicate, SubagentRegistryError::Conflict(_)));

        let ascii = registry
            .create(SubagentProfileWriteRequest {
                name: "Test Agent".to_string(),
                avatar_data_url: None,
            })
            .await
            .expect("create mixed-case profile");
        let case_insensitive_duplicate = registry
            .create(SubagentProfileWriteRequest {
                name: "test agent".to_string(),
                avatar_data_url: None,
            })
            .await
            .expect_err("case-insensitive duplicate must fail");
        assert!(matches!(
            case_insensitive_duplicate,
            SubagentRegistryError::Conflict(_)
        ));
        registry
            .delete(&ascii.id)
            .await
            .expect("remove mixed-case profile");

        let ids = registry.identities().await;
        for identity in ids.into_iter().take(18) {
            registry.delete(&identity.id).await.expect("delete profile");
        }
        let last = registry.identities().await[0].id.clone();
        let error = registry.delete(&last).await.expect_err("minimum must hold");
        assert!(matches!(error, SubagentRegistryError::Validation(_)));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn avatar_validation_rejects_unsupported_and_mismatched_content() {
        assert!(validate_avatar_data_url(&tiny_png()).is_ok());
        assert!(validate_avatar_data_url("data:image/svg+xml;base64,PHN2Zz4=").is_err());
        assert!(validate_avatar_data_url("data:image/gif;base64,R0lGODlh").is_err());
        assert!(validate_avatar_data_url("data:image/png;base64,ZmFrZQ==").is_err());
        assert!(validate_avatar_data_url("data:image/png;base64,***").is_err());
        assert!(validate_avatar_data_url("not-a-data-url").is_err());

        let oversized = vec![0_u8; MAX_SUBAGENT_AVATAR_BYTES + 1];
        let oversized = format!("data:image/png;base64,{}", STANDARD.encode(oversized));
        assert!(validate_avatar_data_url(&oversized).is_err());
    }

    #[tokio::test]
    async fn registry_rejects_profile_limits_and_invalid_names() {
        let path = unique_path("subagents-limits");
        let registry = SubagentRegistry::load(path.clone()).expect("load defaults");
        for index in 0..(MAX_SUBAGENT_PROFILES - 22) {
            registry
                .create(SubagentProfileWriteRequest {
                    name: format!("成员-{index}"),
                    avatar_data_url: None,
                })
                .await
                .expect("fill profile list");
        }
        let overflow = registry
            .create(SubagentProfileWriteRequest {
                name: "超额成员".to_string(),
                avatar_data_url: None,
            })
            .await
            .expect_err("maximum must hold");
        assert!(matches!(overflow, SubagentRegistryError::Validation(_)));

        let invalid = registry
            .update(
                "builtin-01",
                SubagentProfileWriteRequest {
                    name: "名".repeat(MAX_SUBAGENT_NAME_CHARS + 1),
                    avatar_data_url: None,
                },
            )
            .await
            .expect_err("long name must fail");
        assert!(matches!(invalid, SubagentRegistryError::Validation(_)));
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn in_memory_registry_never_writes_its_display_path() {
        let path = unique_path("subagents-memory");
        let registry = SubagentRegistry::in_memory(path.clone());
        registry
            .create(SubagentProfileWriteRequest {
                name: "内存成员".to_string(),
                avatar_data_url: None,
            })
            .await
            .expect("create in memory");
        assert!(!path.exists());
    }

    #[test]
    fn invalid_stores_report_the_source_path_without_overwriting() {
        let path = unique_path("subagents-invalid-store");
        fs::write(&path, r#"{"schema_version":3,"profiles":[]}"#).expect("write invalid store");
        let error = match SubagentRegistry::load(path.clone()) {
            Ok(_) => panic!("schema must fail"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            SubagentRegistryError::UnsupportedSchema { .. }
        ));
        assert!(error.to_string().contains(&path.display().to_string()));
        assert_eq!(
            fs::read_to_string(&path).expect("store remains"),
            r#"{"schema_version":3,"profiles":[]}"#
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn persisted_store_rejects_invalid_profile_ids_and_names_without_overwriting() {
        let mut invalid_id = PersistedSubagentStore::default();
        invalid_id.profiles[0].id = "invalid id".to_string();
        let mut invalid_name = PersistedSubagentStore::default();
        invalid_name.profiles[0].name = " 后藤一里".to_string();

        for (label, store) in [("invalid-id", invalid_id), ("invalid-name", invalid_name)] {
            let path = unique_path(label);
            let content = serde_json::to_string(&store).expect("serialize invalid store");
            fs::write(&path, &content).expect("write invalid store");

            let error = match SubagentRegistry::load(path.clone()) {
                Ok(_) => panic!("invalid persisted profile must fail"),
                Err(error) => error,
            };
            assert!(matches!(error, SubagentRegistryError::InvalidStore { .. }));
            assert!(error.to_string().contains(&path.display().to_string()));
            assert_eq!(fs::read_to_string(&path).expect("store remains"), content);
            let _ = fs::remove_file(path);
        }
    }

    #[tokio::test]
    async fn reset_restores_stable_default_identities() {
        let path = unique_path("subagents-reset");
        let registry = SubagentRegistry::load(path.clone()).expect("load defaults");
        registry
            .update(
                "builtin-01",
                SubagentProfileWriteRequest {
                    name: "波奇".to_string(),
                    avatar_data_url: Some(tiny_png()),
                },
            )
            .await
            .expect("update default");
        registry
            .update_role(
                SubagentRole::Worker,
                SubagentRoleWriteRequest {
                    model_selection: None,
                    prompt_suffix: "keep this role override".to_string(),
                    timeout_secs: 600,
                    max_tool_rounds: 12,
                },
            )
            .await
            .expect("update role before identity reset");
        let reset = registry.reset().await.expect("reset profiles");
        assert_eq!(reset.profiles[0].id, "builtin-01");
        assert_eq!(reset.profiles[0].name, "后藤一里");
        assert!(reset.profiles[0].avatar_data_url.is_none());
        let worker = reset
            .roles
            .iter()
            .find(|role| role.role == SubagentRole::Worker)
            .expect("worker role");
        assert_eq!(worker.overrides.prompt_suffix, "keep this role override");
        assert_eq!(worker.overrides.timeout_secs, 600);
        assert_eq!(worker.overrides.max_tool_rounds, 12);
        let _ = fs::remove_file(path);
    }
}
