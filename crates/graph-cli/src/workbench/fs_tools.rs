//! Workbench filesystem research tools: read, grep, and glob over the
//! project directory the workbench was started in, so the agent can ground
//! drafts and answers in real files. Read-only, contained to that
//! directory, and registered under `workbench__` alongside the draft tools
//! — never part of the plan catalog.

use async_trait::async_trait;
use graph_core::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const READ_FILE: &str = "workbench__read_file";
pub const GREP: &str = "workbench__grep";
pub const GLOB: &str = "workbench__glob";

const READ_DEFAULT_LIMIT: usize = 500;
const READ_MAX_LIMIT: usize = 2000;
const READ_MAX_LINE_CHARS: usize = 2000;
const READ_MAX_BYTES: usize = 128 * 1024;
const GREP_DEFAULT_MAX_MATCHES: usize = 100;
const GREP_MAX_MATCHES: usize = 500;
const GREP_MAX_LINE_CHARS: usize = 300;
const GLOB_MAX_FILES: usize = 500;
const WALK_MAX_ENTRIES: usize = 50_000;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

pub struct FsTools {
    root: PathBuf,
}

impl FsTools {
    /// `root` is the directory the tools are contained to — the directory
    /// the workbench was started in. Canonicalized once so `resolve` can
    /// compare prefixes against a stable form.
    pub fn new(root: PathBuf) -> std::io::Result<Self> {
        Ok(Self {
            root: root.canonicalize()?,
        })
    }
}

/// Resolve a user-supplied path against the root and require containment:
/// relative paths join to the root, canonicalization collapses `..` and
/// symlinks, and anything landing outside the root is rejected.
fn resolve(root: &Path, raw: &str) -> Result<PathBuf, String> {
    let path = Path::new(raw);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let canonical = candidate.canonicalize().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("'{raw}' not found")
        } else {
            format!("cannot resolve '{raw}': {error}")
        }
    })?;
    if !canonical.starts_with(root) {
        return Err(format!("'{raw}' escapes the project directory"));
    }
    Ok(canonical)
}

fn error_outcome(message: &str) -> ToolOutcome {
    ToolOutcome {
        result: json!({"error": message}),
        is_error: true,
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

/// Walk `dir` collecting files: gitignore-respecting (even outside a git
/// repo), dotfiles included, `.git` skipped, symlinks not followed. Sorted
/// for deterministic output. The bool reports hitting the entry cap.
fn walk_files(dir: &Path) -> (Vec<PathBuf>, bool) {
    let mut files = Vec::new();
    let mut capped = false;
    let mut seen = 0usize;
    for entry in ignore::WalkBuilder::new(dir)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
    {
        let Ok(entry) = entry else { continue };
        seen += 1;
        if seen > WALK_MAX_ENTRIES {
            capped = true;
            break;
        }
        if entry.file_type().is_some_and(|kind| kind.is_file()) {
            files.push(entry.into_path());
        }
    }
    files.sort();
    (files, capped)
}

fn read_file(root: &Path, input: &Value) -> ToolOutcome {
    let Some(raw) = input.get("path").and_then(Value::as_str) else {
        return error_outcome("read_file requires a 'path' string");
    };
    let path = match resolve(root, raw) {
        Ok(path) => path,
        Err(message) => return error_outcome(&message),
    };
    if path.is_dir() {
        return error_outcome(&format!(
            "'{raw}' is a directory — use workbench__glob to list files"
        ));
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => return error_outcome(&format!("cannot read '{raw}': {error}")),
    };
    if looks_binary(&bytes) {
        return error_outcome(&format!("'{raw}' is a binary file"));
    }
    let content = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    let offset = input
        .get("offset")
        .and_then(Value::as_u64)
        .map(|n| n.max(1) as usize)
        .unwrap_or(1);
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, READ_MAX_LIMIT))
        .unwrap_or(READ_DEFAULT_LIMIT);
    if offset > total_lines && total_lines > 0 {
        return error_outcome(&format!(
            "offset {offset} is beyond the end of '{raw}' ({total_lines} lines)"
        ));
    }

    let start = offset - 1;
    let mut out = String::new();
    let mut returned = 0usize;
    let mut byte_capped = false;
    for line in lines.iter().skip(start).take(limit) {
        let line = super::runner::truncate(line, READ_MAX_LINE_CHARS);
        if out.len() + line.len() + 1 > READ_MAX_BYTES {
            byte_capped = true;
            break;
        }
        out.push_str(&line);
        out.push('\n');
        returned += 1;
    }
    let truncated = byte_capped || start + returned < total_lines;
    ToolOutcome {
        result: json!({
            "path": raw,
            "content": out,
            "offset": offset,
            "linesReturned": returned,
            "totalLines": total_lines,
            "truncated": truncated,
        }),
        is_error: false,
    }
}

fn grep(root: &Path, input: &Value) -> ToolOutcome {
    let Some(pattern) = input.get("pattern").and_then(Value::as_str) else {
        return error_outcome("grep requires a 'pattern' string (Rust regex)");
    };
    let case_insensitive = input
        .get("case_insensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let regex = match regex::RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
    {
        Ok(regex) => regex,
        Err(error) => return error_outcome(&format!("invalid pattern: {error}")),
    };
    let max_matches = input
        .get("max_matches")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, GREP_MAX_MATCHES))
        .unwrap_or(GREP_DEFAULT_MAX_MATCHES);
    let name_filter = match input.get("glob").and_then(Value::as_str) {
        Some(pattern) => match glob_matcher(pattern) {
            Ok(matcher) => Some((matcher, pattern.contains('/'))),
            Err(message) => return error_outcome(&message),
        },
        None => None,
    };

    let raw_path = input.get("path").and_then(Value::as_str).unwrap_or(".");
    let path = match resolve(root, raw_path) {
        Ok(path) => path,
        Err(message) => return error_outcome(&message),
    };
    let (files, mut truncated) = if path.is_dir() {
        walk_files(&path)
    } else {
        (vec![path], false)
    };

    let mut matches = Vec::new();
    'files: for file in &files {
        let rel = relative(root, file);
        if let Some((matcher, match_full_path)) = &name_filter {
            let matched = if *match_full_path {
                matcher.is_match(&rel)
            } else {
                let name = file.file_name().unwrap_or_default().to_string_lossy();
                matcher.is_match(name.as_ref())
            };
            if !matched {
                continue;
            }
        }
        let Ok(bytes) = std::fs::read(file) else {
            continue;
        };
        if looks_binary(&bytes) {
            continue;
        }
        let content = String::from_utf8_lossy(&bytes);
        for (index, line) in content.lines().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            if matches.len() >= max_matches {
                truncated = true;
                break 'files;
            }
            matches.push(json!({
                "path": rel,
                "line": index + 1,
                "text": super::runner::truncate(line.trim(), GREP_MAX_LINE_CHARS),
            }));
        }
    }
    ToolOutcome {
        result: json!({
            "matches": matches,
            "matchCount": matches.len(),
            "truncated": truncated,
        }),
        is_error: false,
    }
}

/// Filename filters like `*.rs` match the file name; patterns containing a
/// separator (`src/**/*.rs`) match the root-relative path.
fn glob_matcher(pattern: &str) -> Result<globset::GlobMatcher, String> {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| format!("invalid glob: {error}"))
}

fn glob_find(root: &Path, input: &Value) -> ToolOutcome {
    let Some(pattern) = input.get("pattern").and_then(Value::as_str) else {
        return error_outcome("glob requires a 'pattern' string, e.g. **/*.rs");
    };
    let matcher = match glob_matcher(pattern) {
        Ok(matcher) => matcher,
        Err(message) => return error_outcome(&message),
    };
    let dir = match input.get("dir").and_then(Value::as_str) {
        Some(raw) => match resolve(root, raw) {
            Ok(dir) => dir,
            Err(message) => return error_outcome(&message),
        },
        None => root.to_path_buf(),
    };
    if !dir.is_dir() {
        return error_outcome("'dir' must be a directory");
    }

    let (all, mut truncated) = walk_files(&dir);
    let mut files = Vec::new();
    for file in &all {
        let rel = relative(root, file);
        if !matcher.is_match(&rel) {
            continue;
        }
        if files.len() >= GLOB_MAX_FILES {
            truncated = true;
            break;
        }
        files.push(rel);
    }
    ToolOutcome {
        result: json!({
            "files": files,
            "count": files.len(),
            "truncated": truncated,
        }),
        is_error: false,
    }
}

#[async_trait]
impl ToolRegistry for FsTools {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        let mut defs = vec![
            ToolDef {
                name: READ_FILE.to_string(),
                description: "Read a text file from the project directory the workbench was \
                              started in (paths outside it are rejected). Returns numbered \
                              window metadata; pass `offset`/`limit` to page through large \
                              files."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": {
                        "path": {"type": "string", "description": "File path, relative to the project directory (or absolute within it)."},
                        "offset": {"type": "integer", "description": "1-based line to start from. Default 1."},
                        "limit": {"type": "integer", "description": "Maximum lines to return. Default 500, cap 2000."}
                    }
                }),
                output_schema: None,
                output_example: Some(json!({
                    "path": "src/main.rs",
                    "content": "fn main() {\n    …\n}\n",
                    "offset": 1,
                    "linesReturned": 312,
                    "totalLines": 312,
                    "truncated": false,
                })),
                read_only: Some(true),
            },
            ToolDef {
                name: GREP.to_string(),
                description: "Search file contents in the project directory with a Rust \
                              regex (read-only; gitignored files and binaries are skipped). \
                              Returns matching lines with path and line number — follow up \
                              with workbench__read_file for surrounding context."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["pattern"],
                    "properties": {
                        "pattern": {"type": "string", "description": "Rust regex matched against each line."},
                        "path": {"type": "string", "description": "File or directory to search. Default: the project directory."},
                        "glob": {"type": "string", "description": "Only search matching files, e.g. *.rs (file name) or src/**/*.rs (relative path)."},
                        "case_insensitive": {"type": "boolean", "description": "Case-insensitive matching. Default false."},
                        "max_matches": {"type": "integer", "description": "Stop after this many matches. Default 100, cap 500."}
                    }
                }),
                output_schema: None,
                output_example: Some(json!({
                    "matches": [{"path": "crates/graph-core/src/tools.rs", "line": 42, "text": "pub struct ToolDef {"}],
                    "matchCount": 1,
                    "truncated": false,
                })),
                read_only: Some(true),
            },
            ToolDef {
                name: GLOB.to_string(),
                description: "Find files in the project directory by glob pattern, e.g. \
                              **/*.rs (read-only; gitignored files are skipped). Paths are \
                              relative to the project directory, sorted."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["pattern"],
                    "properties": {
                        "pattern": {"type": "string", "description": "Glob matched against project-relative paths, e.g. **/*.rs or docs/**/*.mdx."},
                        "dir": {"type": "string", "description": "Directory subtree to search. Default: the project directory."}
                    }
                }),
                output_schema: None,
                output_example: Some(json!({
                    "files": ["crates/graph-cli/src/main.rs"],
                    "count": 1,
                    "truncated": false,
                })),
                read_only: Some(true),
            },
        ];
        for def in &mut defs {
            def.description.push_str(super::tools::WORKBENCH_ONLY_NOTE);
        }
        Ok(defs)
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        match name {
            READ_FILE | GREP | GLOB => {}
            // Not ours: stay silent, or the composite registry's fallthrough
            // double-logs calls that another workbench__* registry owns.
            other => return Err(ToolError::Unknown(other.to_string())),
        }
        tracing::debug!(
            target: "workbench",
            "agent invoked {name}: {}",
            super::runner::truncate(&input.to_string(), 300)
        );
        let started = std::time::Instant::now();
        let outcome = match name {
            READ_FILE => Ok(read_file(&self.root, &input)),
            // Tree walks run on the blocking pool so a large repo never
            // stalls the TUI's runtime workers.
            GREP | GLOB => {
                let root = self.root.clone();
                let name = name.to_string();
                tokio::task::spawn_blocking(move || {
                    if name == GREP {
                        grep(&root, &input)
                    } else {
                        glob_find(&root, &input)
                    }
                })
                .await
                .map_err(|error| ToolError::Transport(error.to_string()))
            }
            other => Err(ToolError::Unknown(other.to_string())),
        };
        if let Ok(outcome) = &outcome {
            tracing::debug!(
                target: "workbench",
                "{name} finished in {:.1}s (is_error={}): {}",
                started.elapsed().as_secs_f64(),
                outcome.is_error,
                super::runner::truncate(&outcome.result.to_string(), 300)
            );
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn main() {}\n// TODO: alpha\n").unwrap();
        fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
        fs::write(dir.path().join("src/nested/deep.rs"), "// TODO: deep\n").unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(dir.path().join("ignored.rs"), "// TODO: ignored\n").unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/config"), "// TODO: git internals\n").unwrap();
        fs::create_dir_all(dir.path().join(".github")).unwrap();
        fs::write(dir.path().join(".github/ci.yml"), "# TODO: dotdir\n").unwrap();
        fs::write(dir.path().join("binary.bin"), b"\x00\x01\x02TODO").unwrap();
        dir
    }

    fn root(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().canonicalize().unwrap()
    }

    #[test]
    fn read_full_file() {
        let dir = scratch();
        let outcome = read_file(&root(&dir), &json!({"path": "a.rs"}));
        assert!(!outcome.is_error);
        assert_eq!(outcome.result["totalLines"], 2);
        assert_eq!(outcome.result["linesReturned"], 2);
        assert_eq!(outcome.result["truncated"], false);
        assert!(outcome.result["content"]
            .as_str()
            .unwrap()
            .contains("fn main"));
    }

    #[test]
    fn read_windows_and_truncates() {
        let dir = scratch();
        let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        fs::write(dir.path().join("long.txt"), body).unwrap();
        let outcome = read_file(
            &root(&dir),
            &json!({"path": "long.txt", "offset": 3, "limit": 4}),
        );
        assert!(!outcome.is_error);
        assert_eq!(outcome.result["offset"], 3);
        assert_eq!(outcome.result["linesReturned"], 4);
        assert_eq!(outcome.result["totalLines"], 10);
        assert_eq!(outcome.result["truncated"], true);
        assert!(outcome.result["content"]
            .as_str()
            .unwrap()
            .starts_with("line 3"));
    }

    #[test]
    fn read_missing_offset_binary_and_dir_errors() {
        let dir = scratch();
        let root = root(&dir);
        assert!(read_file(&root, &json!({"path": "nope.rs"})).is_error);
        assert!(read_file(&root, &json!({"path": "a.rs", "offset": 99})).is_error);
        assert!(read_file(&root, &json!({"path": "binary.bin"})).is_error);
        assert!(read_file(&root, &json!({"path": "src"})).is_error);
    }

    #[test]
    fn resolve_rejects_escapes() {
        let dir = scratch();
        let root = root(&dir);
        let escape = resolve(&root, "../");
        assert!(escape.unwrap_err().contains("escapes"));
        let absolute = resolve(&root, "/etc/hosts");
        assert!(absolute.unwrap_err().contains("escapes"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlink_escape() {
        let dir = scratch();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("link.txt"),
        )
        .unwrap();
        let error = resolve(&root(&dir), "link.txt").unwrap_err();
        assert!(error.contains("escapes"), "{error}");
    }

    #[test]
    fn grep_finds_and_excludes() {
        let dir = scratch();
        let outcome = grep(&root(&dir), &json!({"pattern": "TODO"}));
        assert!(!outcome.is_error);
        let matches = outcome.result["matches"].as_array().unwrap();
        let paths: Vec<&str> = matches
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"a.rs"));
        assert!(paths.contains(&"src/nested/deep.rs"));
        // Dotdirs are searchable, but .git, gitignored files, and binaries are not.
        assert!(paths.contains(&".github/ci.yml"));
        assert!(!paths.iter().any(|p| p.starts_with(".git/")));
        assert!(!paths.contains(&"ignored.rs"));
        assert!(!paths.contains(&"binary.bin"));
        let first = &matches[0];
        assert!(first["line"].as_u64().unwrap() >= 1);
        assert!(first["text"].as_str().unwrap().contains("TODO"));
    }

    #[test]
    fn grep_filters_and_truncates() {
        let dir = scratch();
        let root = root(&dir);
        let yml = grep(&root, &json!({"pattern": "TODO", "glob": "*.yml"}));
        let paths: Vec<&str> = yml.result["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec![".github/ci.yml"]);

        let nested = grep(&root, &json!({"pattern": "TODO", "glob": "src/**/*.rs"}));
        assert_eq!(nested.result["matchCount"], 1);

        let insensitive = grep(&root, &json!({"pattern": "todo", "case_insensitive": true}));
        assert!(insensitive.result["matchCount"].as_u64().unwrap() >= 2);

        let capped = grep(&root, &json!({"pattern": "TODO", "max_matches": 1}));
        assert_eq!(capped.result["matchCount"], 1);
        assert_eq!(capped.result["truncated"], true);

        assert!(grep(&root, &json!({"pattern": "("})).is_error);
        assert!(grep(&root, &json!({})).is_error);
    }

    #[test]
    fn grep_single_file_path() {
        let dir = scratch();
        let outcome = grep(&root(&dir), &json!({"pattern": "TODO", "path": "a.rs"}));
        assert_eq!(outcome.result["matchCount"], 1);
        assert_eq!(outcome.result["matches"][0]["path"], "a.rs");
        assert_eq!(outcome.result["matches"][0]["line"], 2);
    }

    #[test]
    fn glob_matches_and_excludes() {
        let dir = scratch();
        let root = root(&dir);
        let outcome = glob_find(&root, &json!({"pattern": "**/*.rs"}));
        assert!(!outcome.is_error);
        let files = outcome.result["files"].as_array().unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.as_str().unwrap()).collect();
        assert_eq!(names, vec!["a.rs", "src/lib.rs", "src/nested/deep.rs"]);

        let scoped = glob_find(&root, &json!({"pattern": "src/**/*.rs", "dir": "src"}));
        assert_eq!(scoped.result["count"], 2);

        assert!(glob_find(&root, &json!({})).is_error);
        assert!(glob_find(&root, &json!({"pattern": "*", "dir": "a.rs"})).is_error);
    }

    #[test]
    fn registry_defs_and_dispatch() {
        let dir = scratch();
        let tools = FsTools::new(dir.path().to_path_buf()).unwrap();
        let defs = futures::executor::block_on(tools.tools()).unwrap();
        assert_eq!(defs.len(), 3);
        assert!(defs.iter().all(|d| d.read_only == Some(true)));
        assert!(defs.iter().all(|d| d.name.starts_with("workbench__")));
        assert!(
            defs.iter().all(|d| d
                .description
                .ends_with(super::super::tools::WORKBENCH_ONLY_NOTE)),
            "fs tools must carry the not-plan-legal note"
        );
    }

    #[tokio::test]
    async fn invoke_dispatches_and_falls_through() {
        let dir = scratch();
        let tools = FsTools::new(dir.path().to_path_buf()).unwrap();
        let read = tools
            .invoke(READ_FILE, json!({"path": "a.rs"}))
            .await
            .unwrap();
        assert!(!read.is_error);
        let grep = tools
            .invoke(GREP, json!({"pattern": "TODO"}))
            .await
            .unwrap();
        assert!(grep.result["matchCount"].as_u64().unwrap() >= 2);
        let glob = tools
            .invoke(GLOB, json!({"pattern": "**/*.rs"}))
            .await
            .unwrap();
        assert_eq!(glob.result["count"], 3);
        let unknown = tools.invoke("workbench__nope", json!({})).await;
        assert!(matches!(unknown, Err(ToolError::Unknown(_))));
    }
}
