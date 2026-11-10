//! Adding an application to the Hub (#64, spec #59 decisions 7, 10 and 15).
//!
//! "添加应用" is the secondary entry: where "新建 PowerShell" makes a throwaway
//! terminal, this one *saves* a launch configuration — name, working directory
//! and command, plus the optional words a user needs to judge the thing later —
//! and the Hub then lists, starts and reuses it like any configured session.
//!
//! ## Why the registry is written to before the file is
//!
//! The two halves of an added application have to agree: a row in the window
//! with nothing behind it, or a config entry the window cannot show, is the
//! "幽灵项" the decision forbids. Registering first is what makes that
//! possible — [`SessionCore::register`] is where a duplicate id is refused, and
//! it refuses *before* anything is written, so the common failure costs the
//! file nothing. A write that then fails takes the registration back
//! ([`SessionCore::unregister`]) and the operation reports the reason.
//!
//! ## Why the id is derived rather than asked for
//!
//! The form asks for a name, a directory and a command (spec #59 decision 7) —
//! not for an id. The id still has to exist: it is what the registry, the
//! config file and a session's log directory are keyed by, so it is derived
//! from the name and made unique against everything already taken.
//!
//! ## Both display modes, and the two dimensions they are not
//!
//! The form offers 展示方式 (Hub-internal or the application's own window) and,
//! for a window entry, whether the Hub manages its lifecycle (#66). They are
//! written to the file only when the user chose something other than the
//! default — the same rule the optional fields already follow, so an entry
//! nobody customised stays as short as the one a user would type.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;

use crate::config::{
    save_session, session_ids, update_saved_session, validate_entry, DisplayMode, LifecycleOwner,
    LoggingConfig, RawSessionConfig, SessionType,
};
use crate::session::core::{CreatedSession, SessionCore};

use super::form::FormError;

/// The form's fields, as the window sends them.
///
/// `name`, `cwd` and `command` are the required three (spec #59 decision 7);
/// everything else is optional and keeps the config layer's own defaults and
/// validation. The session is always a `service`: this entry adds an
/// application the Hub hosts, and the interactive-terminal shape is what "新建
/// PowerShell" and its saved form (#65) are for.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewApplication {
    pub name: String,
    pub cwd: String,
    pub command: String,
    /// What the application is for; free text (D-027).
    pub purpose: Option<String>,
    /// What stopping it costs the user; free text (D-027).
    pub close_impact: Option<String>,
    pub port: Option<u16>,
    pub url: Option<String>,
    /// Where the application is displayed (#66). Absent means Hub-internal —
    /// the mode every entry written before this field existed had — so a form
    /// that leaves the choice alone does not freeze a default into the file.
    pub display: Option<DisplayMode>,
    /// Who ends the application's run (#66). Absent means the display mode's
    /// own default: the Hub for a Hub-hosted entry, the application itself for
    /// one that keeps its window.
    pub lifecycle: Option<LifecycleOwner>,
    /// The `logging:` block verbatim, so the form cannot invent a policy the
    /// config layer would not accept.
    pub logging: Option<LoggingConfig>,
}

/// Validate, register and save one new application.
///
/// On success the session is in the registry *and* in the config file, so the
/// window can list it now and the next start can load it (stories 30–32). On
/// failure nothing has changed: no file content, no registry row.
///
/// The answer is the same pair a quick-entry terminal answers with — the
/// configuration and the state it really has — so the window can select and
/// render the new row without following the call with a list read that may not
/// have caught up (spec #59 decision 4).
pub fn add_application(
    core: &SessionCore,
    config_file: Option<&Path>,
    form: NewApplication,
) -> Result<CreatedSession, FormError> {
    let Some(config_file) = config_file else {
        return Err(FormError::new(
            None,
            "无法确定配置文件位置；请检查系统应用数据目录后重启应用。",
        ));
    };

    let occupancy = occupancy(core, config_file)?;
    let raw = raw_entry(&form, &occupancy.ids)?;
    // The entry takes the position after the last one the file defines, which
    // is the number its validation messages are indexed by.
    let config = validate_entry(occupancy.file_entries + 1, &raw)
        .map_err(|error| FormError::new(error.field.as_deref(), error.message))?;

    // The registry first: a duplicate id is refused here, while the config file
    // is still untouched.
    let runtime = core
        .register(config.clone())
        .map_err(|error| FormError::new(None, format!("会话未能加入列表：{}", error.message)))?;

    if let Err(error) = save_session(config_file, &raw) {
        core.unregister(&config.id);
        return Err(FormError::new(None, error.message(config_file)));
    }

    // Only now, and never before: the event says "this application exists", and
    // until the file holds it that would be a claim the next start would not
    // keep.
    core.publish_created(&config.id);
    Ok(CreatedSession { config, runtime })
}

/// Validate, apply and persist an edit of one configured application.
///
/// The id is the session's, not the form's: an edit changes what the entry
/// runs, never which session it is. Fields the form cleared are removed from
/// the entry; keys this build does not know are left in place (see
/// [`crate::config::update_session_text`]).
///
/// The registry moves first, exactly as [`add_application`] registers first:
/// validation and the "nothing is running" check happen there, while the file
/// is still untouched. A write that then fails puts the previous configuration
/// back, and the answer is the file's refusal. Only after the file holds the
/// edit is it announced (`session-saved`), because until then a listener would
/// be showing a configuration the next start would not load.
pub fn update_application(
    core: &SessionCore,
    config_file: Option<&Path>,
    session_id: &str,
    form: NewApplication,
) -> Result<CreatedSession, FormError> {
    let Some(config_file) = config_file else {
        return Err(FormError::new(
            None,
            "无法确定配置文件位置；请检查系统应用数据目录后重启应用。",
        ));
    };

    let Some(entry) = core.session_entry(session_id) else {
        return Err(FormError::new(
            None,
            format!("会话 `{session_id}` 不在受管名单里，无法编辑"),
        ));
    };
    if entry.temporary {
        return Err(FormError::new(
            None,
            "临时终端还没有配置文件；请先用“保存启动配置”保存，再编辑它。",
        ));
    }
    if entry.config.session_type != SessionType::Service {
        return Err(FormError::new(
            None,
            "这里编辑的是应用配置；已保存终端的启动方式由它自己的“保存启动配置”管理。",
        ));
    }

    let raw = raw_entry_for(&form, session_id.to_owned())?;
    // The file has to define this entry, and its position is what a validation
    // message counts by. Reading it here also refuses before the registry moves
    // when the file no longer holds the session (a hand edit, a stale row).
    let text = std::fs::read_to_string(config_file).map_err(|error| {
        FormError::new(
            None,
            format!("无法读取配置文件 `{}`：{error}", config_file.display()),
        )
    })?;
    let ids = session_ids(&text);
    let matches = ids.iter().filter(|id| id.as_str() == session_id).count();
    let position = ids.iter().position(|id| id == session_id);
    let Some(position) = position.filter(|_| matches == 1) else {
        return Err(FormError::new(
            None,
            format!(
                "`{}` 里没有唯一的 `{session_id}` 配置项；请检查配置文件后重试",
                config_file.display()
            ),
        ));
    };

    let config = validate_entry(position + 1, &raw)
        .map_err(|error| FormError::new(error.field.as_deref(), error.message))?;

    let previous = core
        .update_config(session_id, config.clone())
        .map_err(|error| FormError::new(None, error.message))?;

    if let Err(error) = update_saved_session(config_file, session_id, &raw) {
        let mut message = error.message(config_file);
        if core.update_config(session_id, previous.config).is_err() {
            message.push_str("；列表中的配置未能恢复，请重试或重启应用");
        }
        return Err(FormError::new(None, message));
    }

    core.publish_saved(session_id);
    let runtime = core
        .snapshot(session_id)
        .ok_or_else(|| FormError::new(None, "会话状态无法读取；请重启应用后重试"))?;
    Ok(CreatedSession { config, runtime })
}

/// What the config file and the registry already hold.
///
/// The two are asked together because the one new entry has to fit both, and
/// they can disagree: a config entry that failed validation is in the file but
/// not in the registry, and a temporary terminal is in the registry but not in
/// the file. Either one would collide with a freshly minted id.
struct Occupancy {
    /// Every session id in use, from both sides.
    ids: BTreeSet<String>,
    /// How many entries the file itself defines — the position a new one takes.
    file_entries: usize,
}

fn occupancy(core: &SessionCore, config_file: &Path) -> Result<Occupancy, FormError> {
    let text = match std::fs::read_to_string(config_file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(FormError::new(
                None,
                format!("无法读取配置文件 `{}`：{error}", config_file.display()),
            ))
        }
    };
    let from_file = session_ids(&text);
    let mut ids: BTreeSet<String> = from_file.iter().cloned().collect();
    ids.extend(core.entries().into_iter().map(|entry| entry.config.id));
    Ok(Occupancy {
        ids,
        file_entries: from_file.len(),
    })
}

/// The three required form values, trimmed, or the field's own refusal.
fn required_fields(form: &NewApplication) -> Result<(String, String, String), FormError> {
    let required = |value: &str, field: &str, hint: &str| -> Result<String, FormError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(FormError::new(
                Some(field),
                format!("`{field}` is empty — {hint}"),
            ));
        }
        Ok(trimmed.to_owned())
    };

    let name = required(&form.name, "name", "set the name shown in the session list")?;
    let cwd = required(
        &form.cwd,
        "cwd",
        "set the directory the application runs in, e.g. `D:\\Tools\\ComfyUI`",
    )?;
    let command = required(
        &form.command,
        "command",
        "set the command that starts the application, e.g. `node server.js`",
    )?;
    Ok((name, cwd, command))
}

/// The entry to write for an addition, with its id minted against everything
/// already taken.
fn raw_entry(
    form: &NewApplication,
    taken: &BTreeSet<String>,
) -> Result<RawSessionConfig, FormError> {
    let (name, cwd, command) = required_fields(form)?;
    Ok(raw_config(form, mint_id(&name, taken), name, cwd, command))
}

/// The entry to write for an edit: the same fields, under the id the session
/// already has.
fn raw_entry_for(form: &NewApplication, id: String) -> Result<RawSessionConfig, FormError> {
    let (name, cwd, command) = required_fields(form)?;
    Ok(raw_config(form, id, name, cwd, command))
}

fn raw_config(
    form: &NewApplication,
    id: String,
    name: String,
    cwd: String,
    command: String,
) -> RawSessionConfig {
    RawSessionConfig {
        id,
        name,
        r#type: "service".to_owned(),
        cwd: Some(cwd),
        command: Some(command),
        url: form.url.clone(),
        port: form.port,
        purpose: form.purpose.clone(),
        close_impact: form.close_impact.clone(),
        shell: None,
        initial_command: None,
        display: form.display,
        lifecycle: form.lifecycle,
        logging: form.logging.clone(),
    }
}

/// A session id for `name` that nothing already holds.
fn mint_id(name: &str, taken: &BTreeSet<String>) -> String {
    let base = slug(name);
    if !taken.contains(&base) {
        return base;
    }
    for ordinal in 2..=999u32 {
        let candidate = format!("{base}-{ordinal}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    // Unreachable for any real config; falling back to the clock keeps the
    // function total rather than inventing a panic.
    format!("{base}-{}", std::process::id())
}

/// The filesystem-safe stem of a display name.
///
/// Session ids become log directory names, so the rules are
/// [`crate::config::is_filesystem_safe_component`]'s: ASCII alphanumerics,
/// `-` and `_`, starting with an alphanumeric. A name with no ASCII in it —
/// 一个纯中文的名字 — keeps none of its characters, which is what the `app`
/// fallback and the ordinal are for.
fn slug(name: &str) -> String {
    const MAX: usize = 48;

    let mut out = String::new();
    let mut separator = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !out.is_empty() {
                out.push('-');
            }
            separator = false;
            out.push(character.to_ascii_lowercase());
        } else {
            separator = true;
        }
        if out.len() >= MAX {
            break;
        }
    }
    if out.is_empty() {
        "app".to_owned()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_from_file, ConfigFileStatusDto, SaveError};
    use crate::session::core::SessionCore;
    use crate::session::state::SessionStatus;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A config file inside a directory of this test's own.
    struct TempConfig(PathBuf);

    impl TempConfig {
        fn with(contents: Option<&str>) -> Self {
            static SEQUENCE: AtomicU32 = AtomicU32::new(0);

            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "lch-64-app-{}-{unique}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&directory).expect("the directory is creatable");
            let path = directory.join("config.yaml");
            if let Some(contents) = contents {
                fs::write(&path, contents).expect("the config is writable");
            }
            TempConfig(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn read(&self) -> String {
            fs::read_to_string(&self.0).expect("the config is readable")
        }
    }

    impl Drop for TempConfig {
        fn drop(&mut self) {
            if let Some(directory) = self.0.parent() {
                let _ = fs::remove_dir_all(directory);
            }
        }
    }

    fn form(name: &str) -> NewApplication {
        NewApplication {
            name: name.to_owned(),
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            command: "Run-Server".to_owned(),
            purpose: None,
            close_impact: None,
            port: None,
            url: None,
            display: None,
            lifecycle: None,
            logging: None,
        }
    }

    /// The whole promise of the entry: what was saved is listed now, and the
    /// file the next start reads has it too (stories 30–32).
    #[test]
    fn an_added_application_is_registered_and_persisted() {
        let config = TempConfig::with(None);
        let core = SessionCore::without_listener();

        let added = add_application(&core, Some(config.path()), form("ComfyUI"))
            .expect("a fresh install accepts the first application");

        assert_eq!(added.config.id, "comfyui");
        let live = core.snapshot("comfyui").expect("the session is registered");
        assert_eq!(
            live.status,
            SessionStatus::Stopped,
            "adding must not start it"
        );
        assert_eq!(core.entries().len(), 1);

        let loaded = load_from_file(config.path()).expect("the file loads");
        assert_eq!(loaded.file_status, ConfigFileStatusDto::Loaded);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].id, "comfyui");
        assert_eq!(loaded.sessions[0].name, "ComfyUI");
        assert_eq!(loaded.sessions[0].command.as_deref(), Some("Run-Server"));
    }

    #[test]
    fn removed_application_is_absent_after_reload_and_disk_logs_are_retained() {
        let config = TempConfig::with(None);
        let core = SessionCore::without_listener();
        let first = add_application(&core, Some(config.path()), form("First")).unwrap();
        add_application(&core, Some(config.path()), form("Second")).unwrap();
        let log = config.path().with_file_name("retained.log");
        fs::write(&log, "keep").unwrap();
        core.remove_application(&first.config.id, config.path())
            .unwrap();
        assert!(core.snapshot(&first.config.id).is_none());
        let loaded = load_from_file(config.path()).unwrap();
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].id, "second");
        assert_eq!(fs::read_to_string(log).unwrap(), "keep");
    }

    #[cfg(windows)]
    #[test]
    fn removal_refuses_a_live_run_but_accepts_its_retained_ended_handle() {
        let config = TempConfig::with(None);
        let core = SessionCore::without_listener();
        let mut application = form("App");
        application.command = "cmd.exe /c ping -n 120 127.0.0.1".to_owned();
        let added = add_application(&core, Some(config.path()), application).unwrap();
        core.start(&added.config.id).unwrap();
        let before = fs::read_to_string(config.path()).unwrap();
        assert!(core
            .remove_application(&added.config.id, config.path())
            .is_err());
        assert_eq!(fs::read_to_string(config.path()).unwrap(), before);
        core.force_stop(&added.config.id).unwrap();
        core.remove_application(&added.config.id, config.path())
            .unwrap();
        assert!(core.snapshot(&added.config.id).is_none());
        assert!(load_from_file(config.path()).unwrap().sessions.is_empty());
    }

    #[test]
    fn failed_removal_keeps_registry_and_config_unchanged() {
        let config = TempConfig::with(None);
        let core = SessionCore::without_listener();
        let added = add_application(&core, Some(config.path()), form("App")).unwrap();
        let unsafe_text = "sessions: [{id: app}]";
        fs::write(config.path(), unsafe_text).unwrap();
        assert!(core
            .remove_application(&added.config.id, config.path())
            .is_err());
        assert!(core.snapshot(&added.config.id).is_some());
        assert_eq!(fs::read_to_string(config.path()).unwrap(), unsafe_text);
    }

    /// The optional half of the form reaches the file, and keeps the config
    /// layer's own vocabulary.
    #[test]
    fn the_optional_fields_are_saved_as_given() {
        let config = TempConfig::with(None);
        let core = SessionCore::without_listener();
        let mut application = form("Chat Frontend");
        application.purpose = Some("聊天前端".to_owned());
        application.close_impact = Some("可停止；网页会失联".to_owned());
        application.port = Some(8000);
        application.url = Some("http://127.0.0.1:8000/".to_owned());
        application.logging = Some(LoggingConfig {
            mode: Some(crate::config::LogMode::OnError),
            source: Some(crate::config::LogSource::Captured),
            path: None,
        });

        add_application(&core, Some(config.path()), application).expect("the form saves");

        let loaded = load_from_file(config.path()).expect("the file loads");
        let session = &loaded.sessions[0];
        assert_eq!(session.purpose.as_deref(), Some("聊天前端"));
        assert_eq!(session.close_impact.as_deref(), Some("可停止；网页会失联"));
        assert_eq!(session.port, Some(8000));
        assert_eq!(session.url.as_deref(), Some("http://127.0.0.1:8000/"));
        assert_eq!(session.logging.mode.as_str(), "on_error");
        assert_eq!(session.logging.source.as_str(), "captured");
    }

    /// An entry the user already had is untouched, and the new one joins it.
    #[test]
    fn an_existing_configuration_keeps_its_entries() {
        let config = TempConfig::with(Some(
            "# 手工维护\nsessions:\n  - id: existing\n    name: Existing\n    type: terminal\n    shell: powershell\n",
        ));
        let core = SessionCore::without_listener();
        core.register(
            load_from_file(config.path())
                .expect("loads")
                .sessions
                .remove(0),
        )
        .expect("registers");

        add_application(&core, Some(config.path()), form("Second")).expect("the append saves");

        let updated = config.read();
        assert!(updated.starts_with("# 手工维护\n"), "{updated}");
        let loaded = load_from_file(config.path()).expect("the file loads");
        let ids: Vec<&str> = loaded.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["existing", "second"]);
        assert_eq!(core.entries().len(), 2);
    }

    /// A name that is already an id gets a free one, so the save cannot collide
    /// with what is on disk.
    #[test]
    fn a_taken_id_is_minted_around() {
        let config = TempConfig::with(None);
        let core = SessionCore::without_listener();

        let first = add_application(&core, Some(config.path()), form("ComfyUI")).expect("saves");
        let second = add_application(&core, Some(config.path()), form("ComfyUI")).expect("saves");

        assert_eq!(first.config.id, "comfyui");
        assert_eq!(second.config.id, "comfyui-2");
        let loaded = load_from_file(config.path()).expect("the file loads");
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(loaded.sessions.len(), 2);
    }

    /// An id the file holds but the registry does not — a broken entry the
    /// loader skipped — is still taken.
    #[test]
    fn an_id_only_the_file_holds_is_still_taken() {
        let config = TempConfig::with(Some(
            "sessions:\n  - id: comfyui\n    name: Broken\n    type: nonsense\n",
        ));
        let core = SessionCore::without_listener();

        let added = add_application(&core, Some(config.path()), form("ComfyUI")).expect("saves");

        assert_eq!(
            added.config.id, "comfyui-2",
            "the file's id must not be reused"
        );
    }

    /// A name with no ASCII in it still produces a usable id.
    #[test]
    fn a_name_without_ascii_still_mints_a_safe_id() {
        let config = TempConfig::with(None);
        let core = SessionCore::without_listener();

        let added = add_application(&core, Some(config.path()), form("秋叶启动器")).expect("saves");

        assert_eq!(added.config.id, "app");
        assert!(crate::config::is_filesystem_safe_component(
            &added.config.id
        ));
        assert_eq!(
            added.config.name, "秋叶启动器",
            "the display name is kept as typed"
        );
    }

    /// A form missing a required field is refused by name, and nothing is
    /// written or registered.
    #[test]
    fn a_missing_required_field_is_refused_without_side_effects() {
        let config = TempConfig::with(None);
        let core = SessionCore::without_listener();

        for (field, blank) in [("name", ""), ("cwd", "   "), ("command", "")] {
            let mut application = form("App");
            match field {
                "name" => application.name = blank.to_owned(),
                "cwd" => application.cwd = blank.to_owned(),
                _ => application.command = blank.to_owned(),
            }

            let error = add_application(&core, Some(config.path()), application)
                .expect_err("a required field is required");

            assert_eq!(error.field.as_deref(), Some(field), "{}", error.message);
            assert!(
                core.entries().is_empty(),
                "{field} must not register anything"
            );
            assert!(
                !config.path().exists(),
                "nothing may be written for {field}"
            );
        }
    }

    /// A directory that is not there is the config layer's own answer, with the
    /// field it belongs to.
    #[test]
    fn a_directory_that_is_not_there_is_refused_with_its_field() {
        let config = TempConfig::with(None);
        let core = SessionCore::without_listener();
        let mut application = form("App");
        application.cwd = "Z:/definitely/not/here/lch-64".to_owned();

        let error = add_application(&core, Some(config.path()), application)
            .expect_err("a missing directory cannot host an application");

        assert_eq!(error.field.as_deref(), Some("cwd"));
        assert!(
            error.message.contains("Z:/definitely/not/here/lch-64"),
            "{}",
            error.message
        );
        assert!(core.entries().is_empty());
        assert!(!config.path().exists());
    }

    /// A broken config file refuses the save instead of rewriting it, and the
    /// registration is taken back so the window cannot list a phantom.
    #[test]
    fn a_broken_config_is_refused_and_the_registration_is_withdrawn() {
        let broken = "sessions: [ uh oh\n";
        let config = TempConfig::with(Some(broken));
        let core = SessionCore::without_listener();

        let error = add_application(&core, Some(config.path()), form("App"))
            .expect_err("a broken file cannot be appended to");

        assert!(error.message.contains("YAML"), "{}", error.message);
        assert_eq!(config.read(), broken, "the user's file must survive");
        assert!(
            core.entries().is_empty(),
            "a refused save must leave no row behind: {:?}",
            core.entries()
                .iter()
                .map(|e| e.config.id.clone())
                .collect::<Vec<_>>()
        );
    }

    /// Where there is no config file location, the entry says so rather than
    /// saving somewhere unintended.
    #[test]
    fn no_config_location_is_refused() {
        let core = SessionCore::without_listener();

        let error =
            add_application(&core, None, form("App")).expect_err("there is nowhere to save");

        assert!(error.field.is_none());
        assert!(error.message.contains("配置文件"), "{}", error.message);
        assert!(core.entries().is_empty());
    }

    /// A form value the config layer rejects keeps the layer's own message and
    /// field, so the two cannot drift into two vocabularies.
    #[test]
    fn a_form_value_the_config_layer_rejects_keeps_its_field() {
        let config = TempConfig::with(None);
        let core = SessionCore::without_listener();
        let mut application = form("App");
        application.port = Some(0);

        let error = add_application(&core, Some(config.path()), application)
            .expect_err("port 0 is not usable");

        assert_eq!(error.field.as_deref(), Some("port"));
        assert!(error.message.contains("1-65535"), "{}", error.message);
        assert!(core.entries().is_empty());
        assert!(!config.path().exists());
    }

    /// The whole promise of an edit: the row and the file describe the same
    /// entry afterwards, under the id it already had.
    #[test]
    fn an_edit_updates_the_row_and_the_file_under_the_same_id() {
        let config = TempConfig::with(None);
        let core = SessionCore::without_listener();
        let added = add_application(&core, Some(config.path()), form("ComfyUI")).unwrap();

        let mut edited = form("ComfyUI Studio");
        edited.purpose = Some("编辑后的用途".to_owned());
        edited.port = Some(8189);
        let updated = update_application(&core, Some(config.path()), &added.config.id, edited)
            .expect("the edit saves");

        assert_eq!(updated.config.id, "comfyui");
        assert_eq!(updated.config.name, "ComfyUI Studio");
        assert_eq!(
            core.session_config("comfyui").expect("registered").name,
            "ComfyUI Studio"
        );

        let loaded = load_from_file(config.path()).expect("the file loads");
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].id, "comfyui");
        assert_eq!(loaded.sessions[0].name, "ComfyUI Studio");
        assert_eq!(loaded.sessions[0].purpose.as_deref(), Some("编辑后的用途"));
        assert_eq!(loaded.sessions[0].port, Some(8189));
    }

    /// Editing one entry leaves the rest of the user's document alone: other
    /// entries, their comments, and keys this build does not own.
    #[test]
    fn an_edit_preserves_the_rest_of_the_document() {
        let original = "\
# 手工维护
sessions:
  - id: app
    name: App
    type: service
    command: run
  # second note
  - id: second
    name: Second
    type: service
    command: two
";
        let config = TempConfig::with(Some(original));
        let core = SessionCore::without_listener();
        let entry = load_from_file(config.path())
            .expect("loads")
            .sessions
            .remove(0);
        core.register(entry).expect("registers");

        let mut edited = form("App");
        edited.name = "App Edited".to_owned();
        update_application(&core, Some(config.path()), "app", edited).expect("the edit saves");

        let text = config.read();
        assert!(text.starts_with("# 手工维护\n"), "{text}");
        assert!(text.contains("# second note"), "{text}");
        assert!(
            text.contains("  - id: second\n    name: Second\n"),
            "{text}"
        );
        let loaded = load_from_file(config.path()).expect("the file loads");
        let ids: Vec<&str> = loaded.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["app", "second"]);
        assert_eq!(loaded.sessions[0].name, "App Edited");
    }

    /// A refused validation writes nothing and changes nothing; the form gets
    /// the field the config layer named.
    #[test]
    fn a_refused_edit_leaves_the_row_and_the_file_unchanged() {
        let config = TempConfig::with(None);
        let core = SessionCore::without_listener();
        let added = add_application(&core, Some(config.path()), form("App")).unwrap();
        let before = config.read();

        let mut edited = form("App");
        edited.port = Some(0);
        let error = update_application(&core, Some(config.path()), &added.config.id, edited)
            .expect_err("port 0 is not usable");

        assert_eq!(error.field.as_deref(), Some("port"));
        assert_eq!(config.read(), before);
        assert_eq!(
            core.session_config("app").expect("registered").name,
            "App",
            "the row keeps the configuration it had"
        );
    }

    /// A live run was started from the configuration being edited; the edit is
    /// refused until the run is settled.
    #[cfg(windows)]
    #[test]
    fn editing_a_live_application_is_refused() {
        let config = TempConfig::with(None);
        let core = SessionCore::without_listener();
        let mut application = form("App");
        application.command = "cmd.exe /c ping -n 120 127.0.0.1".to_owned();
        let added = add_application(&core, Some(config.path()), application).unwrap();
        core.start(&added.config.id).expect("start succeeds");
        let before = config.read();

        let error = update_application(
            &core,
            Some(config.path()),
            &added.config.id,
            form("App Edited"),
        )
        .expect_err("a live run cannot be edited");

        assert!(error.message.contains("stop"), "{}", error.message);
        assert_eq!(config.read(), before);
        assert_eq!(core.session_config("app").expect("registered").name, "App");
        core.force_stop("app").expect("cleanup");
    }

    /// A row the file no longer holds (a hand edit took it out) is refused
    /// before anything is written: the file is the source of the entry.
    #[test]
    fn editing_an_entry_the_file_no_longer_holds_is_refused() {
        let config = TempConfig::with(None);
        let core = SessionCore::without_listener();
        let added = add_application(&core, Some(config.path()), form("App")).unwrap();
        let other =
            "sessions:\n  - id: other\n    name: Other\n    type: service\n    command: run\n";
        fs::write(config.path(), other).unwrap();

        let error = update_application(
            &core,
            Some(config.path()),
            &added.config.id,
            form("App Edited"),
        )
        .expect_err("the file does not define this id");

        assert!(error.message.contains("app"), "{}", error.message);
        assert_eq!(config.read(), other);
        assert_eq!(core.session_config("app").expect("registered").name, "App");
    }

    /// The slug rules, stated where they are decided.
    #[test]
    fn a_name_becomes_a_filesystem_safe_stem() {
        assert_eq!(slug("ComfyUI"), "comfyui");
        assert_eq!(slug("Chat  Frontend"), "chat-frontend");
        assert_eq!(slug("  leading and trailing  "), "leading-and-trailing");
        assert_eq!(slug("秋叶启动器"), "app");
        assert_eq!(slug("---"), "app");
        assert_eq!(slug("a/b\\c:d"), "a-b-c-d");
        for name in ["ComfyUI", "秋叶 启动器", "///"] {
            assert!(
                crate::config::is_filesystem_safe_component(&slug(name)),
                "{name:?} did not yield a usable id"
            );
        }
    }

    /// A name long enough to matter is trimmed to something a path can hold.
    #[test]
    fn a_long_name_is_trimmed_to_a_usable_length() {
        let long = "a".repeat(200);

        let id = slug(&long);

        assert!(id.len() <= 64, "{id} is too long to be a path component");
        assert!(crate::config::is_filesystem_safe_component(&id));
    }

    /// The last line of defence behind a lost race: even if two adds picked the
    /// same id, the shared save refuses the second one instead of writing an
    /// entry the loader would reject as a duplicate.
    #[test]
    fn a_second_entry_with_the_same_id_is_refused_by_the_save() {
        let config = TempConfig::with(Some("sessions:\n"));
        let entry = raw_entry(&form("App"), &BTreeSet::new()).expect("a usable entry");

        save_session(config.path(), &entry).expect("the first write lands");
        let error = save_session(config.path(), &entry).expect_err("the second is refused");

        assert!(matches!(error, SaveError::Unsafe(_)), "{error:?}");
        let loaded = load_from_file(config.path()).expect("the file loads");
        assert_eq!(loaded.sessions.len(), 1);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    }
}
