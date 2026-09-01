use super::*;

pub(crate) const DEFAULT_SEARCH_RESULTS: usize = 100;
pub(crate) const MAX_SEARCH_RESULTS: usize = 200;
pub(crate) const MAX_SEARCH_LINE_CHARS: usize = 500;
const MAX_SEARCH_TOTAL_BYTES: usize = 20_000;
pub(crate) const SEARCH_SKIP_NAMES: &[&str] = &[".git", "node_modules", "dist", "build", "target"];
pub const SEARCH_TEXT_TOOL_NAME: &str = "search_text";
pub(crate) fn clamp_limit(
    value: Option<usize>,
    default: usize,
    max: usize,
) -> Result<usize, String> {
    let value = value.unwrap_or(default).min(max);
    if value == 0 {
        return Err("limit must be at least 1".to_string());
    }
    Ok(value)
}

pub(crate) fn should_skip_entry(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SEARCH_SKIP_NAMES.contains(&name))
}

#[cfg(windows)]
fn ripgrep_sidecar_name() -> &'static str {
    "morrow-rg.exe"
}

#[cfg(not(windows))]
fn ripgrep_sidecar_name() -> &'static str {
    "morrow-rg"
}

#[cfg(windows)]
fn path_ripgrep_name() -> &'static str {
    "rg.exe"
}

#[cfg(not(windows))]
fn path_ripgrep_name() -> &'static str {
    "rg"
}

pub(crate) fn ripgrep_binary() -> Option<PathBuf> {
    if let Ok(current_exe) = env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        let sidecar = dir.join(ripgrep_sidecar_name());
        if sidecar.is_file() {
            return Some(sidecar);
        }
    }

    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|dir| dir.join(path_ripgrep_name()))
            .find(|candidate| candidate.is_file())
    })
}

#[derive(Debug)]
pub(crate) enum RipgrepSearchError {
    Unavailable,
    Failed(String),
}

pub(crate) struct SearchOutput {
    pub(crate) query: String,
    pub(crate) path: String,
    pub(crate) case_sensitive: bool,
    pub(crate) max_results: usize,
    pub(crate) total_result_bytes: usize,
    pub(crate) result_truncated: bool,
    pub(crate) results: Vec<Value>,
}

impl SearchOutput {
    pub(crate) fn new(
        query: impl Into<String>,
        path: impl Into<String>,
        case_sensitive: bool,
        max_results: usize,
    ) -> Self {
        Self {
            query: query.into(),
            path: path.into(),
            case_sensitive,
            max_results,
            total_result_bytes: 0,
            result_truncated: false,
            results: Vec::new(),
        }
    }

    pub(crate) fn push_match(&mut self, path: String, line: usize, text: String) -> bool {
        if self.results.len() >= self.max_results {
            self.result_truncated = true;
            return false;
        }

        let (text, text_truncated) = truncate_chars(trim_line_endings(text), MAX_SEARCH_LINE_CHARS);
        let item = json!({
            "path": path,
            "line": line,
            "text": text,
            "text_truncated": text_truncated,
        });
        let item_bytes = serde_json::to_vec(&item)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX);
        if self.total_result_bytes.saturating_add(item_bytes) > MAX_SEARCH_TOTAL_BYTES {
            self.result_truncated = true;
            return false;
        }

        self.total_result_bytes += item_bytes;
        self.results.push(item);
        true
    }

    pub(crate) fn into_value(self) -> Value {
        json!({
            "query": self.query,
            "path": self.path,
            "case_sensitive": self.case_sensitive,
            "truncated": self.result_truncated,
            "result_truncated": self.result_truncated,
            "results": self.results,
        })
    }
}

fn trim_line_endings(mut text: String) -> String {
    while text.ends_with('\n') || text.ends_with('\r') {
        text.pop();
    }
    text
}

#[derive(Debug, Deserialize)]
struct RipgrepEvent {
    #[serde(rename = "type")]
    kind: String,
    data: Option<RipgrepEventData>,
}

#[derive(Debug, Deserialize)]
struct RipgrepEventData {
    path: Option<RipgrepText>,
    lines: Option<RipgrepText>,
    line_number: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct RipgrepText {
    text: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RipgrepMatch {
    pub(crate) path: String,
    pub(crate) line: usize,
    pub(crate) text: String,
}

pub(crate) fn parse_ripgrep_match(frame: &str) -> Result<Option<RipgrepMatch>, RipgrepSearchError> {
    if frame.trim().is_empty() {
        return Ok(None);
    }
    let event = serde_json::from_str::<RipgrepEvent>(frame).map_err(|err| {
        RipgrepSearchError::Failed(format!("failed to parse ripgrep JSON output: {err}"))
    })?;
    if event.kind != "match" {
        return Ok(None);
    }
    let Some(data) = event.data else {
        return Ok(None);
    };
    let Some(path) = data.path.and_then(|path| path.text) else {
        return Ok(None);
    };
    let Some(text) = data.lines.and_then(|lines| lines.text) else {
        return Ok(None);
    };
    let Some(line) = data.line_number else {
        return Ok(None);
    };

    Ok(Some(RipgrepMatch { path, line, text }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct SearchTextArgs {
    pub(crate) query: String,
    pub(crate) path: Option<String>,
    pub(crate) case_sensitive: Option<bool>,
    pub(crate) max_results: Option<usize>,
}

pub(crate) struct SearchOptions<'a> {
    pub(crate) query: &'a str,
    pub(crate) case_sensitive: bool,
    pub(crate) max_results: usize,
}
