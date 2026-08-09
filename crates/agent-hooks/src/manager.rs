use crate::adapter::register_hook;
use crate::config::{LoadedHooks, load_hook_file};
use crate::trust::{hook_fingerprint, load_trust_store, save_trust_store, workspace_key};
use crate::types::{HookError, HookSettings, HookSnapshot};
use agent_protocol::MiddlewareSource;
use agent_runtime::MiddlewareRegistry;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

#[derive(Debug, Clone)]
pub struct HookManager {
    home_dir: PathBuf,
    workspace_root: PathBuf,
}

impl HookManager {
    pub fn for_workspace(workspace_root: impl Into<PathBuf>) -> Result<Self, HookError> {
        let home_dir = dirs::home_dir().ok_or(HookError::HomeDirNotFound)?;
        Ok(Self::new(home_dir, workspace_root))
    }

    pub fn new(home_dir: impl Into<PathBuf>, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            home_dir: home_dir.into(),
            workspace_root: workspace_root.into(),
        }
    }

    pub fn user_config_path(&self) -> PathBuf {
        self.home_dir.join(".morrow").join("hooks.toml")
    }

    pub fn project_config_path(&self) -> PathBuf {
        self.workspace_root.join(".morrow").join("hooks.toml")
    }

    pub fn trust_store_path(&self) -> PathBuf {
        self.home_dir.join(".morrow").join("hook-trust.json")
    }

    pub fn load_snapshot(&self) -> Result<HookSnapshot, HookError> {
        let loaded = self.load_configuration()?;
        let mut registry = MiddlewareRegistry::new();
        let context_budget = Arc::new(AtomicUsize::new(0));
        for hook in &loaded.user_hooks {
            register_hook(
                &mut registry,
                hook.clone(),
                MiddlewareSource::UserCommand,
                context_budget.clone(),
            );
        }
        if loaded.project_trusted {
            for hook in &loaded.project_hooks {
                register_hook(
                    &mut registry,
                    hook.clone(),
                    MiddlewareSource::ProjectCommand,
                    context_budget.clone(),
                );
            }
        }
        Ok(HookSnapshot {
            registry: Arc::new(registry),
            settings: self.settings_from(&loaded),
        })
    }

    pub fn settings(&self) -> Result<HookSettings, HookError> {
        let loaded = self.load_configuration()?;
        Ok(self.settings_from(&loaded))
    }

    pub fn trust_project(&self) -> Result<HookSettings, HookError> {
        let loaded = self.load_configuration()?;
        let fingerprint = loaded
            .project_fingerprint
            .clone()
            .ok_or(HookError::ProjectConfigNotFound)?;
        let trust_store_path = self.trust_store_path();
        let mut trust = load_trust_store(&trust_store_path)?;
        trust.trust(workspace_key(&self.workspace_root)?, fingerprint);
        save_trust_store(&trust_store_path, &trust)?;
        self.settings()
    }

    pub fn revoke_project(&self) -> Result<HookSettings, HookError> {
        let trust_store_path = self.trust_store_path();
        let mut trust = load_trust_store(&trust_store_path)?;
        trust.revoke(&workspace_key(&self.workspace_root)?);
        save_trust_store(&trust_store_path, &trust)?;
        self.settings()
    }

    fn load_configuration(&self) -> Result<LoadedHooks, HookError> {
        let user_hooks = load_hook_file(&self.user_config_path())?.unwrap_or_default();
        let project_config_path = self.project_config_path();
        let project_hooks = load_hook_file(&project_config_path)?.unwrap_or_default();
        let project_fingerprint = (!project_hooks.is_empty() || project_config_path.is_file())
            .then(|| hook_fingerprint(&project_hooks))
            .transpose()?;
        let trust = load_trust_store(&self.trust_store_path())?;
        let project_trusted = match project_fingerprint.as_deref() {
            Some(fingerprint) => {
                trust.is_trusted(&workspace_key(&self.workspace_root)?, fingerprint)
            }
            None => false,
        };
        Ok(LoadedHooks {
            user_hooks,
            project_hooks,
            project_fingerprint,
            project_trusted,
        })
    }

    fn settings_from(&self, loaded: &LoadedHooks) -> HookSettings {
        loaded.settings(
            &self.user_config_path(),
            &self.project_config_path(),
            &self.trust_store_path(),
        )
    }
}
