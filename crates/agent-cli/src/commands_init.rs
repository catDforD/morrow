use super::*;

const INIT_CONFIG_MODEL: &str = "gpt-4.1";
const INIT_CONFIG_BASE_URL: &str = "https://api.openai.com/v1";
pub(crate) const INIT_CONFIG_API_KEY_PLACEHOLDER: &str = "replace-with-your-openai-api-key";
const INIT_CONFIG_TIMEOUT_SECS: u64 = 120;
const INIT_CONFIG_CONTEXT_WINDOW_TOKENS: usize = 1_047_576;
const INIT_CONFIG_RESERVED_OUTPUT_TOKENS: usize = 8_192;
pub(crate) const CONFIG_PROVIDER_ID: &str = "current-config";
pub(crate) const CONFIG_PROVIDER_NAME: &str = "默认配置";

pub(crate) fn handle_init_command(
    force: bool,
    template: bool,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    let path = default_config_path()?;
    let api_key = if template {
        INIT_CONFIG_API_KEY_PLACEHOLDER.to_string()
    } else {
        read_init_api_key()?
    };

    write_init_config(&path, &api_key, force)?;
    writeln!(stdout, "wrote config: {}", path.display()).map_err(CliError::Stdout)?;
    if template {
        writeln!(stdout, "edit [model].OPENAI_API_KEY before running morrow")
            .map_err(CliError::Stdout)?;
    } else {
        writeln!(stdout, "try: morrow \"hello\"").map_err(CliError::Stdout)?;
    }
    Ok(())
}

fn default_config_path() -> Result<PathBuf, CliError> {
    let home = dirs::home_dir().ok_or(CliError::HomeDirNotFound)?;
    Ok(default_config_path_for_home(&home))
}

pub(crate) fn default_config_path_for_home(home: &Path) -> PathBuf {
    home.join(".morrow").join("config.toml")
}

fn read_init_api_key() -> Result<String, CliError> {
    eprint!("OpenAI API key: ");
    io::stderr().flush().map_err(CliError::Stderr)?;
    let input = read_stdin_line()?.ok_or_else(|| CliError::EmptyApiKey)?;
    warn_if_lossy_input(&input);
    let api_key = input.text.trim().to_string();
    if api_key.is_empty() {
        return Err(CliError::EmptyApiKey);
    }
    Ok(api_key)
}

pub(crate) fn write_init_config(path: &Path, api_key: &str, force: bool) -> Result<(), CliError> {
    if path.exists() && !force {
        return Err(CliError::ConfigExists {
            path: path.to_path_buf(),
        });
    }
    if api_key.trim().is_empty() {
        return Err(CliError::EmptyApiKey);
    }

    let parent = path.parent().expect("config path must have parent");
    fs::create_dir_all(parent).map_err(|source| CliError::ConfigCreateDir {
        path: parent.to_path_buf(),
        source,
    })?;
    fs::write(path, render_init_config(api_key)).map_err(|source| CliError::ConfigWrite {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn render_init_config(api_key: &str) -> String {
    format!(
        r#"[model]
base_url = "{INIT_CONFIG_BASE_URL}"
model = "{INIT_CONFIG_MODEL}"
OPENAI_API_KEY = "{api_key}"
timeout_secs = {INIT_CONFIG_TIMEOUT_SECS}
context_window_tokens = {INIT_CONFIG_CONTEXT_WINDOW_TOKENS}
reserved_output_tokens = {INIT_CONFIG_RESERVED_OUTPUT_TOKENS}

[permissions]
mode = "read_only"
shell = "deny"
"#
    )
}
