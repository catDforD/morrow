use super::*;

pub(crate) const DEFAULT_READ_LINES: usize = 200;
pub(crate) const MAX_READ_LINES: usize = 1000;
pub(crate) const DEFAULT_LIST_ENTRIES: usize = 100;
pub(crate) const MAX_LIST_ENTRIES: usize = 500;
pub const READ_FILE_TOOL_NAME: &str = "read_file";
pub const LIST_FILES_TOOL_NAME: &str = "list_files";
pub const EDIT_FILE_TOOL_NAME: &str = "edit_file";
pub const WRITE_FILE_TOOL_NAME: &str = "write_file";
pub(crate) fn parse_args<T: DeserializeOwned>(call: &ToolCall) -> Result<T, String> {
    serde_json::from_str(&call.function.arguments).map_err(|err| {
        invalid_arguments_message(
            &call.function.name,
            &err,
            known_tool_parameters(&call.function.name).as_ref(),
        )
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReadFileArgs {
    pub(crate) path: String,
    pub(crate) start_line: Option<usize>,
    pub(crate) max_lines: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListFilesArgs {
    pub(crate) path: Option<String>,
    pub(crate) recursive: Option<bool>,
    pub(crate) max_entries: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EditFileArgs {
    pub(crate) path: String,
    pub(crate) old_text: String,
    pub(crate) new_text: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WriteFileArgs {
    pub(crate) path: String,
    pub(crate) content: String,
    pub(crate) overwrite: Option<bool>,
}
