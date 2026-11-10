//! Config layer — schema, validation, defaults.
//!
//! Owned by T01 (#2, configuration schema and session model): YAML config
//! loading, Service/Interactive-Terminal session schemas, logging config,
//! per-session actionable validation, and app-data path resolution
//! ([`paths`]). One invalid session never hides unrelated valid sessions
//! (spec §13); no process is ever launched from here.

mod dto;
mod model;
mod paths;
mod save;
mod validate;

pub use dto::{
    ConfigFileStatusDto, ConfigReportDto, EffectiveLoggingDto, SessionConfigDto,
    SessionConfigErrorDto,
};
pub use model::{
    DisplayMode, EffectiveLogMode, EffectiveLogging, LifecycleOwner, LogMode, LogSource,
    LoggingConfig, RawConfigFile, RawSessionConfig, SessionConfig, SessionType,
};
pub use paths::{
    is_filesystem_safe_component, local_utc_offset_secs, run_file_name, run_log_filename,
    run_log_path, run_metadata_path, session_log_dir, AppPaths,
};
pub use save::{
    append_session, remove_saved_session, save_session, session_ids, update_saved_session,
    SaveError,
};
// Crate-visible, and only under `cargo test`: the release guards in
// `crate::release` compare the bundle's product name against this. The
// app-data layout is documented for users to read; the constant itself does
// not need to be public API for that, and outside the test build nothing in
// the crate reads it through this path.
#[cfg(test)]
pub(crate) use paths::APP_DIR_NAME;
pub use validate::{validate_entry, SessionConfigError};

use std::collections::HashSet;
use std::path::Path;

/// Outcome of loading a config document: the valid sessions plus the
/// per-session problems. Both lists are present whenever the file parsed;
/// only unreadable files or broken YAML produce a file-level error entry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadedConfig {
    /// Whether a file was read or no file exists yet.
    pub file_status: ConfigFileStatusDto,
    /// Validated sessions, in file order.
    pub sessions: Vec<SessionConfig>,
    /// Per-session (or file-level, `index == 0`) errors, in file order.
    pub errors: Vec<SessionConfigError>,
}

impl LoadedConfig {
    /// The report DTO exposed to the frontend (stable contract).
    pub fn to_dto(&self) -> ConfigReportDto {
        ConfigReportDto {
            file_status: self.file_status,
            config_path: None,
            sessions: self.sessions.iter().map(SessionConfigDto::from).collect(),
            errors: self
                .errors
                .iter()
                .cloned()
                .map(SessionConfigErrorDto::from)
                .collect(),
        }
    }
}

/// Parse and validate a config document.
///
/// - An empty/whitespace document loads as an empty session list (fresh
///   install with no config yet is not an error).
/// - Broken YAML yields a single error entry: file-level, except for the one
///   mistake whose real cause cannot be read off the parser's own words (see
///   [`quoted_windows_path_entry`]), which is attributed to the entry holding
///   it and told what to change.
/// - Each entry under `sessions:` is parsed and validated independently;
///   failures become per-entry errors while the remaining sessions load.
/// - Duplicate ids reject every entry after the first, keeping the
///   earliest definition valid and pointing the message at both.
pub fn load_from_str(text: &str) -> LoadedConfig {
    if text.trim().is_empty() {
        return LoadedConfig {
            file_status: ConfigFileStatusDto::Loaded,
            ..LoadedConfig::default()
        };
    }
    // A comment-only document deserializes as null; treat it like an
    // empty file rather than a parse error.
    let document: serde_yaml::Value = match serde_yaml::from_str(text) {
        Ok(document) => document,
        Err(err) => {
            let (index, message) = yaml_failure_message(text, &err);
            return LoadedConfig {
                file_status: ConfigFileStatusDto::Loaded,
                sessions: Vec::new(),
                errors: vec![SessionConfigError {
                    index,
                    session_id: None,
                    field: None,
                    message,
                }],
            };
        }
    };
    let root: Option<RawConfigFile> = match serde_yaml::from_value(document.clone()) {
        Ok(root) => root,
        Err(err) => {
            let field = root_deserialization_error_field(&document, &err);
            let message = match &field {
                Some(field) => {
                    format!("config file has an invalid field `{field}`: {err}")
                }
                None => format!("config file has an invalid structure: {err}"),
            };
            return LoadedConfig {
                file_status: ConfigFileStatusDto::Loaded,
                sessions: Vec::new(),
                errors: vec![SessionConfigError {
                    index: 0,
                    session_id: None,
                    field,
                    message,
                }],
            };
        }
    };

    let entries = root.and_then(|root| root.sessions).unwrap_or_default();
    let mut sessions = Vec::new();
    let mut errors = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    for (offset, entry) in entries.iter().enumerate() {
        let index = offset + 1;
        let raw: RawSessionConfig = match serde_yaml::from_value(entry.clone()) {
            Ok(raw) => raw,
            Err(err) => {
                let field = deserialization_error_field(entry, &err);
                let message = match &field {
                    Some(field) => {
                        format!("session entry {index} has an invalid field `{field}`: {err}")
                    }
                    None => format!("session entry {index} is invalid: {err}"),
                };
                errors.push(SessionConfigError {
                    index,
                    session_id: None,
                    field,
                    message,
                });
                continue;
            }
        };
        let id = raw.id.trim().to_owned();
        if !id.is_empty() && !seen_ids.insert(id.clone()) {
            errors.push(SessionConfigError {
                index,
                session_id: Some(id.clone()),
                field: Some("id".to_owned()),
                message: format!(
                    "duplicate session id `{id}` — first defined earlier in the file; \
                     session ids must be unique"
                ),
            });
            continue;
        }
        match validate_entry(index, &raw) {
            Ok(config) => sessions.push(config),
            Err(err) => errors.push(err),
        }
    }

    LoadedConfig {
        file_status: ConfigFileStatusDto::Loaded,
        sessions,
        errors,
    }
}

fn root_deserialization_error_field(
    document: &serde_yaml::Value,
    error: &serde_yaml::Error,
) -> Option<String> {
    let message = error.to_string();
    if let Some(field) = quoted_error_field(&message, "unknown field") {
        return Some(field);
    }
    if let Some(field) = quoted_error_field(&message, "missing field") {
        return Some(field);
    }
    if let Some(sessions) = mapping_value(document, "sessions") {
        if serde_yaml::from_value::<Option<Vec<serde_yaml::Value>>>(sessions.clone()).is_err() {
            return Some("sessions".to_owned());
        }
    }
    Some("config".to_owned())
}

/// Resolve a serde error to the config key that needs attention. Serde's
/// `from_value` error does not retain the nested key path, so recover explicit
/// missing/unknown keys from its message and otherwise check the known schema
/// in declaration order to identify a value with the wrong shape.
fn deserialization_error_field(
    entry: &serde_yaml::Value,
    error: &serde_yaml::Error,
) -> Option<String> {
    let message = error.to_string();
    if let Some(field) = quoted_error_field(&message, "missing field") {
        return Some(field);
    }
    if let Some(field) = quoted_error_field(&message, "unknown field") {
        return unknown_session_field_path(entry).or(Some(field));
    }

    let fields = entry.as_mapping()?;
    for (name, expected) in [
        ("id", FieldShape::String),
        ("name", FieldShape::String),
        ("type", FieldShape::String),
        ("cwd", FieldShape::OptionalString),
        ("command", FieldShape::OptionalString),
        ("url", FieldShape::OptionalString),
        ("port", FieldShape::OptionalPort),
        ("purpose", FieldShape::OptionalString),
        ("close_impact", FieldShape::OptionalString),
        ("shell", FieldShape::OptionalString),
        ("initial_command", FieldShape::OptionalString),
        ("logging", FieldShape::OptionalMapping),
    ] {
        let Some(value) = fields.get(serde_yaml::Value::String(name.to_owned())) else {
            continue;
        };
        if !expected.matches(value) {
            return Some(name.to_owned());
        }
    }

    let logging = mapping_value(entry, "logging")?.as_mapping()?;
    for (name, expected) in [
        ("mode", FieldShape::OptionalLogMode),
        ("source", FieldShape::OptionalLogSource),
        ("path", FieldShape::OptionalString),
    ] {
        let Some(value) = logging.get(serde_yaml::Value::String(name.to_owned())) else {
            continue;
        };
        if !expected.matches(value) {
            return Some(format!("logging.{name}"));
        }
    }
    None
}

/// Follow the same mapping order serde uses so identically named unknown
/// fields at the entry and inside `logging` are attributed to the first one it
/// would reject.
fn unknown_session_field_path(entry: &serde_yaml::Value) -> Option<String> {
    let fields = entry.as_mapping()?;
    for (key, value) in fields {
        let name = key.as_str()?;
        if name == "logging" {
            if let Some(logging) = value.as_mapping() {
                for (logging_key, _) in logging {
                    let logging_name = logging_key.as_str()?;
                    if !matches!(logging_name, "mode" | "source" | "path") {
                        return Some(format!("logging.{logging_name}"));
                    }
                }
            }
        } else if !matches!(
            name,
            "id" | "name"
                | "type"
                | "cwd"
                | "command"
                | "url"
                | "port"
                | "purpose"
                | "close_impact"
                | "shell"
                | "initial_command"
        ) {
            return Some(name.to_owned());
        }
    }
    None
}

fn mapping_value<'a>(value: &'a serde_yaml::Value, key: &str) -> Option<&'a serde_yaml::Value> {
    value
        .as_mapping()?
        .get(serde_yaml::Value::String(key.to_owned()))
}

fn quoted_error_field(message: &str, marker: &str) -> Option<String> {
    let value = message
        .get(message.find(marker)? + marker.len()..)?
        .trim_start();
    let quote = value.chars().next()?;
    if !matches!(quote, '`' | '\'') {
        return None;
    }
    let value = &value[quote.len_utf8()..];
    let end = value.find(quote)?;
    Some(value[..end].to_owned())
}

/// The entry number and message for a document that failed to parse.
///
/// Normally the failure is the file's: the document is not YAML, and the
/// parser's own words (`<err>`) say what is wrong with it. One mistake is the
/// exception, because its message names the mechanism rather than the thing
/// the user wrote: a Windows path inside a `"double-quoted"` scalar. The
/// config file is hand-written and Windows paths are the normal case here
/// (D-010), so that failure is attributed to the entry holding it and told
/// what to change — see [`QUOTED_WINDOWS_PATH_FIX`].
fn yaml_failure_message(text: &str, error: &serde_yaml::Error) -> (usize, String) {
    let Some(entry) = quoted_windows_path_entry(text, error) else {
        return (0, format!("config file is not valid YAML: {error}"));
    };
    let detail = format!("{error} — {QUOTED_WINDOWS_PATH_FIX}");
    match entry {
        Some(index) => (
            index,
            format!("session entry {index} is not valid YAML: {detail}"),
        ),
        None => (0, format!("config file is not valid YAML: {detail}")),
    }
}

/// The `sessions:` entry that holds a quoted Windows path, when the parse
/// error is that mistake.
///
/// `None` means the failure is not this one — and so is left exactly as the
/// parser reported it. `Some(None)` means it is this one but the entry number
/// could not be read off the text, which leaves the error file-level rather
/// than pointing at a guessed entry.
fn quoted_windows_path_entry(text: &str, error: &serde_yaml::Error) -> Option<Option<usize>> {
    let location = error.location()?;
    let line = text.lines().nth(location.line().checked_sub(1)?)?;
    if !has_quoted_invalid_escape(line) {
        return None;
    }
    Some(entry_at(text, location.index()))
}

/// The fix for a path that the scanner reads as escape sequences.
///
/// Both spellings are named because both work and neither is guessable from
/// the parser's message: single quotes are the literal style YAML has for
/// exactly this, and forward slashes are accepted by Windows itself.
const QUOTED_WINDOWS_PATH_FIX: &str = "a backslash inside a \"double-quoted\" YAML scalar starts \
     an escape sequence, so the path is not read as written — write it in single quotes \
     (`command: 'C:\\Tools\\app.exe'`) or with forward slashes (`command: C:/Tools/app.exe`)";

/// Whether a line writes a path the scanner will read as escapes: a backslash
/// inside a double-quoted scalar that YAML does not define as an escape
/// sequence.
///
/// Checking the source rather than the parser's wording keeps this working
/// for both shapes of the failure, which say different things: an undefined
/// escape (`"C:\Tools\app.exe"` → *found unknown escape character*) and a
/// `\U` that is not followed by the eight hex digits it requires
/// (`"C:\Users\me"` → *did not find expected hexadecimal number*).
fn has_quoted_invalid_escape(line: &str) -> bool {
    let mut in_double = false;
    let mut in_single = false;
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        match ch {
            // A quote only opens or closes its own style; backslashes are
            // literal in single quotes, which is the fix this looks for.
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '\\' if in_double => {
                let Some(escape) = chars.next() else {
                    return true;
                };
                if !is_yaml_escape(escape, &mut chars) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// YAML's double-quoted escape sequences (YAML 1.2 §5.7): the named ones,
/// plus `\x`, `\u` and `\U` with their hex digits.
fn is_yaml_escape(first: char, rest: &mut std::str::Chars<'_>) -> bool {
    match first {
        '0' | 'a' | 'b' | 't' | 'n' | 'v' | 'f' | 'r' | 'e' | ' ' | '"' | '/' | '\\' | 'N'
        | '_' | 'L' | 'P' => true,
        'x' => hex_digits(rest, 2),
        'u' => hex_digits(rest, 4),
        'U' => hex_digits(rest, 8),
        _ => false,
    }
}

fn hex_digits(rest: &mut std::str::Chars<'_>, count: usize) -> bool {
    for _ in 0..count {
        match rest.next() {
            Some(ch) if ch.is_ascii_hexdigit() => {}
            _ => return false,
        }
    }
    true
}

/// The 1-based number of the `sessions:` list item that contains the byte
/// offset `offset`.
///
/// It reads the text rather than a parsed value, because there is no parsed
/// value: the document is what failed. The items are the `-` lines carrying
/// the indentation of the list's first one, so a list nested inside an entry
/// is not mistaken for a sibling. `None` when no item starts before the
/// offset or the root key is not there — the caller then says "the file"
/// rather than naming an entry that may not be the one at fault.
fn entry_at(text: &str, offset: usize) -> Option<usize> {
    let mut key_seen = false;
    let mut item_indent = None;
    let mut entry = 0usize;
    let mut start = 0usize;
    for line in text.split('\n') {
        let trimmed = line.trim();
        if !key_seen {
            key_seen = trimmed == "sessions:" && !line.starts_with(char::is_whitespace);
        } else if trimmed == "-" || trimmed.starts_with("- ") {
            let indent = line.len() - line.trim_start().len();
            let expected = *item_indent.get_or_insert(indent);
            if indent == expected {
                if start > offset {
                    break;
                }
                entry += 1;
            }
        }
        start += line.len() + 1;
    }
    (entry > 0).then_some(entry)
}

#[derive(Clone, Copy)]
enum FieldShape {
    String,
    OptionalString,
    OptionalPort,
    OptionalMapping,
    OptionalLogMode,
    OptionalLogSource,
}

impl FieldShape {
    fn matches(self, value: &serde_yaml::Value) -> bool {
        use serde_yaml::Value;
        match self {
            FieldShape::String => matches!(value, Value::String(_)),
            FieldShape::OptionalString => matches!(value, Value::Null | Value::String(_)),
            FieldShape::OptionalPort => {
                matches!(value, Value::Null)
                    || value.as_u64().is_some_and(|port| port <= u16::MAX as u64)
            }
            FieldShape::OptionalMapping => matches!(value, Value::Null | Value::Mapping(_)),
            FieldShape::OptionalLogMode => {
                matches!(value, Value::Null)
                    || matches!(
                        value.as_str(),
                        Some("off" | "always" | "on_error" | "manual" | "auto")
                    )
            }
            FieldShape::OptionalLogSource => {
                matches!(value, Value::Null)
                    || matches!(value.as_str(), Some("none" | "captured" | "external"))
            }
        }
    }
}

/// Load the config file at `path`.
///
/// A missing file is a fresh install, not an error: it loads as an empty
/// session list so the app can start and offer to create one. Read errors
/// (permissions and friends) are returned as [`std::io::Error`].
pub fn load_from_file(path: &Path) -> std::io::Result<LoadedConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(load_from_str(&text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(LoadedConfig::default()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Unique existing directory for `cwd` validation tests; created under
    /// the system temp dir and best-effort removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let unique = format!(
                "lch-t01-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock after epoch")
                    .as_nanos()
            );
            let path = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn session_yaml(id: &str, session_type: &str, extra: &str) -> String {
        format!("id: {id}\nname: Session {id}\ntype: {session_type}\n{extra}")
    }

    /// A `sessions:` document whose items are `entries`.
    ///
    /// The `- ` marker already places an item's first key at column 5, so only
    /// the continuation lines need the four-space indent that lines the rest of
    /// the item up under it.
    fn config(entries: &[String]) -> String {
        let mut out = String::from("sessions:\n");
        for entry in entries {
            let mut lines = entry.lines();
            if let Some(first) = lines.next() {
                out.push_str("  - ");
                out.push_str(first);
                out.push('\n');
            }
            for line in lines {
                out.push_str("    ");
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    /// Indent the continuation lines of a YAML fragment by two spaces, so every
    /// key of the fragment lines up under its first one.
    fn indent_continuation(fragment: &str) -> String {
        fragment.replace('\n', "\n  ")
    }

    #[test]
    fn empty_document_is_an_empty_session_list() {
        for text in ["", "   \n", "# only a comment\n"] {
            let loaded = load_from_str(text);
            assert!(loaded.sessions.is_empty());
            assert!(loaded.errors.is_empty(), "unexpected errors for {text:?}");
        }
    }

    #[test]
    fn broken_yaml_reports_one_file_level_error() {
        let loaded = load_from_str("sessions: [ uh oh");
        assert!(loaded.sessions.is_empty());
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].index, 0);
        assert_eq!(loaded.errors[0].field, None);
        assert!(loaded.errors[0].message.contains("YAML"));
    }

    /// A backslash inside a `"double-quoted"` scalar is an escape sequence,
    /// so the scanner rejects the entry with a sentence about escapes and
    /// never mentions the path. The user wrote a Windows path — the normal
    /// case in a hand-written config (D-010) — so the error has to say which
    /// entry it is and what to write instead.
    #[test]
    fn a_quoted_windows_path_names_its_entry_and_the_fix() {
        // The two fields a Windows user writes a path into, and the two
        // sentences the scanner produces for one: an undefined escape, and a
        // `\U` that is not the eight hex digits it requires.
        for path in ["C:\\Tools\\app.exe", "C:\\Users\\me\\app.exe"] {
            for field in ["command", "cwd"] {
                let first = session_yaml("other", "service", "command: run");
                let second = session_yaml("app", "service", &format!("{field}: \"{path}\""));
                let loaded = load_from_str(&config(&[first, second]));

                assert!(
                    loaded.sessions.is_empty(),
                    "the document does not parse at all"
                );
                assert_eq!(loaded.errors.len(), 1);
                let error = &loaded.errors[0];
                assert_eq!(
                    error.index, 2,
                    "the error belongs to the entry holding the path"
                );
                assert!(error.message.contains("entry 2"), "{:?}", error.message);
                assert!(
                    error.message.contains("double-quoted"),
                    "{:?}",
                    error.message
                );
                assert!(
                    error.message.contains("single quotes"),
                    "{:?}",
                    error.message
                );
                assert!(
                    error.message.contains("forward slashes"),
                    "{:?}",
                    error.message
                );
            }
        }
    }

    /// Both spellings the message names are what actually loads, so the
    /// advice cannot drift away from the loader.
    #[test]
    fn the_fixes_the_message_names_load() {
        let dir = TempDir::new("path-fixes");
        let slashes = dir.0.display().to_string().replace('\\', "/");
        let quoted = session_yaml(
            "quoted",
            "service",
            &format!("command: 'C:\\Tools\\app.exe'\ncwd: {slashes}"),
        );
        let forward = session_yaml(
            "forward",
            "service",
            &format!("command: C:/Tools/app.exe\ncwd: {slashes}"),
        );
        let loaded = load_from_str(&config(&[quoted, forward]));

        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(
            loaded.sessions[0].command.as_deref(),
            Some("C:\\Tools\\app.exe"),
            "a single-quoted path is taken literally"
        );
        assert_eq!(
            loaded.sessions[1].command.as_deref(),
            Some("C:/Tools/app.exe")
        );
    }

    /// Every other way a document fails to parse keeps the message it had:
    /// the file is what is wrong, and the fix is not a quoted path.
    #[test]
    fn a_yaml_error_that_is_not_a_quoted_windows_path_keeps_its_message() {
        // A stray flow sequence in the second entry.
        let broken = "id: broken\nname: Broken\ntype: service\ncommand: [oops".to_owned();
        let other = session_yaml("ok", "service", "command: run");
        let loaded = load_from_str(&config(&[other.clone(), broken]));
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].index, 0);
        assert!(loaded.errors[0]
            .message
            .contains("config file is not valid YAML"));
        assert!(!loaded.errors[0].message.contains("single quotes"));

        // A backslash that *is* a valid escape must not be blamed for an
        // error that is somewhere else on the page.
        let escaped = "id: esc\nname: Esc\ntype: service\n\
                       command: \"C:\\\\Tools\\\\app.exe\"\nport: [oops"
            .to_owned();
        let loaded = load_from_str(&config(&[other, escaped]));
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].index, 0);
        assert!(loaded.errors[0]
            .message
            .contains("config file is not valid YAML"));
        assert!(!loaded.errors[0].message.contains("single quotes"));
    }

    #[test]
    fn invalid_root_shape_names_the_config_field_to_repair() {
        let loaded = load_from_str("sessions: nope\n");

        assert!(loaded.sessions.is_empty());
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].field.as_deref(), Some("sessions"));
        assert!(loaded.errors[0]
            .message
            .contains("invalid field `sessions`"));
        assert!(!loaded.errors[0].message.contains("not valid YAML"));
    }

    #[test]
    fn unknown_root_field_is_reported_as_a_config_field_error() {
        let loaded = load_from_str("sessions: []\nunknown: true\n");

        assert!(loaded.sessions.is_empty());
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].field.as_deref(), Some("unknown"));
        assert!(loaded.errors[0].message.contains("unknown"));
        assert!(!loaded.errors[0].message.contains("not valid YAML"));
    }

    #[test]
    fn duplicate_ids_reject_later_entries_and_keep_the_first() {
        let first = session_yaml("api", "service", "command: run api");
        let second = session_yaml("api", "service", "command: run api again");
        let loaded = load_from_str(&config(&[first, second]));
        assert_eq!(loaded.sessions.len(), 1, "first definition stays valid");
        assert_eq!(loaded.sessions[0].command.as_deref(), Some("run api"));
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].session_id.as_deref(), Some("api"));
        assert!(loaded.errors[0]
            .message
            .contains("duplicate session id `api`"));
    }

    #[test]
    fn one_invalid_session_does_not_hide_valid_ones() {
        let good_dir = TempDir::new("good");
        let good = session_yaml(
            "good",
            "service",
            &format!("command: node server.js\ncwd: {}", good_dir.0.display()),
        );
        let bad_type = session_yaml("bad", "daemon", "command: whatever");
        let loaded = load_from_str(&config(&[good, bad_type]));
        assert_eq!(loaded.sessions.len(), 1, "valid session survives");
        assert_eq!(loaded.sessions[0].id, "good");
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].session_id.as_deref(), Some("bad"));
        assert_eq!(loaded.errors[0].field.as_deref(), Some("type"));
        assert!(loaded.errors[0]
            .message
            .contains("unsupported type `daemon`"));
    }

    #[test]
    fn service_without_command_is_actionable() {
        let entry = session_yaml("nosvc", "service", "port: 8000");
        let loaded = load_from_str(&config(&[entry]));
        assert!(loaded.sessions.is_empty());
        assert_eq!(loaded.errors[0].field.as_deref(), Some("command"));
        assert!(loaded.errors[0].message.contains("require `command`"));
    }

    #[test]
    fn terminal_without_shell_is_actionable() {
        let entry = session_yaml("noterm", "terminal", "initial_command: ls");
        let loaded = load_from_str(&config(&[entry]));
        assert!(loaded.sessions.is_empty());
        assert_eq!(loaded.errors[0].field.as_deref(), Some("shell"));
        assert!(loaded.errors[0].message.contains("require `shell`"));
    }

    #[test]
    fn port_zero_is_rejected_and_out_of_range_ports_fail_per_session() {
        let zero = session_yaml("svc0", "service", "command: run\nport: 0");
        let loaded = load_from_str(&config(&[zero]));
        assert_eq!(loaded.errors[0].field.as_deref(), Some("port"));
        assert!(loaded.errors[0].message.contains("1-65535"));

        let huge = session_yaml("svcbig", "service", "command: run\nport: 70000");
        let other = session_yaml("other", "service", "command: run");
        let loaded = load_from_str(&config(&[huge, other]));
        assert_eq!(
            loaded.sessions.len(),
            1,
            "out-of-range port must not hide the sibling session"
        );
        assert_eq!(loaded.sessions[0].id, "other");
        assert_eq!(loaded.errors.len(), 1);
        assert!(loaded.errors[0].message.contains("70000"));
    }

    #[test]
    fn malformed_and_non_http_urls_are_rejected() {
        for bad in ["not a url", "ftp://127.0.0.1/x"] {
            let entry = session_yaml("urlsvc", "service", &format!("command: run\nurl: {bad}"));
            let loaded = load_from_str(&config(&[entry]));
            assert_eq!(loaded.errors.len(), 1, "url `{bad}` must be rejected");
            assert_eq!(loaded.errors[0].field.as_deref(), Some("url"));
        }
        let good = session_yaml(
            "urlsvc",
            "service",
            "command: run\nurl: https://127.0.0.1:8443/x",
        );
        let loaded = load_from_str(&config(&[good]));
        assert!(loaded.errors.is_empty());
        assert!(loaded.sessions[0]
            .url
            .as_deref()
            .expect("url kept")
            .starts_with("https://"));
    }

    #[test]
    fn missing_working_directory_is_rejected_with_the_path() {
        let entry = session_yaml(
            "ghost",
            "service",
            "command: run\ncwd: Z:/definitely/not/here/lch-t01",
        );
        let loaded = load_from_str(&config(&[entry]));
        assert!(loaded.sessions.is_empty());
        assert_eq!(loaded.errors[0].field.as_deref(), Some("cwd"));
        assert!(loaded.errors[0]
            .message
            .contains("Z:/definitely/not/here/lch-t01"));
    }

    #[test]
    fn cross_type_fields_are_rejected() {
        let service_with_shell =
            session_yaml("mixed", "service", "command: run\nshell: powershell");
        let loaded = load_from_str(&config(&[service_with_shell]));
        assert_eq!(loaded.errors[0].field.as_deref(), Some("shell"));
        assert!(loaded.errors[0].message.contains("type: terminal"));

        let terminal_with_command =
            session_yaml("mixed2", "terminal", "shell: powershell\ncommand: run");
        let loaded = load_from_str(&config(&[terminal_with_command]));
        assert_eq!(loaded.errors[0].field.as_deref(), Some("command"));
        assert!(loaded.errors[0].message.contains("type: service"));

        // `purpose` and `close_impact` are *not* cross-type: they describe any
        // session in words and carry no runtime meaning (D-027), so a terminal
        // keeping the rest of the service-only fields rejected is the whole
        // rule.
        let terminal_with_port =
            session_yaml("mixed3", "terminal", "shell: powershell\nport: 8000");
        let loaded = load_from_str(&config(&[terminal_with_port]));
        assert_eq!(loaded.errors[0].field.as_deref(), Some("port"));
        assert!(loaded.errors[0].message.contains("type: service"));
    }

    #[test]
    fn a_terminal_may_carry_purpose_and_close_impact() {
        let entry = session_yaml(
            "term",
            "terminal",
            "shell: powershell\npurpose: 日常交互终端，跑一次性命令与 REPL。\n\
             close_impact: 仅结束本终端；不会停止其它受管服务。",
        );
        let loaded = load_from_str(&config(&[entry]));
        assert!(
            loaded.errors.is_empty(),
            "unexpected errors: {:?}",
            loaded.errors
        );
        let term = &loaded.sessions[0];
        assert_eq!(
            term.purpose.as_deref(),
            Some("日常交互终端，跑一次性命令与 REPL。")
        );
        assert_eq!(
            term.close_impact.as_deref(),
            Some("仅结束本终端；不会停止其它受管服务。")
        );
    }

    #[test]
    fn a_terminal_that_omits_purpose_or_close_impact_keeps_them_absent() {
        let entry = session_yaml("term", "terminal", "shell: powershell");
        let loaded = load_from_str(&config(&[entry]));
        assert!(
            loaded.errors.is_empty(),
            "unexpected errors: {:?}",
            loaded.errors
        );
        assert_eq!(loaded.sessions[0].purpose, None);
        assert_eq!(loaded.sessions[0].close_impact, None);
    }

    #[test]
    fn an_empty_terminal_purpose_is_actionable() {
        let entry = session_yaml("term", "terminal", "shell: powershell\npurpose: '   '");
        let loaded = load_from_str(&config(&[entry]));
        assert!(loaded.sessions.is_empty());
        assert_eq!(loaded.errors[0].field.as_deref(), Some("purpose"));
    }

    #[test]
    fn unknown_fields_are_per_session_errors_not_file_errors() {
        let typo = session_yaml("typo", "service", "command: run\nprot: 8000");
        let other = session_yaml("other", "service", "command: run");
        let loaded = load_from_str(&config(&[typo, other]));
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].id, "other");
        assert_eq!(loaded.errors.len(), 1);
        assert!(loaded.errors[0].message.contains("unknown field `prot`"));
    }

    #[test]
    fn wrong_field_types_identify_the_field_to_repair() {
        let entry = session_yaml("bad-command", "service", "command: 42");
        let loaded = load_from_str(&config(&[entry]));

        assert_eq!(loaded.sessions.len(), 0);
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].field.as_deref(), Some("command"));
        assert!(loaded.errors[0].message.contains("command"));
    }

    #[test]
    fn nested_logging_errors_identify_the_full_field_path() {
        let entry = session_yaml(
            "bad-logging",
            "service",
            "command: run\nlogging:\n  mode: verbose",
        );
        let loaded = load_from_str(&config(&[entry]));

        assert_eq!(loaded.sessions.len(), 0);
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].field.as_deref(), Some("logging.mode"));
    }

    #[test]
    fn top_level_unknown_field_is_not_attributed_to_a_valid_logging_field() {
        let entry = session_yaml(
            "bad-source",
            "service",
            "command: run\nsource: typo\nlogging:\n  source: captured",
        );
        let loaded = load_from_str(&config(&[entry]));

        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].field.as_deref(), Some("source"));
    }

    #[test]
    fn duplicate_unknown_names_are_attributed_to_the_first_invalid_mapping() {
        let top_level_first = session_yaml(
            "bad-source",
            "service",
            "command: run\nsource_typo: top\nlogging:\n  source_typo: nested",
        );
        let nested_first = session_yaml(
            "bad-logging",
            "service",
            "command: run\nlogging:\n  source_typo: nested\nsource_typo: top",
        );

        let top_level_error = load_from_str(&config(&[top_level_first]));
        let nested_error = load_from_str(&config(&[nested_first]));

        assert_eq!(
            top_level_error.errors[0].field.as_deref(),
            Some("source_typo")
        );
        assert_eq!(
            nested_error.errors[0].field.as_deref(),
            Some("logging.source_typo")
        );
    }

    #[test]
    fn unsafe_session_ids_are_rejected() {
        for bad in ["has space", "../escape", "\"\"", "-leading-dash"] {
            let entry = session_yaml(bad, "service", "command: run");
            let loaded = load_from_str(&config(&[entry]));
            assert_eq!(
                loaded.errors.len(),
                1,
                "id {bad} should produce exactly one error, got {:?}",
                loaded.errors
            );
        }
    }

    #[test]
    fn logging_defaults_and_auto_resolution() {
        // Terminal without a logging block: no persistence (LOGGING.md §1.2).
        let term = session_yaml("term", "terminal", "shell: powershell");
        let loaded = load_from_str(&config(&[term]));
        assert!(loaded.errors.is_empty());
        let logging = &loaded.sessions[0].logging;
        assert_eq!(logging.mode, EffectiveLogMode::Off);
        assert_eq!(logging.source, LogSource::None);

        // Service without a logging block: on_error with captured output.
        let svc = session_yaml("svc", "service", "command: run");
        let loaded = load_from_str(&config(&[svc]));
        let logging = &loaded.sessions[0].logging;
        assert_eq!(logging.mode, EffectiveLogMode::OnError);
        assert_eq!(logging.source, LogSource::Captured);

        // Service referencing an application-owned log: link, don't capture.
        let ext = session_yaml(
            "ext",
            "service",
            "command: run\nlogging:\n  mode: auto\n  source: external\n  path: ./data/app.log",
        );
        let loaded = load_from_str(&config(&[ext]));
        assert!(loaded.errors.is_empty());
        let logging = &loaded.sessions[0].logging;
        assert_eq!(logging.mode, EffectiveLogMode::Always);
        assert_eq!(logging.source, LogSource::External);
        assert_eq!(logging.external_path.as_deref(), Some("./data/app.log"));
    }

    #[test]
    fn contradictory_logging_combinations_are_rejected() {
        let cases: &[(&str, &str)] = &[
            // captured output that is never persisted
            ("mode: off\nsource: captured", "contradict"),
            // external link that persists nothing
            ("mode: off\nsource: external\npath: a.log", "contradict"),
            // external link that writes Hub-captured output
            (
                "mode: on_error\nsource: external\npath: a.log",
                "contradict",
            ),
            // persisting mode without a source
            ("mode: always", "needs `source: captured`"),
            // persistence mode with nothing to persist
            ("mode: always\nsource: none", "nothing to persist"),
            // external without a path
            ("mode: always\nsource: external", "requires `logging.path`"),
            // path without external source
            (
                "mode: always\nsource: captured\npath: a.log",
                "source: external",
            ),
        ];
        for (block, expected) in cases {
            let entry = session_yaml(
                "badlog",
                "service",
                &format!("command: run\nlogging:\n  {}", indent_continuation(block)),
            );
            let loaded = load_from_str(&config(&[entry]));
            assert_eq!(
                loaded.errors.len(),
                1,
                "combination `{block}` should produce exactly one error"
            );
            assert!(
                loaded.errors[0].message.contains(expected),
                "message {:?} should mention {expected} for `{block}`",
                loaded.errors[0].message
            );
        }
    }

    #[test]
    fn valid_logging_combinations_load() {
        let cases = [
            "mode: always\nsource: captured",
            "mode: on_error\nsource: captured",
            "mode: manual\nsource: captured",
            "mode: off\nsource: none",
            "mode: always\nsource: external\npath: a.log",
        ];
        for block in cases {
            let entry = session_yaml(
                "oklog",
                "service",
                &format!("command: run\nlogging:\n  {}", indent_continuation(block)),
            );
            let loaded = load_from_str(&config(&[entry]));
            assert!(
                loaded.errors.is_empty(),
                "combination `{block}` should be valid, got {:?}",
                loaded.errors
            );
        }
    }

    #[test]
    fn terminal_cannot_force_captured_auto_logging() {
        let entry = session_yaml(
            "termlog",
            "terminal",
            "shell: powershell\nlogging:\n  mode: auto\n  source: captured",
        );
        let loaded = load_from_str(&config(&[entry]));
        assert_eq!(loaded.errors.len(), 1);
        assert!(loaded.errors[0].message.contains("terminal"));
    }

    /// An entry that says nothing about #66's two dimensions is the entry the
    /// Hub has always hosted and ended — the behaviour every configuration
    /// written before them keeps (spec #59 decision 8).
    #[test]
    fn an_entry_without_display_or_lifecycle_is_the_hub_internal_managed_one() {
        let entry = session_yaml("svc", "service", "command: run");
        let loaded = load_from_str(&config(&[entry]));

        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        let session = &loaded.sessions[0];
        assert_eq!(session.display, DisplayMode::Internal);
        assert_eq!(session.lifecycle, LifecycleOwner::Managed);
        assert!(!session.is_window());
        assert!(session.is_managed());
    }

    /// A standalone window entry owns its lifecycle by default: a third-party
    /// application must not be ended by leaving the Hub unless the user said so
    /// (spec #59 decision 12).
    #[test]
    fn a_window_entry_is_independent_unless_it_says_otherwise() {
        let independent = session_yaml(
            "app",
            "service",
            "command: app.exe\ndisplay: window\nlogging:\n  mode: off",
        );
        let managed = session_yaml(
            "app2",
            "service",
            "command: app.exe\ndisplay: window\nlifecycle: managed",
        );
        let loaded = load_from_str(&config(&[independent, managed]));

        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(loaded.sessions[0].lifecycle, LifecycleOwner::Independent);
        assert_eq!(loaded.sessions[1].lifecycle, LifecycleOwner::Managed);
    }

    /// The two dimensions are separate settings, and the only combination the
    /// app cannot produce is refused with the mode that makes it possible
    /// rather than silently downgraded (spec #59 decision 8).
    #[test]
    fn an_independent_hub_hosted_entry_is_refused_with_the_other_display_mode() {
        let entry = session_yaml("svc", "service", "command: run\nlifecycle: independent");
        let loaded = load_from_str(&config(&[entry]));

        assert!(loaded.sessions.is_empty());
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].field.as_deref(), Some("lifecycle"));
        let message = &loaded.errors[0].message;
        assert!(message.contains("display: window"), "{message}");
    }

    /// Display is a service's setting: a terminal is typed into in this window
    /// and ended by this window, so neither key belongs on one.
    #[test]
    fn display_keys_belong_to_services_only() {
        let entry = session_yaml(
            "term",
            "terminal",
            "shell: powershell\ndisplay: window\nlifecycle: managed",
        );
        let loaded = load_from_str(&config(&[entry]));

        assert!(loaded.sessions.is_empty());
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].field.as_deref(), Some("display"));
        assert!(loaded.errors[0].message.contains("service"));
    }

    /// A standalone entry has no Hub console to capture from, so `captured` is
    /// refused — with the two modes that do work named — instead of resolving
    /// into a policy that would record nothing while claiming to (spec #59
    /// decision 16).
    #[test]
    fn a_window_entry_cannot_capture_output() {
        for logging in [
            "logging:\n  mode: auto\n  source: captured",
            "logging:\n  mode: on_error\n  source: captured",
            "logging:\n  mode: always\n  source: captured",
        ] {
            let entry = session_yaml(
                "app",
                "service",
                &format!("command: app.exe\ndisplay: window\n{logging}"),
            );
            let loaded = load_from_str(&config(&[entry]));

            assert_eq!(loaded.errors.len(), 1, "for {logging}");
            assert_eq!(loaded.errors[0].field.as_deref(), Some("logging.source"));
            let message = &loaded.errors[0].message;
            assert!(message.contains("external"), "{message}");
            assert!(message.contains("display: internal"), "{message}");
        }
    }

    /// What a standalone entry *can* do: keep nothing, or link the log the
    /// application writes itself (`docs/LOGGING.md` §5).
    #[test]
    fn a_window_entry_records_nothing_or_links_the_applications_own_log() {
        let silent = session_yaml("silent", "service", "command: app.exe\ndisplay: window");
        let silent_explicit = session_yaml(
            "silent2",
            "service",
            "command: app.exe\ndisplay: window\nlogging:\n  mode: off",
        );
        let linked = session_yaml(
            "linked",
            "service",
            "command: app.exe\ndisplay: window\nlogging:\n  source: external\n  path: D:/Tools/app/log.txt",
        );
        let loaded = load_from_str(&config(&[silent, silent_explicit, linked]));

        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        for session in &loaded.sessions[..2] {
            assert_eq!(session.logging.mode, EffectiveLogMode::Off);
            assert_eq!(session.logging.source, LogSource::None);
        }
        assert_eq!(loaded.sessions[2].logging.source, LogSource::External);
        assert_eq!(
            loaded.sessions[2].logging.external_path.as_deref(),
            Some("D:/Tools/app/log.txt")
        );
    }

    #[test]
    fn report_dto_keeps_valid_sessions_and_errors_together() {
        let good = session_yaml("good", "service", "command: run");
        let bad = session_yaml("bad", "service", "");
        let report = load_from_str(&config(&[good, bad])).to_dto();
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].id, "good");
        assert_eq!(report.sessions[0].session_type, "service");
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].index, 2);
    }

    #[test]
    fn missing_config_file_is_a_fresh_install() {
        let root = TempDir::new("missing-config");
        let missing = root.0.join("config.yaml");
        let loaded = load_from_file(&missing).expect("missing file is not an error");
        assert!(loaded.sessions.is_empty());
        assert!(loaded.errors.is_empty());
        assert_eq!(loaded.to_dto().file_status, ConfigFileStatusDto::Missing);

        let empty = root.0.join("empty.yaml");
        std::fs::write(&empty, "\n # intentionally empty\n").expect("write empty config");
        let loaded = load_from_file(&empty).expect("empty config is readable");
        assert!(loaded.sessions.is_empty());
        assert!(loaded.errors.is_empty());
        assert_eq!(loaded.to_dto().file_status, ConfigFileStatusDto::Loaded);
    }

    #[test]
    fn development_fixture_loads_cleanly() {
        let fixture = include_str!("../../../fixtures/dev-config.yaml");
        let loaded = load_from_str(fixture);
        assert!(
            loaded.errors.is_empty(),
            "fixture must load without errors: {:?}",
            loaded.errors
        );
        let ids: Vec<&str> = loaded.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["sillytavern", "comfyui", "devshell"]);
        let st = &loaded.sessions[0];
        assert_eq!(st.session_type, SessionType::Service);
        assert_eq!(st.port, Some(8000));
        // mode: auto + source: captured on a service resolves to on_error.
        assert_eq!(st.logging.mode, EffectiveLogMode::OnError);
        let shell = loaded.sessions.last().expect("terminal entry");
        assert_eq!(shell.session_type, SessionType::Terminal);
        assert_eq!(shell.shell.as_deref(), Some("powershell"));
        assert_eq!(shell.logging.mode, EffectiveLogMode::Off);
    }

    /// The T11 verification fixture loads cleanly, and still covers the
    /// policies the manual checklist walks.
    ///
    /// The point is not that the file parses — it is that a checklist row
    /// cannot quietly lose its subject. Every assertion below names a row of
    /// `docs/VERIFICATION.md`; deleting the session, or changing its logging
    /// policy to something already covered, fails here rather than being
    /// discovered by whoever runs the checklist next.
    #[test]
    fn verification_fixture_loads_cleanly_and_covers_the_matrix() {
        let fixture = include_str!("../../../fixtures/verification-config.yaml");
        let loaded = load_from_str(fixture);
        assert!(
            loaded.errors.is_empty(),
            "verification fixture must load without errors: {:?}",
            loaded.errors
        );

        let ids: Vec<&str> = loaded.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "svc-listening",
                "svc-fails",
                "svc-external",
                "term-pwsh",
                "term-manual",
            ]
        );

        let by_id = |id: &str| {
            loaded
                .sessions
                .iter()
                .find(|session| session.id == id)
                .unwrap_or_else(|| panic!("`{id}` is in the fixture"))
        };

        // "a captured service writes one file per run" — `always` is the only
        // policy that writes as output arrives, so at least one service must
        // carry it (LOGGING.md §3).
        let always = by_id("svc-listening");
        assert_eq!(always.logging.mode, EffectiveLogMode::Always);
        assert_eq!(always.logging.source, LogSource::Captured);
        assert_eq!(
            always.port,
            Some(28900),
            "the readiness row needs a service that really listens"
        );

        // "on_error preserves pre-error context" (LOGGING.md 场景 C).
        assert_eq!(by_id("svc-fails").logging.mode, EffectiveLogMode::OnError);

        // "an external log is linked, not duplicated" (LOGGING.md §11): the
        // external source only means anything together with its path.
        let external = by_id("svc-external");
        assert_eq!(external.logging.source, LogSource::External);
        let external_path = external
            .logging
            .external_path
            .as_deref()
            .expect("an `external` session names the file it points at");
        assert!(
            !external_path.is_empty(),
            "an `external` session with no path has nothing to point at"
        );
        // Both of these were real defects in the first version of this fixture,
        // and neither is visible by reading it: `%LOCALAPPDATA%\...` looks like
        // a path and is eleven characters of directory name, and `access.log`
        // looks relative and resolves against the app's working directory.
        assert!(
            !external_path.contains('%'),
            "`{external_path}` is not expanded: the path is taken literally \
             (`resolve_logging`), so this names a directory called `%LOCALAPPDATA%`"
        );
        assert!(
            Path::new(external_path).is_absolute(),
            "`{external_path}` must be absolute: a relative path resolves against the \
             app's working directory, which is not the session's `cwd`"
        );

        // "a plain interactive terminal creates no log file" (LOGGING.md
        // 场景 A) and the `manual` policy the Logs tab's controls act on.
        assert_eq!(by_id("term-pwsh").logging.mode, EffectiveLogMode::Off);
        assert_eq!(by_id("term-manual").logging.mode, EffectiveLogMode::Manual);

        // "close impact is visible before destructive actions" (UI_STYLE_GUIDE
        // §5/§13): a terminal states why it exists and what stopping it costs,
        // exactly as a service does (D-027). `term-pwsh` carries the two
        // sentences the V2 terminal reference shows, so the checklist's
        // terminal rows reproduce `assets/ui/ui-v2-terminal.png` in the live
        // window instead of falling back to `关闭影响 —`.
        let terminal = by_id("term-pwsh");
        assert_eq!(
            terminal.purpose.as_deref(),
            Some("日常交互终端，跑一次性命令与 REPL。")
        );
        assert_eq!(
            terminal.close_impact.as_deref(),
            Some("仅结束本终端；不会停止其它受管服务。")
        );

        // Every command and shell must run on a machine with nothing
        // installed, or the checklist stops being repeatable. `powershell` is
        // the one tool this repo may assume (D-001: Windows-first).
        for session in &loaded.sessions {
            match session.session_type {
                SessionType::Service => {
                    let command = session.command.as_deref().expect("a service has a command");
                    assert!(
                        command.starts_with("powershell "),
                        "`{}` needs something besides Windows itself: {command}",
                        session.id
                    );
                }
                SessionType::Terminal => {
                    assert_eq!(
                        session.shell.as_deref(),
                        Some("powershell"),
                        "`{}` must use the shell every Windows has",
                        session.id
                    );
                }
            }
        }
    }
}
