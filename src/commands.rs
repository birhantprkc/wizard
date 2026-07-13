//! Custom slash commands and `@file` references.
//!
//! Custom commands are markdown files in `~/.wizard/commands/` and
//! `<project>/.wizard/commands/` (project files shadow global ones on a name
//! collision). The file stem is the command name; an optional `---`-fenced
//! frontmatter block (the same convention as skills) may carry a
//! `description` shown in the TUI suggestion popup. The body is a prompt
//! template: `$ARGUMENTS` expands to everything typed after the command name
//! and `$1`..`$9` to the whitespace-split positional arguments (missing
//! positions expand to the empty string).
//!
//! `@path` tokens in user input expand to the referenced file's contents in a
//! fenced code block. Both the TUI (`App::submit`) and headless `-p` runs go
//! through the same [`preprocess`] pipeline, so a prompt behaves identically
//! on every surface.

use std::path::{Path, PathBuf};

use crate::config::Config;

/// One loaded custom command.
#[derive(Debug, Clone)]
pub struct CustomCommand {
    /// Command name (the file stem): `/name` invokes it.
    pub name: String,
    /// Frontmatter `description`, shown in the suggestion popup.
    pub description: Option<String>,
    /// Prompt template with `$ARGUMENTS` / `$1`..`$9` placeholders.
    pub template: String,
    /// File it was loaded from.
    pub path: PathBuf,
}

impl CustomCommand {
    /// Whether the template references any argument placeholder — drives the
    /// `[args]` hint and Enter-to-complete behavior in the TUI.
    pub fn expects_args(&self) -> bool {
        let bytes = self.template.as_bytes();
        self.template.match_indices('$').any(|(i, _)| {
            let rest = &bytes[i + 1..];
            rest.starts_with(b"ARGUMENTS")
                || rest.first().is_some_and(|b| (b'1'..=b'9').contains(b))
        })
    }
}

/// Load custom commands from the canonical roots: `~/.wizard/commands/`,
/// then `<project>/.wizard/commands/` (project shadows global).
pub fn load(project_root: &Path) -> Vec<CustomCommand> {
    let mut dirs = Vec::new();
    match Config::wizard_dir() {
        Ok(dir) => dirs.push(dir.join("commands")),
        Err(err) => tracing::warn!("could not resolve ~/.wizard for commands: {err}"),
    }
    dirs.push(project_root.join(".wizard").join("commands"));
    load_from_dirs(&dirs)
}

/// Load `*.md` commands from `dirs` in order; later directories shadow
/// earlier ones on a name collision. Missing directories are skipped;
/// unreadable files are logged and skipped. The result is sorted by name.
pub fn load_from_dirs(dirs: &[PathBuf]) -> Vec<CustomCommand> {
    let mut by_name: std::collections::BTreeMap<String, CustomCommand> =
        std::collections::BTreeMap::new();
    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                tracing::warn!("could not read {}: {err}", dir.display());
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let raw = match std::fs::read_to_string(&path) {
                Ok(raw) => raw,
                Err(err) => {
                    tracing::warn!("could not read {}: {err}", path.display());
                    continue;
                }
            };
            let (meta, body) = crate::skills::split_frontmatter(&raw);
            by_name.insert(
                name.to_string(),
                CustomCommand {
                    name: name.to_string(),
                    description: meta.description,
                    template: body,
                    path,
                },
            );
        }
    }
    by_name.into_values().collect()
}

/// Expand `$ARGUMENTS` and `$1`..`$9` in `template`. A single pass over the
/// template, so placeholder-like text inside the arguments themselves is
/// never re-expanded.
pub fn expand_template(template: &str, args: &str) -> String {
    let args = args.trim();
    let positional: Vec<&str> = args.split_whitespace().collect();
    let mut out = String::with_capacity(template.len() + args.len());
    let mut rest = template;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            out.push_str(args);
            rest = tail;
        } else if let Some(digit) = after.chars().next().filter(|c| ('1'..='9').contains(c)) {
            let index = digit as usize - '1' as usize;
            if let Some(arg) = positional.get(index) {
                out.push_str(arg);
            }
            rest = &after[1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// If `input` is `/name [args...]` for one of `commands`, expand its
/// template. `None` when the input is not a custom-command invocation.
pub fn expand_custom(input: &str, commands: &[CustomCommand]) -> Option<String> {
    let rest = input.trim().strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let command = commands.iter().find(|command| command.name == name)?;
    Some(expand_template(&command.template, args))
}

/// Byte cap applied to one `@file` expansion.
pub const MAX_FILE_REF_BYTES: usize = 50_000;

/// Extensions treated as images. These expand to a short placeholder in the
/// prompt text and are collected as attachment paths for vision models.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Result of expanding custom commands and `@file` / image references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preprocessed {
    /// Expanded prompt text (file contents inlined; images as `[image: name]`).
    pub text: String,
    /// Absolute paths of image files referenced via `@path` (and any other
    /// image attachments the caller may merge in before the agent turn).
    pub images: Vec<PathBuf>,
}

impl Preprocessed {
    /// Text-only result with no attachments.
    pub fn text_only(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }
}

/// Whether `path` looks like a supported image file (by extension).
pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.as_str()))
}

/// Expand `@path` tokens in `input` to fenced code blocks with the file's
/// contents (capped at [`MAX_FILE_REF_BYTES`], with a truncation note).
///
/// A token expands only when `@` starts a whitespace-delimited token and the
/// rest resolves to an existing file (relative to `project_root`, absolute,
/// or `~/`-prefixed). Everything else — `@@escaped` tokens, email-like
/// `user@host`, `@missing-paths` — passes through unchanged. Image files
/// expand to a short `[image: name]` placeholder and their absolute paths are
/// collected in [`Preprocessed::images`] for vision-capable providers.
pub fn expand_file_refs(input: &str, project_root: &Path) -> Preprocessed {
    let mut out = String::with_capacity(input.len());
    let mut images = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        // Copy leading whitespace verbatim, then take one token.
        let token_start = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        out.push_str(&rest[..token_start]);
        rest = &rest[token_start..];
        if rest.is_empty() {
            break;
        }
        let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..token_end];
        match expand_token(token, project_root) {
            Some(TokenExpansion::Text(expanded)) => out.push_str(&expanded),
            Some(TokenExpansion::Image { placeholder, path }) => {
                out.push_str(&placeholder);
                images.push(path);
            }
            None => out.push_str(token),
        }
        rest = &rest[token_end..];
    }
    Preprocessed {
        text: out,
        images,
    }
}

/// Result of expanding one `@` token.
enum TokenExpansion {
    Text(String),
    Image { placeholder: String, path: PathBuf },
}

/// Expand one whitespace-delimited token, or `None` to pass it through.
fn expand_token(token: &str, project_root: &Path) -> Option<TokenExpansion> {
    let path_part = token.strip_prefix('@')?;
    // `@@path` is the escape hatch and a lone `@` is not a reference.
    if path_part.is_empty() || path_part.starts_with('@') {
        return None;
    }
    let path = resolve(path_part, project_root);
    if !path.is_file() {
        return None;
    }
    if is_image_path(&path) {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path_part);
        let absolute = path
            .canonicalize()
            .unwrap_or_else(|_| path.clone());
        return Some(TokenExpansion::Image {
            placeholder: format!("[image: {name}]"),
            path: absolute,
        });
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        // Unreadable / non-UTF-8: leave the token for the model to act on.
        Err(_) => return None,
    };
    let (content, truncated) = cap_bytes(&raw, MAX_FILE_REF_BYTES);
    let fence = fence_for(content);
    let mut block = format!("{fence}{path_part}\n{content}");
    if !content.ends_with('\n') {
        block.push('\n');
    }
    if truncated {
        block.push_str("… [truncated at 50KB]\n");
    }
    block.push_str(&fence);
    Some(TokenExpansion::Text(block))
}

/// Resolve a `@`-reference against the project root, expanding a leading `~`.
fn resolve(path: &str, project_root: &Path) -> PathBuf {
    let expanded = shellexpand::tilde(path);
    let candidate = Path::new(expanded.as_ref());
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        project_root.join(candidate)
    }
}

/// Truncate to at most `max` bytes on a char boundary. Returns the slice and
/// whether anything was dropped.
fn cap_bytes(raw: &str, max: usize) -> (&str, bool) {
    if raw.len() <= max {
        return (raw, false);
    }
    let mut cut = max;
    while cut > 0 && !raw.is_char_boundary(cut) {
        cut -= 1;
    }
    (&raw[..cut], true)
}

/// A backtick fence one longer than the longest run inside `content`
/// (minimum three), so embedded fences cannot break the block.
fn fence_for(content: &str) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in content.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

/// The one shared preprocessing pipeline for user prompts: expand a custom
/// `/command` invocation (when `input` is one), then `@file` references.
/// Used by both the TUI submit path and headless `-p` runs.
pub fn preprocess(input: &str, commands: &[CustomCommand], project_root: &Path) -> Preprocessed {
    let expanded = expand_custom(input, commands).unwrap_or_else(|| input.to_string());
    expand_file_refs(&expanded, project_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), content).unwrap();
    }

    // --- loading ---

    #[test]
    fn loads_commands_from_md_files_with_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("commands");
        write(
            &dir,
            "review.md",
            "---\ndescription: review the diff\n---\nReview this: $ARGUMENTS",
        );
        write(&dir, "notes.txt", "not a command");
        let commands = load_from_dirs(&[dir]);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "review");
        assert_eq!(commands[0].description.as_deref(), Some("review the diff"));
        assert_eq!(commands[0].template, "Review this: $ARGUMENTS");
        assert!(commands[0].expects_args());
    }

    #[test]
    fn command_without_frontmatter_has_no_description() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("commands");
        write(&dir, "ship.md", "Commit and push everything.");
        let commands = load_from_dirs(&[dir]);
        assert_eq!(commands[0].name, "ship");
        assert_eq!(commands[0].description, None);
        assert!(!commands[0].expects_args());
    }

    #[test]
    fn project_commands_shadow_global_on_name_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global");
        let project = tmp.path().join("project");
        write(&global, "deploy.md", "global deploy");
        write(&global, "lint.md", "global lint");
        write(&project, "deploy.md", "project deploy");
        let commands = load_from_dirs(&[global, project]);
        assert_eq!(commands.len(), 2);
        let deploy = commands.iter().find(|c| c.name == "deploy").unwrap();
        assert_eq!(deploy.template, "project deploy");
        assert!(commands.iter().any(|c| c.name == "lint"));
    }

    #[test]
    fn missing_directories_load_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let commands = load_from_dirs(&[tmp.path().join("absent")]);
        assert!(commands.is_empty());
    }

    // --- template expansion ---

    #[test]
    fn arguments_placeholder_takes_everything_after_the_name() {
        assert_eq!(
            expand_template("Fix: $ARGUMENTS", "the login bug now"),
            "Fix: the login bug now"
        );
    }

    #[test]
    fn positional_placeholders_split_on_whitespace() {
        assert_eq!(
            expand_template("from $1 to $2 ($ARGUMENTS)", "main release"),
            "from main to release (main release)"
        );
    }

    #[test]
    fn missing_positionals_expand_to_empty() {
        assert_eq!(expand_template("a=$1 b=$2 c=$3", "only"), "a=only b= c=");
    }

    #[test]
    fn dollar_in_arguments_is_not_reexpanded() {
        assert_eq!(expand_template("run $1", "$2"), "run $2");
        assert_eq!(
            expand_template("say $ARGUMENTS", "$1 literal"),
            "say $1 literal"
        );
    }

    #[test]
    fn bare_dollar_and_unknown_placeholders_pass_through() {
        assert_eq!(
            expand_template("price $0 and $x end$", "y"),
            "price $0 and $x end$"
        );
    }

    #[test]
    fn expand_custom_matches_by_name() {
        let commands = vec![CustomCommand {
            name: "review".into(),
            description: None,
            template: "Review $ARGUMENTS carefully.".into(),
            path: PathBuf::new(),
        }];
        assert_eq!(
            expand_custom("/review src/app.rs", &commands).as_deref(),
            Some("Review src/app.rs carefully.")
        );
        assert_eq!(expand_custom("/other x", &commands), None);
        assert_eq!(expand_custom("not a command", &commands), None);
    }

    // --- @file references ---

    #[test]
    fn existing_file_expands_to_a_fenced_block() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.md"), "hello world\n").unwrap();
        let out = expand_file_refs("see @notes.md please", tmp.path());
        assert_eq!(out.text, "see ```notes.md\nhello world\n``` please");
        assert!(out.images.is_empty());
    }

    #[test]
    fn missing_file_token_passes_through() {
        let tmp = tempfile::tempdir().unwrap();
        let out = expand_file_refs("see @missing.md please", tmp.path());
        assert_eq!(out.text, "see @missing.md please");
        assert!(out.images.is_empty());
    }

    #[test]
    fn double_at_escapes_and_emails_pass_through() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("real.md"), "x").unwrap();
        let out = expand_file_refs("@@real.md and user@host.com and a lone @", tmp.path());
        assert_eq!(out.text, "@@real.md and user@host.com and a lone @");
    }

    #[test]
    fn oversized_file_is_capped_with_a_truncation_note() {
        let tmp = tempfile::tempdir().unwrap();
        let big = "x".repeat(MAX_FILE_REF_BYTES + 1000);
        std::fs::write(tmp.path().join("big.txt"), &big).unwrap();
        let out = expand_file_refs("@big.txt", tmp.path());
        assert!(out.text.contains("… [truncated at 50KB]"));
        assert!(out.text.len() < big.len() + 200);
    }

    #[test]
    fn tilde_paths_resolve_against_home() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let name = format!("wizard-atref-test-{}.txt", std::process::id());
        let path = home.join(&name);
        std::fs::write(&path, "tilde ok").unwrap();
        let out = expand_file_refs(&format!("@~/{name}"), Path::new("/nonexistent-root"));
        std::fs::remove_file(&path).unwrap();
        assert!(out.text.contains("tilde ok"), "got: {}", out.text);
    }

    #[test]
    fn absolute_paths_resolve_as_is() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("abs.txt");
        std::fs::write(&file, "absolute").unwrap();
        let input = format!("@{}", file.display());
        let out = expand_file_refs(&input, Path::new("/elsewhere"));
        assert!(out.text.contains("absolute"));
    }

    #[test]
    fn image_extensions_attach_path_and_placeholder() {
        let tmp = tempfile::tempdir().unwrap();
        let shot = tmp.path().join("shot.png");
        std::fs::write(&shot, [0x89u8, b'P', b'N', b'G']).unwrap();
        let out = expand_file_refs("look at @shot.png", tmp.path());
        assert_eq!(out.text, "look at [image: shot.png]");
        assert_eq!(out.images.len(), 1);
        assert_eq!(
            out.images[0],
            shot.canonicalize().unwrap_or(shot),
            "image path is absolute"
        );
    }

    #[test]
    fn directories_pass_through() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("subdir")).unwrap();
        let out = expand_file_refs("@subdir", tmp.path());
        assert_eq!(out.text, "@subdir");
        assert!(out.images.is_empty());
    }

    #[test]
    fn embedded_fences_get_a_longer_fence() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("doc.md"), "```rust\ncode\n```\n").unwrap();
        let out = expand_file_refs("@doc.md", tmp.path());
        assert!(out.text.starts_with("````doc.md\n"), "got: {}", out.text);
        assert!(out.text.ends_with("````"), "got: {}", out.text);
    }

    #[test]
    fn multiline_input_preserves_whitespace() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "F").unwrap();
        let out = expand_file_refs("line one\n  @f.txt\nline three", tmp.path());
        assert_eq!(out.text, "line one\n  ```f.txt\nF\n```\nline three");
    }

    // --- the shared pipeline ---

    #[test]
    fn preprocess_expands_commands_then_file_refs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ctx.txt"), "context").unwrap();
        let commands = vec![CustomCommand {
            name: "with-ctx".into(),
            description: None,
            template: "Use @ctx.txt for $ARGUMENTS".into(),
            path: PathBuf::new(),
        }];
        let out = preprocess("/with-ctx the task", &commands, tmp.path());
        assert!(out.text.contains("context"), "got: {}", out.text);
        assert!(out.text.ends_with("for the task"), "got: {}", out.text);
        assert!(out.images.is_empty());
    }

    #[test]
    fn preprocess_passes_plain_prompts_through() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            preprocess("just a prompt", &[], tmp.path()),
            Preprocessed::text_only("just a prompt")
        );
        assert_eq!(
            preprocess("/unknown cmd", &[], tmp.path()),
            Preprocessed::text_only("/unknown cmd")
        );
    }

    #[test]
    fn preprocess_collects_image_attachments() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.png"), b"png").unwrap();
        std::fs::write(tmp.path().join("b.webp"), b"webp").unwrap();
        let out = preprocess("compare @a.png and @b.webp", &[], tmp.path());
        assert!(out.text.contains("[image: a.png]"));
        assert!(out.text.contains("[image: b.webp]"));
        assert_eq!(out.images.len(), 2);
    }
}
