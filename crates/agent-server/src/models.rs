use agent_config::ModelContextLimits;
use agent_model::{ModelError, OpenAiCompatClient, OpenAiCompatConfig, OpenAiCompatRequestOptions};
use agent_protocol::{ModelInvocation, ModelSelection, ReasoningLevel, ReasoningProfile};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::RwLock;

const MODEL_STORE_SCHEMA_VERSION: u32 = 1;
const FALLBACK_PROVIDER_ID: &str = "current-config";
const DEFAULT_RESERVED_OUTPUT_TOKENS: usize = 8_192;
const DEFAULT_TIMEOUT_SECS: u64 = 120;

#[derive(Clone)]
pub struct FallbackModel {
    pub provider_name: String,
    pub model_id: String,
    pub model_name: String,
    pub client: OpenAiCompatClient,
    pub limits: ModelContextLimits,
    pub reasoning_profile: ReasoningProfile,
}

impl std::fmt::Debug for FallbackModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FallbackModel")
            .field("provider_name", &self.provider_name)
            .field("model_id", &self.model_id)
            .field("model_name", &self.model_name)
            .field("limits", &self.limits)
            .field("reasoning_profile", &self.reasoning_profile)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedModel {
    pub id: String,
    pub name: String,
    pub context_window_tokens: usize,
    pub reserved_output_tokens: usize,
    #[serde(default = "default_true")]
    pub supports_tools: bool,
    #[serde(default)]
    pub reasoning_profile: ReasoningProfile,
}

#[derive(Clone, Serialize, Deserialize)]
struct ManagedProvider {
    id: String,
    name: String,
    base_url: String,
    api_key: String,
    enabled: bool,
    timeout_secs: u64,
    models: Vec<ManagedModel>,
}

impl std::fmt::Debug for ManagedProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedProvider")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("base_url", &"<configured>")
            .field("api_key", &"<redacted>")
            .field("enabled", &self.enabled)
            .field("timeout_secs", &self.timeout_secs)
            .field("models", &self.models)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedModelStore {
    schema_version: u32,
    #[serde(default)]
    providers: Vec<ManagedProvider>,
    #[serde(default)]
    default_selection: Option<ModelSelection>,
    #[serde(default)]
    session_selections: BTreeMap<String, ModelSelection>,
}

impl Default for PersistedModelStore {
    fn default() -> Self {
        Self {
            schema_version: MODEL_STORE_SCHEMA_VERSION,
            providers: Vec::new(),
            default_selection: None,
            session_selections: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelSettingsResponse {
    pub providers: Vec<ModelProviderResponse>,
    pub default_selection: Option<ModelSelection>,
    pub model_ready: bool,
    pub store_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelProviderResponse {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_format: &'static str,
    pub enabled: bool,
    pub read_only: bool,
    pub api_key_configured: bool,
    pub timeout_secs: u64,
    pub models: Vec<ManagedModel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderWriteRequest {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    pub models: Vec<ManagedModel>,
    #[serde(default)]
    pub default_model: Option<DefaultModelRequest>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DefaultModelRequest {
    pub model_id: String,
    pub reasoning: ReasoningLevel,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiscoverModelsRequest {
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoverModelsResponse {
    pub models: Vec<DiscoveredModel>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredModel {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested: Option<ManagedModel>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionModelSelectionResponse {
    pub selection: Option<ModelSelection>,
    pub inherited: bool,
}

#[derive(Clone)]
pub struct ResolvedModel {
    pub selection: ModelSelection,
    pub invocation: ModelInvocation,
    pub client: OpenAiCompatClient,
    pub limits: ModelContextLimits,
}

impl std::fmt::Debug for ResolvedModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedModel")
            .field("selection", &self.selection)
            .field("invocation", &self.invocation)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
pub enum ModelRegistryError {
    #[error("failed to read model settings {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse model settings {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("unsupported model settings schema version {version}; expected {expected}")]
    UnsupportedSchema { version: u32, expected: u32 },
    #[error("failed to create model settings directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize model settings: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("failed to write model settings {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to replace model settings {path}: {source}")]
    Replace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid model settings: {0}")]
    Validation(String),
    #[error("model settings conflict: {0}")]
    Conflict(String),
    #[error("model provider {0:?} was not found")]
    ProviderNotFound(String),
    #[error("model selection is unavailable: {0}")]
    SelectionUnavailable(String),
    #[error(transparent)]
    Model(#[from] ModelError),
}

struct RegistryState {
    store: PersistedModelStore,
    clients: HashMap<(String, String), OpenAiCompatClient>,
}

pub struct ModelRegistry {
    path: PathBuf,
    workspace_scope: String,
    fallback: Option<FallbackModel>,
    state: RwLock<RegistryState>,
}

impl ModelRegistry {
    pub fn load(
        path: PathBuf,
        workspace_root: &Path,
        fallback: Option<FallbackModel>,
    ) -> Result<Self, ModelRegistryError> {
        let store = load_store(&path)?;
        validate_store(&store, fallback.as_ref())?;
        let clients = build_clients(&store)?;
        Ok(Self {
            path,
            workspace_scope: hex_encode(workspace_root.as_os_str().as_encoded_bytes()),
            fallback,
            state: RwLock::new(RegistryState { store, clients }),
        })
    }

    pub async fn settings(&self) -> ModelSettingsResponse {
        let state = self.state.read().await;
        let mut providers = Vec::new();
        if let Some(fallback) = self.fallback.as_ref() {
            providers.push(fallback_response(fallback));
        }
        providers.extend(state.store.providers.iter().map(provider_response));
        let default_selection = effective_default(&state.store, self.fallback.as_ref());
        let model_ready = default_selection
            .as_ref()
            .and_then(|selection| {
                resolve_from_state(&state, self.fallback.as_ref(), selection).ok()
            })
            .is_some();
        ModelSettingsResponse {
            providers,
            default_selection,
            model_ready,
            store_path: self.path.display().to_string(),
        }
    }

    pub async fn create_provider(
        &self,
        request: ProviderWriteRequest,
    ) -> Result<ModelProviderResponse, ModelRegistryError> {
        let mut state = self.state.write().await;
        validate_provider_request(&request, None, &state.store.providers)?;
        let id = unique_provider_id(&request.name, &state.store.providers);
        let provider = provider_from_request(id.clone(), request.clone(), None)?;
        let had_default = effective_default(&state.store, self.fallback.as_ref()).is_some();
        if !had_default && request.default_model.is_none() {
            return Err(ModelRegistryError::Validation(
                "the first provider must select a default model and reasoning level".to_string(),
            ));
        }
        let mut next_store = state.store.clone();
        if let Some(default_model) = request.default_model {
            let selection = ModelSelection {
                provider_id: id.clone(),
                model_id: default_model.model_id.trim().to_string(),
                reasoning: default_model.reasoning,
            };
            validate_selection_for_provider(&provider, &selection)?;
            next_store.default_selection = Some(selection);
        }
        next_store.providers.push(provider.clone());
        commit_store(&self.path, &mut state, next_store, self.fallback.as_ref())?;
        Ok(provider_response(&provider))
    }

    pub async fn update_provider(
        &self,
        provider_id: &str,
        request: ProviderWriteRequest,
    ) -> Result<ModelProviderResponse, ModelRegistryError> {
        let mut state = self.state.write().await;
        let index = state
            .store
            .providers
            .iter()
            .position(|provider| provider.id == provider_id)
            .ok_or_else(|| ModelRegistryError::ProviderNotFound(provider_id.to_string()))?;
        validate_provider_request(&request, Some(provider_id), &state.store.providers)?;
        let current = state.store.providers[index].clone();
        let provider =
            provider_from_request(provider_id.to_string(), request.clone(), Some(&current))?;
        let next_default = request.default_model.map(|default_model| ModelSelection {
            provider_id: provider_id.to_string(),
            model_id: default_model.model_id.trim().to_string(),
            reasoning: default_model.reasoning,
        });
        if let Some(selection) = next_default.as_ref() {
            validate_selection_for_provider(&provider, selection)?;
        }
        if let Some(default) = state.store.default_selection.as_ref()
            && default.provider_id == provider_id
            && next_default.is_none()
        {
            validate_selection_for_provider(&provider, default)?;
        }
        let mut next_store = state.store.clone();
        next_store.providers[index] = provider.clone();
        if next_default.is_some() {
            next_store.default_selection = next_default;
        }
        remove_invalid_session_selections(&mut next_store, self.fallback.as_ref());
        commit_store(&self.path, &mut state, next_store, self.fallback.as_ref())?;
        Ok(provider_response(&provider))
    }

    pub async fn delete_provider(&self, provider_id: &str) -> Result<(), ModelRegistryError> {
        let mut state = self.state.write().await;
        if provider_id == FALLBACK_PROVIDER_ID {
            return Err(ModelRegistryError::Validation(
                "the current config provider is read-only".to_string(),
            ));
        }
        if state
            .store
            .default_selection
            .as_ref()
            .is_some_and(|selection| selection.provider_id == provider_id)
        {
            return Err(ModelRegistryError::Conflict(
                "choose another global default before deleting this provider".to_string(),
            ));
        }
        let mut next_store = state.store.clone();
        let original_len = next_store.providers.len();
        next_store
            .providers
            .retain(|provider| provider.id != provider_id);
        if next_store.providers.len() == original_len {
            return Err(ModelRegistryError::ProviderNotFound(
                provider_id.to_string(),
            ));
        }
        next_store
            .session_selections
            .retain(|_, selection| selection.provider_id != provider_id);
        commit_store(&self.path, &mut state, next_store, self.fallback.as_ref())
    }

    pub async fn set_default(
        &self,
        selection: ModelSelection,
    ) -> Result<ModelSelection, ModelRegistryError> {
        let mut state = self.state.write().await;
        resolve_from_state(&state, self.fallback.as_ref(), &selection)?;
        let mut next_store = state.store.clone();
        next_store.default_selection = Some(selection.clone());
        commit_store(&self.path, &mut state, next_store, self.fallback.as_ref())?;
        Ok(selection)
    }

    pub async fn session_selection(&self, session: &str) -> SessionModelSelectionResponse {
        let state = self.state.read().await;
        let key = self.session_key(session);
        if let Some(selection) = state.store.session_selections.get(&key)
            && resolve_from_state(&state, self.fallback.as_ref(), selection).is_ok()
        {
            return SessionModelSelectionResponse {
                selection: Some(selection.clone()),
                inherited: false,
            };
        }
        SessionModelSelectionResponse {
            selection: effective_default(&state.store, self.fallback.as_ref()),
            inherited: true,
        }
    }

    pub async fn set_session_selection(
        &self,
        session: &str,
        selection: ModelSelection,
    ) -> Result<SessionModelSelectionResponse, ModelRegistryError> {
        let mut state = self.state.write().await;
        resolve_from_state(&state, self.fallback.as_ref(), &selection)?;
        let mut next_store = state.store.clone();
        next_store
            .session_selections
            .insert(self.session_key(session), selection.clone());
        commit_store(&self.path, &mut state, next_store, self.fallback.as_ref())?;
        Ok(SessionModelSelectionResponse {
            selection: Some(selection),
            inherited: false,
        })
    }

    pub async fn resolve_for_turn(
        &self,
        session: &str,
        requested: Option<ModelSelection>,
    ) -> Result<ResolvedModel, ModelRegistryError> {
        let selection = match requested {
            Some(selection) => selection,
            None => self
                .session_selection(session)
                .await
                .selection
                .ok_or_else(|| {
                    ModelRegistryError::SelectionUnavailable(
                        "no default model is configured".to_string(),
                    )
                })?,
        };
        let state = self.state.read().await;
        resolve_from_state(&state, self.fallback.as_ref(), &selection)
    }

    pub async fn discover(
        &self,
        request: DiscoverModelsRequest,
    ) -> Result<DiscoverModelsResponse, ModelRegistryError> {
        let (base_url, api_key, timeout_secs) = if let Some(provider_id) = request.provider_id {
            let state = self.state.read().await;
            let provider = state
                .store
                .providers
                .iter()
                .find(|provider| provider.id == provider_id)
                .ok_or(ModelRegistryError::ProviderNotFound(provider_id))?;
            (
                provider.base_url.clone(),
                provider.api_key.clone(),
                provider.timeout_secs,
            )
        } else {
            let base_url = normalize_base_url(request.base_url.as_deref().unwrap_or_default())?;
            let api_key = request
                .api_key
                .filter(|key| !key.trim().is_empty())
                .ok_or_else(|| ModelRegistryError::Validation("API key is required".to_string()))?;
            validate_timeout(request.timeout_secs)?;
            (base_url, api_key, request.timeout_secs)
        };
        let client = OpenAiCompatClient::new(OpenAiCompatConfig {
            base_url,
            model: "discovery".to_string(),
            api_key,
            timeout: Duration::from_secs(timeout_secs),
        })?;
        let models = client
            .list_models()
            .await?
            .into_iter()
            .map(|id| DiscoveredModel {
                suggested: deepseek_model_template(&id),
                id,
            })
            .collect();
        Ok(DiscoverModelsResponse { models })
    }

    fn session_key(&self, session: &str) -> String {
        format!("{}:{session}", self.workspace_scope)
    }
}

fn load_store(path: &Path) -> Result<PersistedModelStore, ModelRegistryError> {
    if !path.is_file() {
        return Ok(PersistedModelStore::default());
    }
    let content = fs::read_to_string(path).map_err(|source| ModelRegistryError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let store = serde_json::from_str::<PersistedModelStore>(&content).map_err(|source| {
        ModelRegistryError::Parse {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if store.schema_version != MODEL_STORE_SCHEMA_VERSION {
        return Err(ModelRegistryError::UnsupportedSchema {
            version: store.schema_version,
            expected: MODEL_STORE_SCHEMA_VERSION,
        });
    }
    Ok(store)
}

fn save_store(path: &Path, store: &PersistedModelStore) -> Result<(), ModelRegistryError> {
    let parent = path.parent().ok_or_else(|| {
        ModelRegistryError::Validation("model settings path has no parent".to_string())
    })?;
    fs::create_dir_all(parent).map_err(|source| ModelRegistryError::CreateDir {
        path: parent.to_path_buf(),
        source,
    })?;
    let content = serde_json::to_vec_pretty(store).map_err(ModelRegistryError::Serialize)?;
    let temp = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("web-models.json"),
        std::process::id()
    ));
    fs::write(&temp, content).map_err(|source| ModelRegistryError::Write {
        path: temp.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).map_err(|source| {
            ModelRegistryError::Write {
                path: temp.clone(),
                source,
            }
        })?;
    }
    fs::rename(&temp, path).map_err(|source| ModelRegistryError::Replace {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn build_clients(
    store: &PersistedModelStore,
) -> Result<HashMap<(String, String), OpenAiCompatClient>, ModelRegistryError> {
    let mut clients = HashMap::new();
    for provider in store.providers.iter().filter(|provider| provider.enabled) {
        for model in &provider.models {
            let client = OpenAiCompatClient::new(OpenAiCompatConfig {
                base_url: provider.base_url.clone(),
                model: model.id.clone(),
                api_key: provider.api_key.clone(),
                timeout: Duration::from_secs(provider.timeout_secs),
            })?;
            clients.insert((provider.id.clone(), model.id.clone()), client);
        }
    }
    Ok(clients)
}

fn commit_store(
    path: &Path,
    state: &mut RegistryState,
    store: PersistedModelStore,
    fallback: Option<&FallbackModel>,
) -> Result<(), ModelRegistryError> {
    validate_store(&store, fallback)?;
    let clients = build_clients(&store)?;
    save_store(path, &store)?;
    state.store = store;
    state.clients = clients;
    Ok(())
}

fn validate_store(
    store: &PersistedModelStore,
    fallback: Option<&FallbackModel>,
) -> Result<(), ModelRegistryError> {
    let mut ids = HashSet::new();
    for provider in &store.providers {
        if !ids.insert(provider.id.as_str()) {
            return Err(ModelRegistryError::Validation(format!(
                "duplicate provider id {:?}",
                provider.id
            )));
        }
        validate_provider(provider)?;
    }
    if let Some(selection) = store.default_selection.as_ref() {
        let state = RegistryState {
            store: store.clone(),
            clients: build_clients(store)?,
        };
        resolve_from_state(&state, fallback, selection)?;
    }
    Ok(())
}

fn validate_provider_request(
    request: &ProviderWriteRequest,
    current_id: Option<&str>,
    providers: &[ManagedProvider],
) -> Result<(), ModelRegistryError> {
    if request.name.trim().is_empty() {
        return Err(ModelRegistryError::Validation(
            "provider name is required".to_string(),
        ));
    }
    if providers.iter().any(|provider| {
        Some(provider.id.as_str()) != current_id
            && provider.name.eq_ignore_ascii_case(request.name.trim())
    }) {
        return Err(ModelRegistryError::Validation(
            "provider name must be unique".to_string(),
        ));
    }
    normalize_base_url(&request.base_url)?;
    validate_timeout(request.timeout_secs)?;
    if request.models.is_empty() {
        return Err(ModelRegistryError::Validation(
            "at least one model is required".to_string(),
        ));
    }
    validate_models(&request.models)
}

fn validate_provider(provider: &ManagedProvider) -> Result<(), ModelRegistryError> {
    if provider.name.trim().is_empty() || provider.api_key.trim().is_empty() {
        return Err(ModelRegistryError::Validation(format!(
            "provider {:?} is missing name or API key",
            provider.id
        )));
    }
    normalize_base_url(&provider.base_url)?;
    validate_timeout(provider.timeout_secs)?;
    validate_models(&provider.models)
}

fn validate_models(models: &[ManagedModel]) -> Result<(), ModelRegistryError> {
    let mut ids = HashSet::new();
    for model in models {
        if model.id.trim().is_empty() || model.name.trim().is_empty() {
            return Err(ModelRegistryError::Validation(
                "model id and name are required".to_string(),
            ));
        }
        if !ids.insert(model.id.trim()) {
            return Err(ModelRegistryError::Validation(format!(
                "duplicate model id {:?}",
                model.id
            )));
        }
        if model.context_window_tokens == 0
            || model.reserved_output_tokens == 0
            || model.reserved_output_tokens >= model.context_window_tokens
        {
            return Err(ModelRegistryError::Validation(format!(
                "model {:?} must have positive context values with reserved output below context window",
                model.id
            )));
        }
    }
    Ok(())
}

fn validate_timeout(timeout_secs: u64) -> Result<(), ModelRegistryError> {
    if !(1..=600).contains(&timeout_secs) {
        return Err(ModelRegistryError::Validation(
            "timeout must be between 1 and 600 seconds".to_string(),
        ));
    }
    Ok(())
}

fn provider_from_request(
    id: String,
    request: ProviderWriteRequest,
    current: Option<&ManagedProvider>,
) -> Result<ManagedProvider, ModelRegistryError> {
    let api_key = request
        .api_key
        .filter(|key| !key.trim().is_empty())
        .or_else(|| current.map(|provider| provider.api_key.clone()))
        .ok_or_else(|| ModelRegistryError::Validation("API key is required".to_string()))?;
    Ok(ManagedProvider {
        id,
        name: request.name.trim().to_string(),
        base_url: normalize_base_url(&request.base_url)?,
        api_key,
        enabled: request.enabled,
        timeout_secs: request.timeout_secs,
        models: request
            .models
            .into_iter()
            .map(|model| ManagedModel {
                id: model.id.trim().to_string(),
                name: model.name.trim().to_string(),
                ..model
            })
            .collect(),
    })
}

fn normalize_base_url(base_url: &str) -> Result<String, ModelRegistryError> {
    let base_url = base_url.trim().trim_end_matches('/');
    if base_url.is_empty() || !(base_url.starts_with("https://") || base_url.starts_with("http://"))
    {
        return Err(ModelRegistryError::Validation(
            "base URL must start with http:// or https://".to_string(),
        ));
    }
    Ok(base_url.to_string())
}

fn validate_selection_for_provider(
    provider: &ManagedProvider,
    selection: &ModelSelection,
) -> Result<(), ModelRegistryError> {
    if !provider.enabled {
        return Err(ModelRegistryError::SelectionUnavailable(format!(
            "provider {:?} is disabled",
            provider.name
        )));
    }
    let model = provider
        .models
        .iter()
        .find(|model| model.id == selection.model_id)
        .ok_or_else(|| {
            ModelRegistryError::SelectionUnavailable(format!(
                "model {:?} was not found",
                selection.model_id
            ))
        })?;
    validate_reasoning(model.reasoning_profile, selection.reasoning)
}

fn validate_reasoning(
    profile: ReasoningProfile,
    reasoning: ReasoningLevel,
) -> Result<(), ModelRegistryError> {
    if profile == ReasoningProfile::None && reasoning != ReasoningLevel::Off {
        return Err(ModelRegistryError::SelectionUnavailable(
            "this model does not support configurable reasoning".to_string(),
        ));
    }
    Ok(())
}

fn resolve_from_state(
    state: &RegistryState,
    fallback: Option<&FallbackModel>,
    selection: &ModelSelection,
) -> Result<ResolvedModel, ModelRegistryError> {
    if selection.provider_id == FALLBACK_PROVIDER_ID {
        let fallback = fallback.ok_or_else(|| {
            ModelRegistryError::SelectionUnavailable(
                "the current config model is unavailable".to_string(),
            )
        })?;
        if fallback.model_id != selection.model_id {
            return Err(ModelRegistryError::SelectionUnavailable(format!(
                "model {:?} is not the current config model",
                selection.model_id
            )));
        }
        validate_reasoning(fallback.reasoning_profile, selection.reasoning)?;
        let client = fallback
            .client
            .clone()
            .with_request_options(OpenAiCompatRequestOptions {
                reasoning_profile: fallback.reasoning_profile,
                reasoning: selection.reasoning,
                supports_tools: true,
            });
        return Ok(ResolvedModel {
            selection: selection.clone(),
            invocation: ModelInvocation {
                provider_id: FALLBACK_PROVIDER_ID.to_string(),
                provider_name: fallback.provider_name.clone(),
                model_id: fallback.model_id.clone(),
                model_name: fallback.model_name.clone(),
                reasoning: selection.reasoning,
            },
            client,
            limits: fallback.limits,
        });
    }

    let provider = state
        .store
        .providers
        .iter()
        .find(|provider| provider.id == selection.provider_id)
        .ok_or_else(|| {
            ModelRegistryError::SelectionUnavailable(format!(
                "provider {:?} was not found",
                selection.provider_id
            ))
        })?;
    validate_selection_for_provider(provider, selection)?;
    let model = provider
        .models
        .iter()
        .find(|model| model.id == selection.model_id)
        .expect("validated model must exist");
    let client = state
        .clients
        .get(&(provider.id.clone(), model.id.clone()))
        .cloned()
        .ok_or_else(|| {
            ModelRegistryError::SelectionUnavailable(
                "model client is unavailable; save the provider again".to_string(),
            )
        })?
        .with_request_options(OpenAiCompatRequestOptions {
            reasoning_profile: model.reasoning_profile,
            reasoning: selection.reasoning,
            supports_tools: model.supports_tools,
        });
    Ok(ResolvedModel {
        selection: selection.clone(),
        invocation: ModelInvocation {
            provider_id: provider.id.clone(),
            provider_name: provider.name.clone(),
            model_id: model.id.clone(),
            model_name: model.name.clone(),
            reasoning: selection.reasoning,
        },
        client,
        limits: ModelContextLimits {
            context_window_tokens: model.context_window_tokens,
            reserved_output_tokens: model.reserved_output_tokens,
        },
    })
}

fn effective_default(
    store: &PersistedModelStore,
    fallback: Option<&FallbackModel>,
) -> Option<ModelSelection> {
    if let Some(selection) = store.default_selection.as_ref() {
        let clients = build_clients(store).ok()?;
        let state = RegistryState {
            store: store.clone(),
            clients,
        };
        if resolve_from_state(&state, fallback, selection).is_ok() {
            return Some(selection.clone());
        }
    }
    fallback.map(fallback_selection)
}

fn fallback_selection(fallback: &FallbackModel) -> ModelSelection {
    ModelSelection {
        provider_id: FALLBACK_PROVIDER_ID.to_string(),
        model_id: fallback.model_id.clone(),
        reasoning: if fallback.reasoning_profile == ReasoningProfile::Deepseek {
            ReasoningLevel::High
        } else {
            ReasoningLevel::Off
        },
    }
}

fn remove_invalid_session_selections(
    store: &mut PersistedModelStore,
    fallback: Option<&FallbackModel>,
) {
    let clients = build_clients(store).unwrap_or_default();
    let state = RegistryState {
        store: store.clone(),
        clients,
    };
    store
        .session_selections
        .retain(|_, selection| resolve_from_state(&state, fallback, selection).is_ok());
}

fn provider_response(provider: &ManagedProvider) -> ModelProviderResponse {
    ModelProviderResponse {
        id: provider.id.clone(),
        name: provider.name.clone(),
        base_url: provider.base_url.clone(),
        api_format: "openai_chat_completions",
        enabled: provider.enabled,
        read_only: false,
        api_key_configured: !provider.api_key.is_empty(),
        timeout_secs: provider.timeout_secs,
        models: provider.models.clone(),
    }
}

fn fallback_response(fallback: &FallbackModel) -> ModelProviderResponse {
    ModelProviderResponse {
        id: FALLBACK_PROVIDER_ID.to_string(),
        name: fallback.provider_name.clone(),
        base_url: "Configured in morrow.toml".to_string(),
        api_format: "openai_chat_completions",
        enabled: true,
        read_only: true,
        api_key_configured: true,
        timeout_secs: DEFAULT_TIMEOUT_SECS,
        models: vec![ManagedModel {
            id: fallback.model_id.clone(),
            name: fallback.model_name.clone(),
            context_window_tokens: fallback.limits.context_window_tokens,
            reserved_output_tokens: fallback.limits.reserved_output_tokens,
            supports_tools: true,
            reasoning_profile: fallback.reasoning_profile,
        }],
    }
}

fn unique_provider_id(name: &str, providers: &[ManagedProvider]) -> String {
    let slug = slugify(name);
    let base = if slug.is_empty() {
        "provider".to_string()
    } else {
        slug
    };
    let mut id = base.clone();
    let mut suffix = 2;
    while providers.iter().any(|provider| provider.id == id) || id == FALLBACK_PROVIDER_ID {
        id = format!("{base}-{suffix}");
        suffix += 1;
    }
    id
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

fn deepseek_model_template(id: &str) -> Option<ManagedModel> {
    let name = match id {
        "deepseek-v4-flash" => "DeepSeek V4 Flash",
        "deepseek-v4-pro" => "DeepSeek V4 Pro",
        _ => return None,
    };
    Some(ManagedModel {
        id: id.to_string(),
        name: name.to_string(),
        context_window_tokens: 1_000_000,
        reserved_output_tokens: DEFAULT_RESERVED_OUTPUT_TOKENS,
        supports_tools: true,
        reasoning_profile: ReasoningProfile::Deepseek,
    })
}

fn default_true() -> bool {
    true
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir()
            .join(format!("morrow-models-{name}-{stamp}"))
            .join("web-models.json")
    }

    fn provider_request(api_key: Option<&str>) -> ProviderWriteRequest {
        ProviderWriteRequest {
            name: "DeepSeek".to_string(),
            base_url: "https://api.deepseek.com".to_string(),
            api_key: api_key.map(str::to_string),
            enabled: true,
            timeout_secs: 120,
            models: vec![deepseek_model_template("deepseek-v4-pro").expect("template")],
            default_model: Some(DefaultModelRequest {
                model_id: "deepseek-v4-pro".to_string(),
                reasoning: ReasoningLevel::High,
            }),
        }
    }

    #[tokio::test]
    async fn provider_key_is_persisted_but_never_returned() {
        let path = unique_path("redaction");
        let registry = ModelRegistry::load(path.clone(), Path::new("."), None).expect("registry");
        let response = registry
            .create_provider(provider_request(Some("secret-key")))
            .await
            .expect("create");
        let settings = registry.settings().await;

        assert!(response.api_key_configured);
        assert!(
            !serde_json::to_string(&settings)
                .expect("json")
                .contains("secret-key")
        );
        assert!(
            fs::read_to_string(&path)
                .expect("stored")
                .contains("secret-key")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[tokio::test]
    async fn first_provider_requires_explicit_default() {
        let path = unique_path("default");
        let registry = ModelRegistry::load(path, Path::new("."), None).expect("registry");
        let mut request = provider_request(Some("key"));
        request.default_model = None;

        let error = registry.create_provider(request).await.expect_err("reject");

        assert!(error.to_string().contains("first provider"));
    }

    #[tokio::test]
    async fn failed_save_does_not_mutate_registry_state() {
        let path = unique_path("failed-save");
        fs::write(path.parent().expect("parent"), "not a directory")
            .expect("write blocking parent");
        let registry = ModelRegistry::load(path, Path::new("."), None).expect("registry");

        registry
            .create_provider(provider_request(Some("key")))
            .await
            .expect_err("save must fail");

        let settings = registry.settings().await;
        assert!(settings.providers.is_empty());
        assert_eq!(settings.default_selection, None);
        assert!(!settings.model_ready);
    }

    #[tokio::test]
    async fn deleting_default_provider_reports_conflict_without_mutation() {
        let path = unique_path("delete-default");
        let registry = ModelRegistry::load(path, Path::new("."), None).expect("registry");
        let provider = registry
            .create_provider(provider_request(Some("key")))
            .await
            .expect("create");

        let error = registry
            .delete_provider(&provider.id)
            .await
            .expect_err("default provider must remain");

        assert!(matches!(error, ModelRegistryError::Conflict(_)));
        let settings = registry.settings().await;
        assert_eq!(settings.providers.len(), 1);
        assert_eq!(
            settings
                .default_selection
                .as_ref()
                .map(|selection| selection.provider_id.as_str()),
            Some(provider.id.as_str())
        );
    }

    #[tokio::test]
    async fn session_selection_overrides_global_default() {
        let path = unique_path("session");
        let registry = ModelRegistry::load(path, Path::new("."), None).expect("registry");
        let response = registry
            .create_provider(provider_request(Some("key")))
            .await
            .expect("create");
        let selection = ModelSelection {
            provider_id: response.id,
            model_id: "deepseek-v4-pro".to_string(),
            reasoning: ReasoningLevel::Max,
        };

        registry
            .set_session_selection("work", selection.clone())
            .await
            .expect("set");

        let resolved = registry.session_selection("work").await;
        assert_eq!(resolved.selection, Some(selection));
        assert!(!resolved.inherited);
    }

    #[tokio::test]
    async fn session_selections_are_isolated_between_workspaces() {
        let path = unique_path("workspace-session");
        let first = ModelRegistry::load(path.clone(), Path::new("workspace-a"), None)
            .expect("first registry");
        let response = first
            .create_provider(provider_request(Some("key")))
            .await
            .expect("create provider");
        let selection = ModelSelection {
            provider_id: response.id,
            model_id: "deepseek-v4-pro".to_string(),
            reasoning: ReasoningLevel::Max,
        };
        first
            .set_session_selection("work", selection.clone())
            .await
            .expect("set first workspace selection");

        let second =
            ModelRegistry::load(path, Path::new("workspace-b"), None).expect("second registry");
        let first_selection = first.session_selection("work").await;
        let second_selection = second.session_selection("work").await;

        assert_eq!(first_selection.selection, Some(selection));
        assert!(!first_selection.inherited);
        assert!(second_selection.inherited);
        assert_ne!(second_selection.selection, first_selection.selection);
    }
}
