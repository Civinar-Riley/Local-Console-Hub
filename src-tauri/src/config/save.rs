//! Safe appends to the user's config file (#64, spec #59 decision 15).
//!
//! The config file is the user's, not the app's: they may have written
//! comments, kept their own ordering, or put entries in it the Hub does not
//! understand yet. So adding one session is a **text edit with a refusal
//! rule**, never a re-serialization — a parse-and-write-back would quietly
//! delete every comment and reformat everything else, which is exactly the
//! "有损重写" the decision forbids.
//!
//! ## What is allowed, and what is refused
//!
//! Appending is only safe when the new entry can be placed at the end of the
//! document's *last* structure. [`scan`] establishes that: the document must
//! parse, its root must be a mapping whose only key is `sessions`, and
//! `sessions` must be a block-style list (or absent, or empty) with nothing
//! root-level after it. Anything else — a second root key, a `sessions: []`
//! written inline, a document that is not a mapping — is refused with the
//! reason rather than rewritten.
//!
//! ## Why the write is checked and atomic
//!
//! A text edit computed from a stale read is how one save silently reverts
//! another. The file is therefore re-read immediately before the write and
//! compared with what this save was computed from; a difference is a refusal,
//! not a merge. The write itself goes to a sibling temporary file and is moved
//! over the target, so a failure at any point leaves the original exactly as
//! it was — the user's file is never opened for truncation.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_yaml::Value;

use super::model::RawSessionConfig;

/// Why a save was refused or could not be completed.
///
/// The variants are the four situations spec #59 decision 15 names, plus the
/// concurrent-edit case: a refusal the caller can act on is a different thing
/// from an I/O failure, and the message the user sees has to say which.
#[derive(Debug)]
pub enum SaveError {
    /// The document is not YAML this build can read.
    Unparsable(String),
    /// The document parses, but editing it in place would mean rewriting
    /// content this build does not understand.
    Unsafe(String),
    /// The file changed between the read this save was computed from and the
    /// write.
    Changed,
    /// The read or the write itself failed (permissions, a missing directory
    /// the app cannot create, a full disk).
    Io(std::io::Error),
}

impl SaveError {
    /// The sentence shown to the user, naming the file when that helps.
    pub fn message(&self, path: &Path) -> String {
        let file = path.display();
        match self {
            SaveError::Unparsable(reason) => {
                format!("`{file}` is not valid YAML ({reason}); fix the file, then add the application again")
            }
            SaveError::Unsafe(reason) => {
                format!("`{file}` cannot be edited safely: {reason}")
            }
            SaveError::Changed => format!(
                "`{file}` changed while this application was being added; nothing was written — try again"
            ),
            SaveError::Io(error) => format!("`{file}` could not be written: {error}"),
        }
    }
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Unparsable(reason) => {
                write!(formatter, "the config file is not valid YAML: {reason}")
            }
            SaveError::Unsafe(reason) => write!(
                formatter,
                "the config file cannot be edited safely: {reason}"
            ),
            SaveError::Changed => write!(
                formatter,
                "the config file changed while it was being saved"
            ),
            SaveError::Io(error) => {
                write!(formatter, "the config file could not be written: {error}")
            }
        }
    }
}

impl std::error::Error for SaveError {}

/// Where the new entry goes in the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Nothing but blank lines and comments: there is no `sessions` key yet.
    Empty,
    /// A `sessions:` key in block form; the entry joins its list.
    Sessions {
        /// Leading spaces of the list's `- ` marker, as the file writes it.
        item_indent: usize,
    },
    /// `sessions: []` — an empty list written inline.
    ///
    /// The empty flow list and an empty block list are the same value, so
    /// opening this one up costs the user nothing: the `[]` token goes and the
    /// rest of the line (a trailing comment, if there is one) stays. `[]` is
    /// also the empty workspace the acceptance run starts from, so refusing it
    /// would mean "添加应用" cannot be the way a fresh config gets its first
    /// entry.
    EmptyInline {
        /// Byte offset of the `[`, in the source text.
        open_at: usize,
        /// Byte offset just past the header line's content — where its
        /// newline (if any) begins.
        line_end: usize,
    },
}

/// The session ids a document already defines.
///
/// Best effort by design: an entry that is too broken to have an id is not
/// going to collide with a fresh one, and refusing to list the ids of a file
/// that has one bad entry would make an unrelated save fail.
pub fn session_ids(text: &str) -> Vec<String> {
    let Ok(Value::Mapping(root)) = serde_yaml::from_str::<Value>(text) else {
        return Vec::new();
    };
    let Some(Value::Sequence(entries)) = root.get("sessions").cloned() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| entry.get("id")?.as_str().map(str::to_owned))
        .collect()
}

/// The document with `entry` appended, or the reason it cannot be.
///
/// Pure: the same text always yields the same result, so the rules above can
/// be read and tested without a filesystem.
pub fn append_session(text: &str, entry: &RawSessionConfig) -> Result<String, SaveError> {
    let shape = scan(text)?;
    if id_in(&entry.id, text) {
        return Err(SaveError::Unsafe(format!(
            "a session with the id `{}` is already defined there",
            entry.id
        )));
    }

    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };

    if let Shape::EmptyInline { open_at, line_end } = shape {
        // The header line loses its `[]` and keeps everything else, then the
        // entry follows it. This is the one inline form that can be opened up
        // without rewriting anything the user wrote: an empty list has no
        // content to reflow.
        let mut out = String::with_capacity(text.len() + 256);
        out.push_str(&text[..open_at]);
        out.push_str(&text[open_at + 2..line_end]);
        out.push_str(newline);
        append_item(&mut out, entry, 2, newline)?;
        out.push_str(&text[line_end..]);
        return Ok(out);
    }

    let mut out = String::from(text);

    match shape {
        Shape::Empty => {
            if !out.is_empty() {
                // Keep the user's comments, and separate them from what the
                // Hub adds with one blank line so the two read as two things.
                if !out.ends_with('\n') {
                    out.push_str(newline);
                }
                if !out.ends_with(&format!("{newline}{newline}")) {
                    out.push_str(newline);
                }
            }
            out.push_str("sessions:");
            out.push_str(newline);
            append_item(&mut out, entry, 2, newline)?;
        }
        Shape::Sessions { item_indent } => {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push_str(newline);
            }
            append_item(&mut out, entry, item_indent, newline)?;
        }
        // Handled above, before the text was copied.
        Shape::EmptyInline { .. } => unreachable!("the inline-empty shape returns early"),
    }
    Ok(out)
}

/// Read the config file, append `entry`, and only replace the file if it did
/// not change in between.
pub fn save_session(path: &Path, entry: &RawSessionConfig) -> Result<(), SaveError> {
    let original = read_optional(path)?;
    let updated = append_session(original.as_deref().unwrap_or(""), entry)?;
    replace(path, original, &updated)
}

/// Remove exactly one block entry without rewriting unrelated YAML.
pub fn remove_saved_session(path: &Path, id: &str) -> Result<(), SaveError> {
    let original = read_optional(path)?;
    let updated = remove_session_text(original.as_deref().unwrap_or(""), id)?;
    replace(path, original, &updated)
}

/// Replace exactly one block entry without rewriting unrelated YAML.
///
/// The entry is identified by `id`; every field of the new entry replaces the
/// old one, while keys this build does not know are kept (see
/// [`update_session_text`]).
pub fn update_saved_session(
    path: &Path,
    id: &str,
    entry: &RawSessionConfig,
) -> Result<(), SaveError> {
    let original = read_optional(path)?;
    let updated = update_session_text(original.as_deref().unwrap_or(""), id, entry)?;
    replace(path, original, &updated)
}

pub fn remove_session_text(text: &str, id: &str) -> Result<String, SaveError> {
    let Shape::Sessions { item_indent } = scan(text)? else {
        return Err(SaveError::Unsafe("没有可移除的配置项".to_owned()));
    };
    let document: Value =
        serde_yaml::from_str(text).map_err(|error| SaveError::Unparsable(error.to_string()))?;
    let entries = document
        .get("sessions")
        .and_then(Value::as_sequence)
        .ok_or_else(|| SaveError::Unsafe("没有可移除的配置项".to_owned()))?;
    let matches: Vec<_> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.get("id").and_then(Value::as_str) == Some(id))
        .map(|(index, _)| index)
        .collect();
    if matches.len() != 1 {
        return Err(SaveError::Unsafe(
            "配置项不存在或 ID 重复；未做任何修改".to_owned(),
        ));
    }
    let starts = item_starts(text, entries.len(), item_indent)?;
    let index = matches[0];
    let start = starts[index];
    let end = starts.get(index + 1).copied().unwrap_or(text.len());
    // Preserve comment and blank lines, including notes before the next entry.
    let retained: String = text[start..end]
        .split_inclusive('\n')
        .filter(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
        .collect();
    let updated = format!("{}{}{}", &text[..start], retained, &text[end..]);
    let mut expected = document;
    expected
        .get_mut("sessions")
        .and_then(Value::as_sequence_mut)
        .unwrap()
        .remove(index);
    // A now-empty block list is YAML null; make the empty list explicit.
    let updated = if entries_count(&expected) == 0 {
        let header = significant_lines(&updated)
            .into_iter()
            .find(|(_, line)| line.starts_with("sessions"))
            .unwrap();
        let colon = header.0 + header.1.find(':').unwrap() + 1;
        format!("{} []{}", &updated[..colon], &updated[colon..])
    } else {
        updated
    };
    let actual: Value =
        serde_yaml::from_str(&updated).map_err(|error| SaveError::Unparsable(error.to_string()))?;
    if actual != expected {
        return Err(SaveError::Unsafe(
            "移除会影响其他配置内容；未做任何修改".to_owned(),
        ));
    }
    Ok(updated)
}

/// The document with the entry `id` replaced by `entry`, or the reason it
/// cannot be.
///
/// Only that one block is rewritten: other entries, their order and every
/// comment outside the block are left byte-for-byte. Inside the block,
/// comment-only lines survive (they follow the rewritten fields); an inline
/// comment on a field line does not, because that line is what the edit
/// replaces.
///
/// Fields this build does not know are kept: the new entry is overlaid on the
/// mapping already in the file, so an edit cannot delete a key a newer version
/// wrote. A field the form cleared is removed, which is what makes an optional
/// value clearable from the window.
pub fn update_session_text(
    text: &str,
    id: &str,
    entry: &RawSessionConfig,
) -> Result<String, SaveError> {
    fn not_found(id: &str) -> SaveError {
        SaveError::Unsafe(format!("a session with the id `{id}` is not defined there"))
    }

    let Shape::Sessions { item_indent } = scan(text)? else {
        return Err(not_found(id));
    };
    let document: Value =
        serde_yaml::from_str(text).map_err(|error| SaveError::Unparsable(error.to_string()))?;
    let entries = document
        .get("sessions")
        .and_then(Value::as_sequence)
        .ok_or_else(|| not_found(id))?;
    let matches: Vec<_> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.get("id").and_then(Value::as_str) == Some(id))
        .map(|(index, _)| index)
        .collect();
    let index = match matches.as_slice() {
        [] => return Err(not_found(id)),
        [index] => *index,
        _ => return Err(SaveError::Unsafe("配置项 ID 重复；未做任何修改".to_owned())),
    };

    let starts = item_starts(text, entries.len(), item_indent)?;
    let start = starts[index];
    let end = starts.get(index + 1).copied().unwrap_or(text.len());
    let old = entries[index]
        .as_mapping()
        .ok_or_else(|| SaveError::Unsafe("配置项不是映射；请手动检查配置文件".to_owned()))?;
    let merged = merge_known_fields(old, entry);

    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let block = &text[start..end];
    // Everything after the old entry's last field — blank lines before the
    // next item, notes that follow it — is preserved as it was. Comments
    // between the fields are kept too, after the rewritten block.
    let trailing_at = last_significant_line_end(block);
    let suffix = &block[trailing_at..];
    let comments = interior_comments(block, trailing_at);
    let mut replacement = render_mapping_item(&merged, item_indent, newline)?;
    let comment_pad = " ".repeat(item_indent + 2);
    for comment in comments {
        replacement.push_str(&comment_pad);
        replacement.push_str(&comment);
        replacement.push_str(newline);
    }
    replacement.push_str(suffix);

    let updated = format!("{}{}{}", &text[..start], replacement, &text[end..]);

    let mut expected = document;
    expected
        .get_mut("sessions")
        .and_then(Value::as_sequence_mut)
        .and_then(|entries| entries.get_mut(index))
        .map(|slot| *slot = merged)
        .ok_or_else(|| SaveError::Unsafe("无法定位配置项；请手动检查配置文件".to_owned()))?;
    let actual: Value =
        serde_yaml::from_str(&updated).map_err(|error| SaveError::Unparsable(error.to_string()))?;
    if actual != expected {
        return Err(SaveError::Unsafe(
            "编辑会影响其他配置内容；未做任何修改".to_owned(),
        ));
    }
    Ok(updated)
}

/// Byte offsets of the document's list items.
///
/// The text is what decides where the blocks begin, so the count has to agree
/// with the parsed list; when it does not, this build cannot tell which text
/// belongs to which entry and refuses rather than guessing.
fn item_starts(text: &str, entries: usize, item_indent: usize) -> Result<Vec<usize>, SaveError> {
    let starts: Vec<_> = significant_lines(text)
        .into_iter()
        .filter(|(_, line)| {
            let trimmed = line.trim_start();
            line.len() - trimmed.len() == item_indent
                && (trimmed == "-" || trimmed.starts_with("- "))
        })
        .map(|(offset, _)| offset)
        .collect();
    if starts.len() != entries {
        return Err(SaveError::Unsafe(
            "无法安全定位配置项；请手动检查配置文件".to_owned(),
        ));
    }
    Ok(starts)
}

/// The offset just past the last line of `block` that carries structure.
fn last_significant_line_end(block: &str) -> usize {
    let mut offset = 0;
    let mut end = 0;
    for line in block.split_inclusive('\n') {
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            end = offset + line.len();
        }
        offset += line.len();
    }
    end
}

/// Comment-only lines between the item's first and last significant lines.
fn interior_comments(block: &str, significant_end: usize) -> Vec<String> {
    let mut comments = Vec::new();
    let mut offset = 0;
    for line in block.split_inclusive('\n') {
        if offset >= significant_end {
            break;
        }
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            comments.push(trimmed.to_owned());
        }
        offset += line.len();
    }
    comments
}

/// The entry fields an edit owns.
///
/// A key outside this list is either hand-written documentation or a field a
/// newer build understands; neither is something this build may delete, so
/// [`merge_known_fields`] leaves it alone.
const ENTRY_FIELDS: [&str; 14] = [
    "id",
    "name",
    "type",
    "cwd",
    "command",
    "url",
    "port",
    "purpose",
    "close_impact",
    "shell",
    "initial_command",
    "display",
    "lifecycle",
    "logging",
];

/// `old` with every field this build owns replaced by `entry`'s value.
fn merge_known_fields(old: &serde_yaml::Mapping, entry: &RawSessionConfig) -> Value {
    let mut merged = old.clone();
    let replacement = match entry_mapping(entry) {
        Value::Mapping(mapping) => mapping,
        _ => unreachable!("entry_mapping always builds a mapping"),
    };
    for field in ENTRY_FIELDS {
        let field = Value::String(field.to_owned());
        match replacement.get(&field) {
            Some(value) => {
                merged.insert(field, value.clone());
            }
            None => {
                merged.remove(&field);
            }
        }
    }
    Value::Mapping(merged)
}

fn entries_count(document: &Value) -> usize {
    document
        .get("sessions")
        .and_then(Value::as_sequence)
        .map_or(0, Vec::len)
}

/// Write `updated` over `path`, but only while the file is still `original`.
///
/// The check is the whole difference between an edit and an overwrite: a save
/// computed from a stale read would otherwise silently revert whatever the
/// user (or another tool) changed in between.
fn replace(path: &Path, original: Option<String>, updated: &str) -> Result<(), SaveError> {
    if read_optional(path)? != original {
        return Err(SaveError::Changed);
    }
    write_atomically(path, updated)
}

/// One mapping as a list item, indented into the document.
///
/// The one rendering, so "what is about to be written" and "what was written"
/// cannot drift between an append and an edit.
fn render_mapping_item(value: &Value, indent: usize, newline: &str) -> Result<String, SaveError> {
    let rendered = serde_yaml::to_string(value)
        .map_err(|error| SaveError::Unsafe(format!("the entry could not be written: {error}")))?;
    let pad = " ".repeat(indent);
    let mut out = String::new();
    for (index, line) in rendered.trim_end_matches('\n').lines().enumerate() {
        if line.is_empty() {
            out.push_str(newline);
            continue;
        }
        out.push_str(&pad);
        // `- ` starts the item; everything after it lines up under the first
        // key, which is what makes the block read like the ones the user
        // already has.
        out.push_str(if index == 0 { "- " } else { "  " });
        out.push_str(line);
        out.push_str(newline);
    }
    Ok(out)
}

/// Whether `id` is already defined by the document's `sessions` list.
fn id_in(id: &str, text: &str) -> bool {
    session_ids(text).iter().any(|existing| existing == id)
}

/// The list item for `entry`, indented into `out`.
fn append_item(
    out: &mut String,
    entry: &RawSessionConfig,
    indent: usize,
    newline: &str,
) -> Result<(), SaveError> {
    out.push_str(&render_mapping_item(
        &entry_mapping(entry),
        indent,
        newline,
    )?);
    Ok(())
}

/// The entry as a YAML mapping, carrying only the fields the user gave.
///
/// Omitting an unspecified field is the point: a written `logging: {}` or a
/// guessed `mode` would freeze a default the user never chose and that a later
/// release could not improve.
fn entry_mapping(entry: &RawSessionConfig) -> Value {
    let mut map = serde_yaml::Mapping::new();
    let mut put = |key: &str, value: Value| {
        map.insert(Value::String(key.to_owned()), value);
    };
    put("id", Value::String(entry.id.clone()));
    put("name", Value::String(entry.name.clone()));
    put("type", Value::String(entry.r#type.clone()));
    for (key, value) in [
        ("cwd", &entry.cwd),
        ("command", &entry.command),
        ("purpose", &entry.purpose),
        ("close_impact", &entry.close_impact),
        ("shell", &entry.shell),
        ("initial_command", &entry.initial_command),
    ] {
        if let Some(value) = value {
            put(key, Value::String(value.clone()));
        }
    }
    // The two #66 dimensions go in only when the entry carries them, which is
    // how a Hub-internal, Hub-managed application — the shape every entry had
    // before them — is written exactly as it was (spec #59 decision 8).
    if let Some(display) = entry.display {
        put("display", Value::String(display.as_str().to_owned()));
    }
    if let Some(lifecycle) = entry.lifecycle {
        put("lifecycle", Value::String(lifecycle.as_str().to_owned()));
    }
    if let Some(port) = entry.port {
        put("port", Value::Number(port.into()));
    }
    if let Some(url) = &entry.url {
        put("url", Value::String(url.clone()));
    }
    if let Some(logging) = &entry.logging {
        let mut block = serde_yaml::Mapping::new();
        if let Some(mode) = logging.mode {
            block.insert(
                Value::String("mode".to_owned()),
                Value::String(mode.as_str().to_owned()),
            );
        }
        if let Some(source) = logging.source {
            block.insert(
                Value::String("source".to_owned()),
                Value::String(source.as_str().to_owned()),
            );
        }
        if let Some(path) = &logging.path {
            block.insert(
                Value::String("path".to_owned()),
                Value::String(path.clone()),
            );
        }
        if !block.is_empty() {
            put("logging", Value::Mapping(block));
        }
    }
    Value::Mapping(map)
}

/// Decide whether the document can carry an appended entry, and where.
fn scan(text: &str) -> Result<Shape, SaveError> {
    if text.trim().is_empty() {
        return Ok(Shape::Empty);
    }

    // Structure first: a document that does not parse cannot be edited, and
    // one whose root is not the `sessions` mapping is not this app's file.
    let document: Value =
        serde_yaml::from_str(text).map_err(|error| SaveError::Unparsable(error.to_string()))?;
    match &document {
        Value::Null => {}
        Value::Mapping(root) => {
            for (key, value) in root {
                match key.as_str() {
                    Some("sessions") => match value {
                        Value::Null | Value::Sequence(_) => {}
                        _ => {
                            return Err(SaveError::Unsafe(
                                "`sessions` is not a list of session entries".to_owned(),
                            ))
                        }
                    },
                    Some(other) => {
                        return Err(SaveError::Unsafe(format!(
                            "it has a root key `{other}` besides `sessions`, which this build \
                             does not understand"
                        )))
                    }
                    None => {
                        return Err(SaveError::Unsafe(
                            "it has a root key this build does not understand".to_owned(),
                        ))
                    }
                }
            }
        }
        _ => {
            return Err(SaveError::Unsafe(
                "its root is not a mapping of `sessions`".to_owned(),
            ))
        }
    }

    scan_lines(text)
}

/// The lines that carry structure: no blank lines, no comments.
///
/// Each one keeps its byte offset in the source, because the inline-empty case
/// has to be opened up by editing the exact characters the user wrote rather
/// than by rewriting the document.
fn significant_lines(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for line in text.split('\n') {
        let content = line.trim_end();
        let trimmed = content.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            out.push((start, content));
        }
        start += line.len() + 1;
    }
    out
}

/// Find the `sessions:` line and the list's shape in the text itself.
///
/// The text is what decides, not the parsed value: `sessions: []` and
/// `sessions:\n  - id: a` are the same `Value` but they are not edited the same
/// way, and only the second one can simply be appended to.
fn scan_lines(text: &str) -> Result<Shape, SaveError> {
    // A leading document marker is content the user wrote, and it does not
    // move the `sessions:` key.
    let mut lines = significant_lines(text).into_iter();
    let Some((first_offset, first)) = lines.next() else {
        return Ok(Shape::Empty);
    };
    let (header_offset, header) = if first.trim() == "---" {
        match lines.next() {
            Some(line) => line,
            None => return Ok(Shape::Empty),
        }
    } else {
        (first_offset, first)
    };

    if header.starts_with(char::is_whitespace) {
        return Err(SaveError::Unsafe(
            "it does not start with a root-level `sessions:` key".to_owned(),
        ));
    }
    // `sessions` is ASCII, so its length is also its byte length and the
    // offsets below are byte offsets into the source line.
    let Some(after_colon) = header
        .strip_prefix("sessions")
        .map(|rest| rest.trim_start())
        .and_then(|rest| rest.strip_prefix(':'))
    else {
        return Err(SaveError::Unsafe(
            "it does not start with a root-level `sessions:` key".to_owned(),
        ));
    };
    let value = strip_comment(after_colon);
    if !value.trim().is_empty() {
        // An empty flow list is the one inline form that can be opened up
        // without touching anything the user wrote; a list with entries in it
        // would have to be reflowed, and that is the rewrite this module
        // refuses to do.
        if value.trim() == "[]" {
            let bracket = header.find("[]").expect("the value is exactly `[]`");
            return Ok(Shape::EmptyInline {
                open_at: header_offset + bracket,
                line_end: header_offset + header.len(),
            });
        }
        return Err(SaveError::Unsafe(format!(
            "`sessions` is written inline with entries in it (`{}`) — put them on their own lines, \
             then add the application again",
            header.trim()
        )));
    }

    let mut item_indent = None;
    for (_, line) in lines {
        let trimmed = line.trim_start();
        if line.starts_with(char::is_whitespace) || trimmed.starts_with('-') {
            if item_indent.is_none() && (trimmed == "-" || trimmed.starts_with("- ")) {
                item_indent = Some(line.len() - trimmed.len());
            }
            continue;
        }
        return Err(SaveError::Unsafe(format!(
            "it has root-level content (`{}`) after `sessions`, which this build could not place \
             an entry after",
            line.trim()
        )));
    }

    Ok(Shape::Sessions {
        // An empty list has no marker to copy, so the entry aligns under the
        // key the way a hand-written one would.
        item_indent: item_indent.unwrap_or(2),
    })
}

/// Everything after an unquoted `#` on a line, i.e. the part that is a comment.
fn strip_comment(text: &str) -> &str {
    match text.find('#') {
        Some(index) => &text[..index],
        None => text,
    }
}

/// The file's contents, or `None` when it does not exist.
///
/// A missing file is not an error here: it is a fresh install, and the save
/// creates it. Every other read failure is.
fn read_optional(path: &Path) -> Result<Option<String>, SaveError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(SaveError::Io(error)),
    }
}

/// Replace the file's contents through a temporary sibling.
///
/// The original is never opened for writing, so a failure anywhere leaves it
/// byte-for-byte as it was — including the case the decision cares about most,
/// a permission error on the target.
///
/// The temporary is flushed to the disk before the rename. NTFS makes the
/// rename itself atomic, but it does not promise the new file's *data* is on
/// disk at that point: without the sync, a power loss could leave a
/// `config.yaml` that exists and is empty or half written. The config file is
/// every saved launch entry the user has, so the barrier is worth the call.
fn write_atomically(path: &Path, contents: &str) -> Result<(), SaveError> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&directory).map_err(SaveError::Io)?;

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.yaml".to_owned());
    let temp = directory.join(format!(".{name}.{}.tmp", unique_suffix()));

    let written = std::fs::File::create(&temp).and_then(|file| {
        let mut writer = std::io::BufWriter::new(&file);
        writer.write_all(contents.as_bytes())?;
        writer.flush()?;
        drop(writer);
        file.sync_all()
    });
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(SaveError::Io(error));
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        // The original is untouched; the half-written copy must not be left
        // beside it as litter.
        let _ = std::fs::remove_file(&temp);
        return Err(SaveError::Io(error));
    }
    Ok(())
}

/// A suffix no sibling save can share, so two of them cannot collide.
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQUENCE: AtomicU32 = AtomicU32::new(0);

    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{}-{millis:x}-{sequence}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::super::model::{LogMode, LogSource, LoggingConfig};
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A directory that exists only for one test, so nothing it counts or
    /// deletes can be another test's.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQUENCE: AtomicU32 = AtomicU32::new(0);

            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "lch-64-{tag}-{}-{unique}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("the temp directory is creatable");
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The directory every fixture opens in: a real one, so the written entry
    /// still validates when the file is read back through the loader.
    fn scratch_cwd() -> String {
        std::env::temp_dir().to_string_lossy().into_owned()
    }

    fn service(id: &str) -> RawSessionConfig {
        RawSessionConfig {
            id: id.to_owned(),
            name: format!("App {id}"),
            r#type: "service".to_owned(),
            cwd: Some(scratch_cwd()),
            command: Some("node server.js".to_owned()),
            url: Some("http://127.0.0.1:8000/".to_owned()),
            port: Some(8000),
            purpose: Some("聊天前端".to_owned()),
            close_impact: Some("可停止；网页会失联".to_owned()),
            shell: None,
            initial_command: None,
            display: None,
            lifecycle: None,
            logging: Some(LoggingConfig {
                mode: Some(LogMode::OnError),
                source: Some(LogSource::Captured),
                path: None,
            }),
        }
    }

    /// A config file inside a test-owned directory, so a count of the
    /// directory's contents is this test's and nobody else's.
    struct TempFile {
        _dir: TempDir,
        path: PathBuf,
    }

    impl TempFile {
        fn with(contents: &str) -> Self {
            let dir = TempDir::new("file");
            let path = dir.0.join("config.yaml");
            fs::write(&path, contents).expect("the temp config is writable");
            TempFile { _dir: dir, path }
        }

        fn missing() -> Self {
            let dir = TempDir::new("missing");
            TempFile {
                path: dir.0.join("config.yaml"),
                _dir: dir,
            }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn read(&self) -> String {
            fs::read_to_string(&self.path).expect("the config is readable")
        }

        fn sibling_count(&self) -> usize {
            fs::read_dir(self.path.parent().expect("the file has a directory"))
                .expect("the directory is readable")
                .count()
        }
    }

    /// A fresh install: no file at all becomes a document with this entry,
    /// carrying exactly the fields the caller supplied.
    #[test]
    fn a_missing_file_is_created_with_the_entry() {
        let file = TempFile::missing();

        save_session(file.path(), &service("app")).expect("a fresh config saves");

        let updated = file.read();
        assert!(updated.starts_with("sessions:\n  - id: app\n"), "{updated}");
        for expected in [
            "name: App app",
            "type: service",
            "cwd:",
            "command: node server.js",
            "port: 8000",
            "purpose: 聊天前端",
            "close_impact: 可停止；网页会失联",
            "logging:",
            "mode: on_error",
            "source: captured",
        ] {
            assert!(
                updated.contains(expected),
                "{expected} missing from {updated}"
            );
        }
        let loaded = super::super::load_from_file(file.path()).expect("the file loads");
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].id, "app");
        assert_eq!(loaded.sessions[0].port, Some(8000));
        assert_eq!(
            loaded.sessions[0].cwd.as_deref(),
            Some(Path::new(&scratch_cwd())),
            "the working directory has to survive the round trip"
        );
        assert_eq!(
            loaded.sessions[0].url.as_deref(),
            Some("http://127.0.0.1:8000/")
        );
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    }

    /// A field the user left out is left out of the file too: writing a
    /// guessed default would freeze a choice they never made.
    #[test]
    fn unspecified_fields_are_not_written() {
        let mut bare = service("app");
        bare.cwd = None;
        bare.url = None;
        bare.port = None;
        bare.purpose = None;
        bare.close_impact = None;
        bare.logging = None;
        let file = TempFile::missing();

        save_session(file.path(), &bare).expect("a fresh config saves");

        let updated = file.read();
        for absent in ["cwd:", "url:", "port:", "purpose:", "logging:"] {
            assert!(
                !updated.contains(absent),
                "{absent} should not be written: {updated}"
            );
        }
        let loaded = super::super::load_from_str(&updated);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(loaded.sessions[0].logging.mode.as_str(), "on_error");
    }

    /// The entry joins the end of the list and everything around it survives:
    /// comments, ordering, and the entries the user wrote by hand.
    #[test]
    fn an_append_keeps_comments_order_and_the_entries_already_there() {
        let original = "\
# 我的本地服务
sessions:
  # 常用
  - id: first
    name: First
    type: terminal
    shell: powershell
  - id: second
    name: Second
    type: service
    command: run me
# 手工维护的说明
";
        let file = TempFile::with(original);

        save_session(file.path(), &service("third")).expect("the append saves");

        let updated = file.read();
        assert!(
            updated.starts_with(original),
            "the original text must survive untouched: {updated}"
        );
        assert!(
            updated.contains("  - id: third\n    name: App third\n"),
            "{updated}"
        );
        let loaded = super::super::load_from_str(&updated);
        let ids: Vec<&str> = loaded.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["first", "second", "third"], "{:?}", loaded.errors);
    }

    /// A list the user indented their own way keeps that way, so the appended
    /// entry does not stick out of the file it was added to.
    #[test]
    fn the_appended_entry_follows_the_files_own_indentation() {
        let file = TempFile::with(
            "sessions:\n    - id: first\n      name: First\n      type: service\n      command: run\n",
        );

        save_session(file.path(), &service("second")).expect("the append saves");

        assert!(
            file.read()
                .contains("    - id: second\n      name: App second\n"),
            "{}",
            file.read()
        );
        assert!(super::super::load_from_str(&file.read()).errors.is_empty());
    }

    /// An empty `sessions:` list is still a place an entry belongs.
    #[test]
    fn an_entry_joins_an_empty_list() {
        let file = TempFile::with("sessions:\n");

        save_session(file.path(), &service("first")).expect("the append saves");

        let loaded = super::super::load_from_str(&file.read());
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].id, "first");
    }

    /// What the user chose about display and lifecycle is written; what they
    /// did not choose is not (spec #59 decision 8). A Hub-internal, Hub-managed
    /// entry is the shape every entry had before #66, so an entry that says
    /// nothing about either dimension is written exactly as it always was.
    #[test]
    fn display_and_lifecycle_are_written_only_when_the_entry_carries_them() {
        let file = TempFile::with("sessions:\n");

        save_session(file.path(), &service("plain")).expect("the append saves");
        let mut window = service("window");
        window.display = Some(crate::config::DisplayMode::Window);
        // A standalone entry has no Hub console to capture from, so it states
        // no logging block at all.
        window.logging = None;
        save_session(file.path(), &window).expect("the append saves");
        let mut managed = service("managed");
        managed.display = Some(crate::config::DisplayMode::Window);
        managed.lifecycle = Some(crate::config::LifecycleOwner::Managed);
        managed.logging = None;
        save_session(file.path(), &managed).expect("the append saves");

        let text = file.read();
        assert!(
            !text.contains("display: internal"),
            "an unstated default must not be frozen into the file: {text}"
        );
        assert_eq!(text.matches("display: window").count(), 2, "{text}");
        assert_eq!(
            text.matches("lifecycle:").count(),
            1,
            "only the entry that asked for management states one: {text}"
        );

        // The round trip is the point: what was written is what the next start
        // reads, with the lifecycle default applied where nothing was written.
        let loaded = super::super::load_from_str(&text);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(
            loaded.sessions[0].display,
            crate::config::DisplayMode::Internal
        );
        assert_eq!(
            loaded.sessions[1].lifecycle,
            crate::config::LifecycleOwner::Independent
        );
        assert_eq!(
            loaded.sessions[2].lifecycle,
            crate::config::LifecycleOwner::Managed
        );
    }

    /// A comment-only file keeps its comments and gains the key it lacked.
    #[test]
    fn a_comment_only_file_keeps_its_comments() {
        let file = TempFile::with("# 只有注释\n");

        save_session(file.path(), &service("first")).expect("the append saves");

        let updated = file.read();
        assert!(updated.starts_with("# 只有注释\n"), "{updated}");
        assert_eq!(super::super::load_from_str(&updated).sessions.len(), 1);
    }

    /// A file that is not YAML is refused, and stays exactly as it was.
    #[test]
    fn broken_yaml_is_refused_and_the_file_is_left_alone() {
        let broken = "sessions: [ uh oh\n";
        let file = TempFile::with(broken);

        let error = save_session(file.path(), &service("any")).expect_err("broken YAML is refused");

        assert!(matches!(error, SaveError::Unparsable(_)), "{error:?}");
        assert_eq!(file.read(), broken, "a refusal must not touch the file");
    }

    /// A root key this build does not understand is content it cannot place an
    /// entry after, so it is refused rather than rewritten away.
    #[test]
    fn an_unknown_root_key_is_refused_rather_than_dropped() {
        let original =
            "sessions:\n  - id: a\n    name: A\n    type: service\n    command: run\nversion: 2\n";
        let file = TempFile::with(original);

        let error =
            save_session(file.path(), &service("b")).expect_err("unknown content is refused");

        assert!(matches!(error, SaveError::Unsafe(_)), "{error:?}");
        assert_eq!(file.read(), original);
    }

    /// `sessions: []` is the empty workspace the acceptance run starts from, so
    /// it is opened up rather than refused: the entry joins it, and the line
    /// keeps everything except the two brackets.
    #[test]
    fn an_empty_inline_list_is_opened_up_and_the_entry_joins_it() {
        let file = TempFile::with("sessions: []  # 还没开始\n");

        save_session(file.path(), &service("first")).expect("an empty inline list can be extended");

        let updated = file.read();
        assert!(
            updated.starts_with("sessions:   # 还没开始\n  - id: first\n"),
            "{updated}"
        );
        let loaded = super::super::load_from_str(&updated);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].id, "first");
    }

    /// An inline list with entries in it would have to be reflowed, so it is
    /// refused with what to change instead.
    #[test]
    fn an_inline_list_with_entries_is_refused_with_the_way_out() {
        let original = "sessions: [{ id: a, name: A, type: service, command: run }]\n";
        let file = TempFile::with(original);

        let error =
            save_session(file.path(), &service("b")).expect_err("a full inline list is refused");

        let SaveError::Unsafe(reason) = error else {
            panic!("expected the unsafe refusal, got {error:?}");
        };
        assert!(reason.contains("inline"), "{reason}");
        assert_eq!(file.read(), original);
    }

    /// Two saves for one id would make the second entry a duplicate the loader
    /// rejects; the file's own list is what says so.
    #[test]
    fn an_id_the_file_already_defines_is_refused() {
        let original =
            "sessions:\n  - id: taken\n    name: Taken\n    type: service\n    command: run\n";
        let file = TempFile::with(original);

        let error =
            save_session(file.path(), &service("taken")).expect_err("a duplicate id is refused");

        assert!(matches!(error, SaveError::Unsafe(_)), "{error:?}");
        assert_eq!(file.read(), original);
    }

    /// The concurrent-edit case: the file moved on between the read the save
    /// was computed from and the write, so nothing is written.
    #[test]
    fn a_file_that_changed_underneath_the_save_is_not_overwritten() {
        let file = TempFile::with("sessions:\n");
        let original = fs::read_to_string(file.path()).expect("readable");
        let updated = append_session(&original, &service("app")).expect("appendable");
        // Someone else edits the file between the read and the write — which
        // is what the second read inside `replace` is there to notice.
        fs::write(file.path(), format!("{original}# someone else was here\n"))
            .expect("the other write lands");

        let error =
            replace(file.path(), Some(original), &updated).expect_err("the change is noticed");

        assert!(matches!(error, SaveError::Changed), "{error:?}");
        let after = file.read();
        assert!(after.contains("someone else"), "{after}");
        assert!(
            !after.contains("id: app"),
            "nothing may be written: {after}"
        );
    }

    /// A file Windows will not let this process replace — the permission case
    /// decision 15 names. The original keeps its exact bytes, and the temporary
    /// copy the failed replace was going to move is not left beside it.
    #[test]
    fn a_config_that_cannot_be_replaced_keeps_its_contents() {
        let original =
            "sessions:\n  - id: locked\n    name: Locked\n    type: service\n    command: run\n";
        let file = TempFile::with(original);
        // The original permissions are kept as they were, not reconstructed:
        // "make it writable again" would mean different things on different
        // platforms, and this test only needs the file back the way it was.
        let writable = fs::metadata(file.path())
            .expect("the config has metadata")
            .permissions();
        let mut read_only = writable.clone();
        read_only.set_readonly(true);
        fs::set_permissions(file.path(), read_only).expect("the config can be marked read-only");

        let result = save_session(file.path(), &service("app"));

        // Whatever the platform says, the file must be exactly as it was, and
        // the attempt must be reported rather than swallowed.
        let after = file.read();
        let siblings = file.sibling_count();
        fs::set_permissions(file.path(), writable).expect("the original permissions come back");

        let error = result.expect_err("a read-only config cannot be replaced");
        assert!(matches!(error, SaveError::Io(_)), "{error:?}");
        assert_eq!(after, original, "a refusal must not touch the file");
        assert_eq!(siblings, 1, "the failed replace left no temporary file");
    }

    /// A path that cannot be read or written reports it as an I/O failure
    /// rather than pretending a save happened.
    #[test]
    fn a_path_that_is_not_a_file_reports_an_io_failure() {
        let directory = TempDir::new("not-a-file");

        let error =
            save_session(&directory.0, &service("app")).expect_err("a directory is not a file");

        assert!(matches!(error, SaveError::Io(_)), "{error:?}");
        assert!(
            directory.0.is_dir(),
            "the path has to survive a failed write"
        );
    }

    /// The write leaves no temporary file behind: a half-written copy beside
    /// the user's config would be litter they never asked for.
    #[test]
    fn no_temporary_file_is_left_beside_the_config() {
        let file = TempFile::with("sessions:\n");
        assert_eq!(file.sibling_count(), 1, "the config alone to start with");

        save_session(file.path(), &service("app")).expect("the append saves");

        assert_eq!(
            file.sibling_count(),
            1,
            "still exactly one file: the config"
        );
    }

    /// The ids a document defines, including a fresh one after a save, so the
    /// caller that mints an id can avoid the ones already taken.
    #[test]
    fn session_ids_lists_what_the_document_defines() {
        let text = "sessions:\n  - id: a\n    name: A\n    type: service\n    command: run\n  - id: b\n    name: B\n    type: service\n    command: run\n";

        assert_eq!(session_ids(text), ["a", "b"]);
        assert!(session_ids("sessions: []\n").is_empty());
        assert!(session_ids("not: yaml: at all").is_empty());
    }

    /// A name that collides with an entry the file holds but the registry has
    /// not loaded (a broken one) still gets a free id from the caller; the id
    /// list is what makes that possible.
    #[test]
    fn session_ids_sees_entries_the_loader_would_reject() {
        let text = "sessions:\n  - id: broken\n    name: Broken\n    type: nonsense\n";

        assert_eq!(session_ids(text), ["broken"]);
    }
}

#[cfg(test)]
mod removal_tests {
    use super::*;

    #[test]
    fn removal_preserves_other_entries_comments_and_crlf() {
        let text = "# header\r\nsessions: # list\r\n  - id: a\r\n    name: A\r\n  # keep note\r\n  - id: b\r\n    unknown: 'keep this'\r\n";
        let result = remove_session_text(text, "a").unwrap();
        assert_eq!(result, "# header\r\nsessions: # list\r\n  # keep note\r\n  - id: b\r\n    unknown: 'keep this'\r\n");
    }

    #[test]
    fn last_removal_leaves_a_valid_empty_list() {
        let result =
            remove_session_text("sessions: # note\n  - id: a\n    name: A\n", "a").unwrap();
        assert_eq!(result, "sessions: [] # note\n");
    }

    #[test]
    fn ambiguous_or_unsupported_documents_are_refused() {
        for text in [
            "sessions:\n  - id: a\n  - id: a\n",
            "sessions: [{id: a}]",
            "sessions:\n  - id: b\n",
            "sessions:\n  - id: a\nother: preserve",
        ] {
            assert!(remove_session_text(text, "a").is_err());
        }
    }
}

#[cfg(test)]
mod update_tests {
    use super::*;

    /// An edited service entry. `cwd` need not exist here: the pure text edit
    /// never validates — that is the app layer's job before it writes.
    fn entry(id: &str, name: &str) -> RawSessionConfig {
        RawSessionConfig {
            id: id.to_owned(),
            name: name.to_owned(),
            r#type: "service".to_owned(),
            cwd: Some(r"D:\Tools\App".to_owned()),
            command: Some("run app".to_owned()),
            url: None,
            port: None,
            purpose: None,
            close_impact: None,
            shell: None,
            initial_command: None,
            display: None,
            lifecycle: None,
            logging: None,
        }
    }

    fn sessions(document: &Value) -> &Vec<Value> {
        document
            .get("sessions")
            .and_then(Value::as_sequence)
            .expect("the document has a sessions list")
    }

    #[test]
    fn an_edit_rewrites_only_its_own_block() {
        let text = "\
# 我的本地服务
sessions:
  # 常用
  - id: first
    name: First
    type: service
    command: old
  - id: second
    name: Second
    type: service
    command: keep me
# 手工维护的说明
";
        let updated = update_session_text(text, "first", &entry("first", "First edited")).unwrap();

        assert!(
            updated.starts_with("# 我的本地服务\nsessions:\n  # 常用\n"),
            "{updated}"
        );
        assert!(
            updated.contains(
                "  - id: second\n    name: Second\n    type: service\n    command: keep me\n"
            ),
            "{updated}"
        );
        assert!(updated.ends_with("# 手工维护的说明\n"), "{updated}");
        let loaded: Value = serde_yaml::from_str(&updated).expect("the edited file parses");
        let first = &sessions(&loaded)[0];
        let second = &sessions(&loaded)[1];
        assert_eq!(
            first.get("name").and_then(Value::as_str),
            Some("First edited")
        );
        assert_eq!(
            first.get("command").and_then(Value::as_str),
            Some("run app")
        );
        assert_eq!(second.get("name").and_then(Value::as_str), Some("Second"));
    }

    /// A key this build does not know is not this build's to delete. A newer
    /// version's field has to survive an edit made here.
    #[test]
    fn unknown_entry_fields_survive_the_rewrite() {
        let text = "sessions:\n  - id: app\n    name: App\n    type: service\n    command: old\n    future_field: keep\n  - id: other\n    name: Other\n";
        let updated = update_session_text(text, "app", &entry("app", "App")).unwrap();

        assert!(updated.contains("future_field: keep"), "{updated}");
        let loaded: Value = serde_yaml::from_str(&updated).unwrap();
        assert_eq!(
            sessions(&loaded)[0]
                .get("future_field")
                .and_then(Value::as_str),
            Some("keep")
        );
    }

    /// Comments written inside the entry survive; only the field lines the
    /// edit owns are re-rendered.
    #[test]
    fn comments_inside_the_entry_survive() {
        let text = "sessions:\n  - id: app\n    # why this command\n    command: old\n    name: App\n    type: service\n";
        let updated = update_session_text(text, "app", &entry("app", "App")).unwrap();

        assert!(updated.contains("# why this command"), "{updated}");
        assert!(
            updated.find("# why this command").unwrap() > updated.find("- id: app").unwrap(),
            "the note stays inside the entry: {updated}"
        );
    }

    /// Clearing an optional field means removing it from the file, which is
    /// what makes a blank box in the form an edit rather than a no-op.
    #[test]
    fn a_cleared_field_is_removed() {
        let text = "sessions:\n  - id: app\n    name: App\n    type: service\n    command: old\n    url: 'http://127.0.0.1:8000/'\n    purpose: old purpose\n";
        let updated = update_session_text(text, "app", &entry("app", "App")).unwrap();

        let loaded: Value = serde_yaml::from_str(&updated).unwrap();
        assert!(sessions(&loaded)[0].get("url").is_none(), "{updated}");
        assert!(sessions(&loaded)[0].get("purpose").is_none(), "{updated}");
    }

    #[test]
    fn crlf_is_preserved() {
        let text = "sessions:\r\n  - id: app\r\n    name: App\r\n    type: service\r\n    command: old\r\n";
        let updated = update_session_text(text, "app", &entry("app", "App")).unwrap();

        assert!(updated.contains("\r\n"), "the file's newlines are kept");
        assert!(
            !updated.replace("\r\n", "").contains('\n'),
            "no lone LF is introduced: {updated:?}"
        );
    }

    #[test]
    fn a_missing_duplicate_or_inline_entry_is_refused() {
        for text in [
            "sessions:\n",
            "sessions: []\n",
            "sessions:\n  - id: app\n  - id: app\n",
            "sessions: [{id: app}]",
            "sessions:\n  - id: other\n",
        ] {
            assert!(
                update_session_text(text, "app", &entry("app", "App")).is_err(),
                "{text:?} must be refused"
            );
        }
    }
}
