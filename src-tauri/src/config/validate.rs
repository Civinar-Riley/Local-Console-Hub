//! Per-session validation and `auto` logging resolution.
//!
//! Every check produces an actionable message (which session, which field,
//! what to do about it) as required by `docs/DEVELOPMENT.md` §9 and
//! `docs/MVP_IMPLEMENTATION_SPEC.md` §13. Validation never launches a
//! process and never mutates the filesystem.

use std::path::{Path, PathBuf};

use url::Url;

use super::model::{
    DisplayMode, EffectiveLogMode, EffectiveLogging, LifecycleOwner, LogMode, LogSource,
    LoggingConfig, RawSessionConfig, SessionConfig, SessionType,
};
use super::paths::is_filesystem_safe_component;

/// A single per-session configuration problem.
///
/// `index == 0` marks a file-level problem (bad YAML, unreadable root);
/// otherwise it is the 1-based position in `sessions:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfigError {
    /// 1-based entry number, or 0 for file-level errors.
    pub index: usize,
    /// Best-effort session id when the entry parsed far enough to have one.
    pub session_id: Option<String>,
    /// Offending field hint, e.g. `port`.
    pub field: Option<String>,
    /// Human-readable, actionable message.
    pub message: String,
}

impl SessionConfigError {
    fn field(index: usize, id: &str, field: &str, message: impl Into<String>) -> Self {
        SessionConfigError {
            index,
            session_id: Some(id.to_owned()),
            field: Some(field.to_owned()),
            message: message.into(),
        }
    }
}

/// Validate one raw session entry. `cwd` existence is checked against the
/// filesystem as of load time (spec §13 "invalid required working
/// directory").
pub fn validate_entry(
    index: usize,
    raw: &RawSessionConfig,
) -> Result<SessionConfig, SessionConfigError> {
    let id = raw.id.trim();
    if id.is_empty() {
        return Err(SessionConfigError {
            index,
            session_id: None,
            field: Some("id".to_owned()),
            message: "session id is empty — set a unique id such as `sillytavern`".to_owned(),
        });
    }
    if !is_filesystem_safe_component(id) {
        return Err(SessionConfigError::field(
            index,
            id,
            "id",
            format!(
                "session id `{id}` is not filesystem-safe — use 1-64 ASCII letters, digits, \
                 `-` or `_`, starting with a letter or digit, and not a Windows device name \
                 such as `nul` (ids become log directory names)"
            ),
        ));
    }
    let name = raw.name.trim();
    if name.is_empty() {
        return Err(SessionConfigError::field(
            index,
            id,
            "name",
            "`name` is empty — set the display name shown in the session list".to_owned(),
        ));
    }

    let session_type = match raw.r#type.as_str() {
        "service" => SessionType::Service,
        "terminal" => SessionType::Terminal,
        other => {
            return Err(SessionConfigError::field(
                index,
                id,
                "type",
                format!(
                    "unsupported type `{other}` — use `service` (managed process) or \
                     `terminal` (interactive shell)"
                ),
            ));
        }
    };

    reject_cross_type_fields(index, id, raw, session_type)?;

    // Trimmed optional strings; empty values are rejected as typos.
    let trimmed =
        |value: &Option<String>, field: &str| -> Result<Option<String>, SessionConfigError> {
            match value {
                None => Ok(None),
                Some(v) if v.trim().is_empty() => Err(SessionConfigError::field(
                    index,
                    id,
                    field,
                    format!("`{field}` is empty — remove it or set a real value"),
                )),
                Some(v) => Ok(Some(v.trim().to_owned())),
            }
        };

    let cwd = trimmed(&raw.cwd, "cwd")?;
    if let Some(cwd) = &cwd {
        if !Path::new(cwd).is_dir() {
            return Err(SessionConfigError::field(
                index,
                id,
                "cwd",
                format!("working directory `{cwd}` does not exist or is not a directory"),
            ));
        }
    }

    let display = raw.display.unwrap_or_default();
    let lifecycle = resolve_lifecycle(index, id, display, raw.lifecycle)?;
    let logging = resolve_logging(index, id, session_type, display, raw.logging.as_ref())?;

    match session_type {
        SessionType::Service => Ok(SessionConfig {
            id: id.to_owned(),
            name: name.to_owned(),
            session_type,
            cwd: cwd.map(PathBuf::from),
            command: Some(require_command(
                index,
                id,
                &trimmed(&raw.command, "command")?,
            )?),
            url: validate_url(index, id, &raw.url)?,
            port: validate_port(index, id, raw.port)?,
            purpose: trimmed(&raw.purpose, "purpose")?,
            close_impact: trimmed(&raw.close_impact, "close_impact")?,
            shell: None,
            initial_command: None,
            display,
            lifecycle,
            logging,
        }),
        // A terminal is hosted by the Hub's own console, so it has no display
        // mode to choose: `display` and `lifecycle` are rejected for it above,
        // and the values it carries are the ones that describe what it is.
        SessionType::Terminal => Ok(SessionConfig {
            id: id.to_owned(),
            name: name.to_owned(),
            session_type,
            cwd: cwd.map(PathBuf::from),
            command: None,
            url: None,
            port: None,
            purpose: trimmed(&raw.purpose, "purpose")?,
            close_impact: trimmed(&raw.close_impact, "close_impact")?,
            shell: Some(require_shell(index, id, &trimmed(&raw.shell, "shell")?)?),
            initial_command: trimmed(&raw.initial_command, "initial_command")?,
            display: DisplayMode::Internal,
            lifecycle: LifecycleOwner::Managed,
            logging,
        }),
    }
}

/// Settle who ends an entry's run (#66, spec #59 decisions 8 and 12).
///
/// The two dimensions are independent, but not freely combinable: an entry the
/// Hub hosts on its own console is ended by the Hub because the console —
/// and the job object behind it — is the Hub's, so `independent` there would
/// describe a state this app cannot produce. That combination is refused with
/// a message naming the other display mode rather than quietly ignored, which
/// is the same rule `reject_cross_type_fields` applies to fields that belong to
/// the other session type.
fn resolve_lifecycle(
    index: usize,
    id: &str,
    display: DisplayMode,
    lifecycle: Option<LifecycleOwner>,
) -> Result<LifecycleOwner, SessionConfigError> {
    match (display, lifecycle) {
        (DisplayMode::Internal, Some(LifecycleOwner::Independent)) => {
            Err(SessionConfigError::field(
                index,
                id,
                "lifecycle",
                "`lifecycle: independent` only applies to `display: window` — an entry the Hub \
                 hosts on its own console is ended by the Hub; use `display: window` to keep \
                 the application's own window and console, or `lifecycle: managed`",
            ))
        }
        (_, Some(owner)) => Ok(owner),
        (_, None) => Ok(LifecycleOwner::default_for(display)),
    }
}

/// Field ownership follows the tables in the implementation spec §4.
/// Fields of the other session type are rejected so a config written for
/// one type but declared as the other fails loudly instead of silently
/// dropping options.
fn reject_cross_type_fields(
    index: usize,
    id: &str,
    raw: &RawSessionConfig,
    session_type: SessionType,
) -> Result<(), SessionConfigError> {
    let (fields, owner): (&[(&str, bool)], &str) = match session_type {
        SessionType::Service => (
            &[
                ("shell", raw.shell.is_some()),
                ("initial_command", raw.initial_command.is_some()),
            ],
            "terminal",
        ),
        SessionType::Terminal => (
            &[
                ("command", raw.command.is_some()),
                ("url", raw.url.is_some()),
                ("port", raw.port.is_some()),
                // A terminal is displayed in the Hub and ended by the Hub,
                // always (#66): it is the Hub's own console the user is
                // typing into, so there is no other window to keep.
                ("display", raw.display.is_some()),
                ("lifecycle", raw.lifecycle.is_some()),
            ],
            "service",
        ),
    };
    for (field, present) in fields {
        if *present {
            return Err(SessionConfigError::field(
                index,
                id,
                field,
                format!("`{field}` only applies to `type: {owner}` sessions"),
            ));
        }
    }
    Ok(())
}

fn require_command(
    index: usize,
    id: &str,
    value: &Option<String>,
) -> Result<String, SessionConfigError> {
    value.clone().ok_or_else(|| {
        SessionConfigError::field(
            index,
            id,
            "command",
            "service sessions require `command`, e.g. `command: node server.js`".to_owned(),
        )
    })
}

fn require_shell(
    index: usize,
    id: &str,
    value: &Option<String>,
) -> Result<String, SessionConfigError> {
    value.clone().ok_or_else(|| {
        SessionConfigError::field(
            index,
            id,
            "shell",
            "terminal sessions require `shell`, e.g. `shell: powershell`".to_owned(),
        )
    })
}

fn validate_url(
    index: usize,
    id: &str,
    raw_url: &Option<String>,
) -> Result<Option<String>, SessionConfigError> {
    let Some(raw_url) = raw_url else {
        return Ok(None);
    };
    let parsed = Url::parse(raw_url.trim()).map_err(|err| {
        SessionConfigError::field(
            index,
            id,
            "url",
            format!("malformed URL `{raw_url}` ({err}) — use e.g. `http://127.0.0.1:8000`"),
        )
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(SessionConfigError::field(
            index,
            id,
            "url",
            format!(
                "URL scheme `{}://` cannot be opened as a service page — use `http://` or \
                 `https://`",
                parsed.scheme()
            ),
        ));
    }
    Ok(Some(parsed.to_string()))
}

fn validate_port(
    index: usize,
    id: &str,
    port: Option<u16>,
) -> Result<Option<u16>, SessionConfigError> {
    if port == Some(0) {
        return Err(SessionConfigError::field(
            index,
            id,
            "port",
            "port 0 is not a usable service port — set 1-65535 or remove `port`".to_owned(),
        ));
    }
    Ok(port)
}

/// Validate a raw `logging:` block and resolve `auto` into an
/// [`EffectiveLogging`], following `docs/LOGGING.md` §3 defaults:
///
/// - interactive terminal → `off` / `none`
/// - service with an external log → link it, no extra capture
/// - service without an external log → `on_error` / `captured`
/// - standalone window application → `off` / `none`, external logs still linked
///
/// Contradictions (e.g. `mode: off` with `source: captured`) are rejected
/// with a message naming the valid combination instead of guessing.
///
/// The standalone case is not a fourth default but a boundary: the Hub has no
/// console of that application's (#66), so `captured` cannot be honoured there
/// at all and is refused rather than resolved into a mode that would record
/// nothing while claiming to (spec #59 decision 16 — no faked capture state).
/// Linking the application's own log is unaffected, because that is a *link*
/// to a file the application writes, not a second copy of its console.
fn resolve_logging(
    index: usize,
    id: &str,
    session_type: SessionType,
    display: DisplayMode,
    logging: Option<&LoggingConfig>,
) -> Result<EffectiveLogging, SessionConfigError> {
    let raw = logging.cloned().unwrap_or_default();
    // Whether this entry keeps its own console, which is what makes Hub-side
    // capture impossible rather than merely unusual.
    let owns_console = display.is_window();

    let external_path = match raw.path.as_deref() {
        None => None,
        Some(path) if path.trim().is_empty() => {
            return Err(SessionConfigError::field(
                index,
                id,
                "logging.path",
                "`logging.path` is empty — set the application-owned log file path or remove it",
            ));
        }
        Some(path) => {
            if raw.source != Some(LogSource::External) {
                return Err(SessionConfigError::field(
                    index,
                    id,
                    "logging.path",
                    "`logging.path` is only valid together with `source: external`",
                ));
            }
            Some(path.trim().to_owned())
        }
    };
    let off_none = || EffectiveLogging {
        mode: EffectiveLogMode::Off,
        source: LogSource::None,
        external_path: None,
    };
    let service_default = || EffectiveLogging {
        mode: EffectiveLogMode::OnError,
        source: LogSource::Captured,
        external_path: None,
    };

    // Defaults (`mode` unspecified) behave exactly like `auto` so the
    // omitted logging block and `mode: auto` cannot diverge.
    let mode = raw.mode.unwrap_or(LogMode::Auto);

    // The two things a standalone entry's own console decides, stated once
    // rather than as guards on the arms below (#66). It cannot be captured
    // from — the Hub has no console of that application's — and its `auto`
    // resolves to "record nothing" instead of the service default, which is
    // `on_error` with Hub-captured output. Everything else about its logging
    // (an external log, an explicit contradiction) is the same table as any
    // other service's.
    if owns_console {
        if raw.source == Some(LogSource::Captured) {
            return Err(owns_its_console(index, id));
        }
        if mode == LogMode::Auto && raw.source != Some(LogSource::External) {
            return Ok(off_none());
        }
    }

    let effective = match (raw.source, mode) {
        // External: the application owns the log; the Hub only links it.
        (Some(LogSource::External), LogMode::Always | LogMode::Auto) => {
            let path = external_path.clone().ok_or_else(|| {
                SessionConfigError::field(
                    index,
                    id,
                    "logging.path",
                    "`source: external` requires `logging.path` pointing at the \
                     application-owned log file",
                )
            })?;
            EffectiveLogging {
                mode: EffectiveLogMode::Always,
                source: LogSource::External,
                external_path: Some(path),
            }
        }
        (Some(LogSource::External), LogMode::Off) => {
            return Err(SessionConfigError::field(
                index,
                id,
                "logging.mode",
                "`mode: off` persists nothing while `source: external` links an \
                 application-owned log, which is contradictory — remove the logging \
                 block, or use `mode: always`",
            ));
        }
        (Some(LogSource::External), mode) => {
            return Err(SessionConfigError::field(
                index,
                id,
                "logging.mode",
                format!(
                    "`mode: {}` writes Hub-captured output, which contradicts `source: \
                     external` — use `mode: always` (or omit mode) so the Hub only links \
                     the application's own log",
                    mode_name(mode)
                ),
            ));
        }
        // Captured: the Hub persists stdout/stderr by policy.
        (Some(LogSource::Captured), LogMode::Auto) => {
            if session_type == SessionType::Terminal {
                return Err(SessionConfigError::field(
                    index,
                    id,
                    "logging.mode",
                    "`mode: auto` resolves terminal sessions to `off`, which contradicts \
                     `source: captured` — pick an explicit mode (`always`, `on_error` or \
                     `manual`)",
                ));
            }
            service_default()
        }
        (Some(LogSource::Captured), LogMode::Off) => {
            return Err(SessionConfigError::field(
                index,
                id,
                "logging.mode",
                "`mode: off` persists nothing, so `source: captured` is contradictory — use \
                 `source: none`",
            ));
        }
        (Some(LogSource::Captured), mode) => EffectiveLogging {
            mode: effective_mode(mode),
            source: LogSource::Captured,
            external_path: None,
        },
        // Explicit none, or no source at all with `mode: off`: no
        // persistence either way.
        (Some(LogSource::None), LogMode::Off) | (None, LogMode::Off) => off_none(),
        (Some(LogSource::None), LogMode::Auto) => {
            if session_type == SessionType::Service {
                return Err(SessionConfigError::field(
                    index,
                    id,
                    "logging.mode",
                    "`mode: auto` resolves service sessions to `on_error` with captured \
                     output, which contradicts `source: none` — set `mode: off` to disable \
                     persistence explicitly",
                ));
            }
            off_none()
        }
        (Some(LogSource::None), mode) => {
            return Err(SessionConfigError::field(
                index,
                id,
                "logging.mode",
                format!(
                    "`mode: {}` has nothing to persist because `source: none` is set — use \
                     `source: captured` (or `external` with a path)",
                    mode_name(mode)
                ),
            ));
        }
        // Source unspecified: per-type defaults from `docs/LOGGING.md` §3.
        (None, LogMode::Auto) => {
            if session_type == SessionType::Service {
                service_default()
            } else {
                off_none()
            }
        }
        (None, mode) => {
            return Err(SessionConfigError::field(
                index,
                id,
                "logging.source",
                format!(
                    "`mode: {}` needs `source: captured` or `source: external` — which \
                     output should be persisted?",
                    mode_name(mode)
                ),
            ));
        }
    };
    Ok(effective)
}

/// Why a standalone window entry cannot capture output, phrased as what the
/// user can do instead (#66).
///
/// Naming the alternative matters more here than usual: `captured` is the
/// default a user gets by copying any other service entry, so the refusal has
/// to say what the standalone equivalent of it is rather than only what is
/// wrong.
fn owns_its_console(index: usize, id: &str) -> SessionConfigError {
    SessionConfigError::field(
        index,
        id,
        "logging.source",
        "`display: window` keeps the application's own console, so the Hub cannot capture its \
         output — link the application's own log with `source: external` and `logging.path`, \
         set `mode: off` (or remove the logging block) to record nothing, or use \
         `display: internal` to host the command on a Hub console",
    )
}

fn mode_name(mode: LogMode) -> &'static str {
    match mode {
        LogMode::Off => "off",
        LogMode::Always => "always",
        LogMode::OnError => "on_error",
        LogMode::Manual => "manual",
        LogMode::Auto => "auto",
    }
}

fn effective_mode(mode: LogMode) -> EffectiveLogMode {
    match mode {
        LogMode::Off | LogMode::Auto => EffectiveLogMode::Off,
        LogMode::Always => EffectiveLogMode::Always,
        LogMode::OnError => EffectiveLogMode::OnError,
        LogMode::Manual => EffectiveLogMode::Manual,
    }
}
