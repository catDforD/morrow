use super::*;

pub(crate) const AGENTS_MD_FILE_NAME: &str = "AGENTS.md";
pub(crate) const MAX_AGENTS_MD_BYTES: u64 = 32 * 1024;
pub(crate) const PROJECT_INSTRUCTIONS_PREFIX: &str = "Project instructions from AGENTS.md. Follow them for work in this workspace unless they conflict with runtime safety or role constraints:\n<project_instructions>";
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInstructionsLoad {
    pub effective_system_prompt: String,
    pub diagnostics: Vec<String>,
}

pub fn load_workspace_instructions(
    workspace_root: &Path,
    base_system_prompt: &str,
) -> WorkspaceInstructionsLoad {
    let path = workspace_root.join(AGENTS_MD_FILE_NAME);
    match read_agents_md(&path) {
        AgentsMdRead::Absent => unchanged_workspace_instructions(base_system_prompt),
        AgentsMdRead::Rejected(diagnostic) => {
            workspace_instruction_diagnostic(base_system_prompt, diagnostic)
        }
        AgentsMdRead::Content(content) => WorkspaceInstructionsLoad {
            effective_system_prompt: join_workspace_instructions(
                base_system_prompt,
                &project_instructions_section(&content),
            ),
            diagnostics: Vec::new(),
        },
    }
}

enum AgentsMdRead {
    /// 文件缺失或内容为空。
    Absent,
    /// 修剪后的有效内容。
    Content(String),
    /// 读取被拒绝，附带诊断信息。
    Rejected(String),
}

fn read_agents_md(path: &Path) -> AgentsMdRead {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return AgentsMdRead::Absent,
        Err(error) => {
            return AgentsMdRead::Rejected(format!(
                "failed to inspect {}: {error}",
                path.display()
            ));
        }
    };

    if !metadata.file_type().is_file() {
        return AgentsMdRead::Rejected(format!(
            "ignored {}: AGENTS.md must be a regular file and symbolic links are not supported",
            path.display()
        ));
    }

    if metadata.len() > MAX_AGENTS_MD_BYTES {
        return AgentsMdRead::Rejected(oversized_agents_md_diagnostic(path, metadata.len()));
    }

    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return AgentsMdRead::Rejected(format!("failed to read {}: {error}", path.display()));
        }
    };
    if bytes.len() as u64 > MAX_AGENTS_MD_BYTES {
        return AgentsMdRead::Rejected(oversized_agents_md_diagnostic(path, bytes.len() as u64));
    }

    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(error) => {
            return AgentsMdRead::Rejected(format!(
                "ignored {}: AGENTS.md is not valid UTF-8: {error}",
                path.display()
            ));
        }
    };
    let content = content.trim_start_matches('\u{feff}').trim();
    if content.is_empty() {
        return AgentsMdRead::Absent;
    }
    AgentsMdRead::Content(content.to_string())
}

fn oversized_agents_md_diagnostic(path: &Path, bytes: u64) -> String {
    format!(
        "ignored {}: AGENTS.md is {bytes} bytes and exceeds the {MAX_AGENTS_MD_BYTES}-byte limit",
        path.display()
    )
}

fn project_instructions_section(content: &str) -> String {
    format!("{PROJECT_INSTRUCTIONS_PREFIX}\n{content}\n</project_instructions>")
}

/// base + AGENTS.md 段落拼接；section 为空时原样返回 base。
fn join_workspace_instructions(base_system_prompt: &str, section: &str) -> String {
    if section.is_empty() {
        return base_system_prompt.to_string();
    }
    let base_system_prompt = base_system_prompt.trim_end();
    if base_system_prompt.is_empty() {
        section.to_string()
    } else {
        format!("{base_system_prompt}\n\n{section}")
    }
}

/// AGENTS.md 的每轮重读缓存：记录 path、mtime 与组装好的
/// `<project_instructions>` 段落。mtime 未变时命中缓存、零文件读取；
/// 文件变更后下一个 turn 自动生效。生命周期由调用方持有（CLI/server 进程级），
/// turn 通过 `RunAgentTurnContext::workspace_instructions` 引用。
#[derive(Debug)]
pub struct WorkspaceInstructionsCache {
    path: PathBuf,
    state: std::sync::Mutex<WorkspaceInstructionsCacheState>,
}

#[derive(Debug, Default)]
struct WorkspaceInstructionsCacheState {
    loaded: bool,
    mtime: Option<SystemTime>,
    section: String,
}

impl WorkspaceInstructionsCache {
    pub fn new(workspace_root: &Path) -> Self {
        Self {
            path: workspace_root.join(AGENTS_MD_FILE_NAME),
            state: std::sync::Mutex::new(WorkspaceInstructionsCacheState::default()),
        }
    }

    /// 冷启动预热：立即读取并返回诊断信息（与 `load_workspace_instructions` 同源规则）。
    pub fn prewarm(&self) -> Vec<String> {
        self.refresh().1
    }

    /// 当前 `<project_instructions>` 段落；AGENTS.md 缺失、为空或被忽略时为空串。
    pub fn section(&self) -> String {
        let mtime = self.current_mtime();
        {
            let state = self.lock_state();
            if state.loaded && state.mtime == mtime {
                return state.section.clone();
            }
        }
        self.refresh().0
    }

    /// base + 每轮新鲜的 AGENTS.md 段落。
    pub fn apply(&self, base_system_prompt: &str) -> String {
        join_workspace_instructions(base_system_prompt, &self.section())
    }

    fn refresh(&self) -> (String, Vec<String>) {
        // 先取 mtime 再读内容：读的过程中文件被改写会让缓存的 mtime 偏旧，
        // 下一 turn 触发重读，而不是把旧内容钉在新 mtime 上。
        let mtime = self.current_mtime();
        let (section, diagnostics) = match read_agents_md(&self.path) {
            AgentsMdRead::Absent => (String::new(), Vec::new()),
            AgentsMdRead::Content(content) => (project_instructions_section(&content), Vec::new()),
            AgentsMdRead::Rejected(diagnostic) => (String::new(), vec![diagnostic]),
        };
        *self.lock_state() = WorkspaceInstructionsCacheState {
            loaded: true,
            mtime,
            section: section.clone(),
        };
        (section, diagnostics)
    }

    fn current_mtime(&self) -> Option<SystemTime> {
        fs::symlink_metadata(&self.path)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, WorkspaceInstructionsCacheState> {
        self.state
            .lock()
            .expect("workspace instructions cache lock poisoned")
    }
}

fn unchanged_workspace_instructions(base_system_prompt: &str) -> WorkspaceInstructionsLoad {
    WorkspaceInstructionsLoad {
        effective_system_prompt: base_system_prompt.to_string(),
        diagnostics: Vec::new(),
    }
}

fn workspace_instruction_diagnostic(
    base_system_prompt: &str,
    diagnostic: String,
) -> WorkspaceInstructionsLoad {
    WorkspaceInstructionsLoad {
        effective_system_prompt: base_system_prompt.to_string(),
        diagnostics: vec![diagnostic],
    }
}

pub fn detect_workspace_root() -> Result<PathBuf, RuntimeError> {
    let cwd = std::env::current_dir().map_err(SessionStoreError::CurrentDir)?;
    let mut candidate = cwd.as_path();

    loop {
        let manifest = candidate.join("Cargo.toml");
        if manifest.is_file() && manifest_has_workspace_header(&manifest) {
            return Ok(candidate.to_path_buf());
        }
        let Some(parent) = candidate.parent() else {
            return Ok(cwd);
        };
        candidate = parent;
    }
}

fn manifest_has_workspace_header(path: &Path) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    content.lines().any(|line| line.trim() == "[workspace]")
}
