use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathCompletion {
    pub path: String,
    pub directory: bool,
}

/// Walk a workspace with gitignore/global-ignore rules enabled.
///
/// Returned paths are workspace-relative, use `/` separators on every platform, and
/// never read file contents. Results are sorted with directories first.
pub fn complete_workspace_paths(
    workspace: &Path,
    query: &str,
    limit: usize,
) -> Vec<PathCompletion> {
    if limit == 0 || !workspace.is_dir() {
        return Vec::new();
    }

    let normalized_query = query.trim_start_matches("./").replace('\\', "/");
    let query_lower = normalized_query.to_lowercase();
    let mut matches = Vec::new();
    let walker = WalkBuilder::new(workspace)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .follow_links(false)
        .build();

    for entry in walker.flatten().skip(1) {
        let Ok(relative) = entry.path().strip_prefix(workspace) else {
            continue;
        };
        let relative = slash_path(relative);
        if relative.is_empty() || !path_matches(&relative, &query_lower) {
            continue;
        }
        let directory = entry.file_type().is_some_and(|kind| kind.is_dir());
        matches.push(PathCompletion {
            path: if directory {
                format!("{relative}/")
            } else {
                relative
            },
            directory,
        });
    }

    matches.sort_by(|left, right| {
        right
            .directory
            .cmp(&left.directory)
            .then_with(|| {
                completion_rank(&left.path, &query_lower)
                    .cmp(&completion_rank(&right.path, &query_lower))
            })
            .then_with(|| left.path.cmp(&right.path))
    });
    matches.truncate(limit);
    matches
}

fn slash_path(path: &Path) -> String {
    let mut output = String::new();
    for component in path.components() {
        if !output.is_empty() {
            output.push('/');
        }
        output.push_str(&component.as_os_str().to_string_lossy());
    }
    output
}

fn path_matches(path: &str, query_lower: &str) -> bool {
    query_lower.is_empty() || path.to_lowercase().contains(query_lower)
}

fn completion_rank(path: &str, query_lower: &str) -> (u8, usize) {
    let lower = path.to_lowercase();
    if lower.starts_with(query_lower) {
        return (0, path.len());
    }
    if PathBuf::from(path).file_name().is_some_and(|name| {
        name.to_string_lossy()
            .to_lowercase()
            .starts_with(query_lower)
    }) {
        return (1, path.len());
    }
    (2, path.len())
}

/// Locate the `@path` token containing the cursor. Email-like `word@host` text is not
/// considered a path trigger.
pub(crate) fn path_token(text: &str, cursor: usize) -> Option<(std::ops::Range<usize>, String)> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let before = &text[..cursor];
    let at = before.rfind('@')?;
    if at > 0 {
        let previous = before[..at].chars().next_back()?;
        if !previous.is_whitespace() && !matches!(previous, '(' | '[' | '{' | '"' | '\'') {
            return None;
        }
    }
    let query = &before[at + 1..];
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    Some((at + 1..cursor, query.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_token_ignores_email_and_finds_workspace_reference() {
        assert_eq!(path_token("mail@example.com", 16), None);
        assert_eq!(
            path_token("看看 @crates/agent", 20),
            Some((8..20, "crates/agent".to_string()))
        );
    }

    #[test]
    fn completion_obeys_gitignore() {
        let root = std::env::temp_dir().join(format!(
            "morrow-tui-complete-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("ignored")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("ignored/secret.rs"), "").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();

        let results = complete_workspace_paths(&root, "rs", 20);
        assert!(results.iter().any(|entry| entry.path == "src/lib.rs"));
        assert!(!results.iter().any(|entry| entry.path.contains("secret")));

        std::fs::remove_dir_all(root).unwrap();
    }
}
