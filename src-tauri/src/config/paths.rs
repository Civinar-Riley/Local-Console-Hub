//! App-data path resolution and human-readable log path layout.
//!
//! Windows conventions frozen in `docs/MVP_IMPLEMENTATION_SPEC.md` §2 and
//! `docs/LOGGING.md` §5:
//!
//! ```text
//! %APPDATA%\LocalConsoleHub\config.yaml
//! %LOCALAPPDATA%\LocalConsoleHub\logs\<session_id>\<YYYY-MM>\<run>.log
//! %LOCALAPPDATA%\LocalConsoleHub\metadata\<session_id>\<YYYY-MM>\<run>.json
//! %LOCALAPPDATA%\LocalConsoleHub\cache\...
//! ```
//!
//! Runtime logs must never land beside the executable by default. All path
//! construction is pure and offset-explicit so it is testable without a
//! running app; [`AppPaths::from_env`] is the thin environment wrapper.

use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset};

/// Whether `value` is safe to use as a single path component (session ids
/// and run ids become directory/file names — D-011).
///
/// Rules: 1..=64 chars, ASCII alphanumeric/`_`/`-` only, must start with an
/// alphanumeric character, and not one of the names Windows reserves for its
/// own devices: a log directory named `nul` is silently redirected to the
/// device instead of created, so such an id is refused here rather than left
/// to fail at its first log write.
pub fn is_filesystem_safe_component(value: &str) -> bool {
    let bytes = value.as_bytes();
    match bytes.first() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    bytes.len() <= 64
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
        && !matches!(
            value.to_ascii_lowercase().as_str(),
            "con"
                | "prn"
                | "aux"
                | "nul"
                | "com0"
                | "com1"
                | "com2"
                | "com3"
                | "com4"
                | "com5"
                | "com6"
                | "com7"
                | "com8"
                | "com9"
                | "lpt0"
                | "lpt1"
                | "lpt2"
                | "lpt3"
                | "lpt4"
                | "lpt5"
                | "lpt6"
                | "lpt7"
                | "lpt8"
                | "lpt9"
        )
}

/// Directory name used under both app-data roots.
pub const APP_DIR_NAME: &str = "LocalConsoleHub";
/// Config file name inside the config root.
pub const CONFIG_FILE_NAME: &str = "config.yaml";

/// Well-known application data locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    /// Roaming config file, e.g. `%APPDATA%\LocalConsoleHub\config.yaml`.
    pub config_file: PathBuf,
    /// Root for Hub-generated logs (`docs/LOGGING.md` §5).
    pub logs_dir: PathBuf,
    /// Root for run metadata (`docs/LOGGING.md` §6).
    pub metadata_dir: PathBuf,
    /// Root for disposable caches.
    pub cache_dir: PathBuf,
}

impl AppPaths {
    /// Pure constructor from platform roots, for tests and non-env callers.
    pub fn new(config_root: &Path, data_root: &Path) -> Self {
        let config_app_dir = config_root.join(APP_DIR_NAME);
        let data_app_dir = data_root.join(APP_DIR_NAME);
        AppPaths {
            config_file: config_app_dir.join(CONFIG_FILE_NAME),
            logs_dir: data_app_dir.join("logs"),
            metadata_dir: data_app_dir.join("metadata"),
            cache_dir: data_app_dir.join("cache"),
        }
    }

    /// Resolve paths from the platform environment.
    ///
    /// Returns `None` when the OS does not provide the expected base
    /// directories (e.g. `APPDATA`/`LOCALAPPDATA` unset on Windows);
    /// callers turn that into an actionable message rather than guessing a
    /// fallback location.
    pub fn from_env() -> Option<Self> {
        #[cfg(feature = "acceptance")]
        {
            let root = PathBuf::from(std::env::var_os("LCH_ACCEPTANCE_ROOT")?);
            root.is_absolute()
                .then(|| Self::new(&root.join("roaming"), &root.join("local")))
        }
        #[cfg(not(feature = "acceptance"))]
        {
            let config_root = dirs::config_dir()?;
            let data_root = dirs::data_local_dir()?;
            Some(Self::new(&config_root, &data_root))
        }
    }
}

/// `logs/<session_id>/<YYYY-MM>` for a run started at `unix_secs` with an
/// explicit UTC offset (local months are what humans browse by, D-011).
///
/// `None` when `session_id` is not a single safe path component or the
/// timestamp is out of range.
pub fn session_log_dir(
    logs_root: &Path,
    session_id: &str,
    unix_secs: i64,
    utc_offset_secs: i32,
) -> Option<PathBuf> {
    if !is_filesystem_safe_component(session_id) {
        return None;
    }
    let offset = FixedOffset::east_opt(utc_offset_secs)?;
    let month = to_local(unix_secs, &offset)?.format("%Y-%m").to_string();
    Some(logs_root.join(session_id).join(month))
}

/// Run log file name like `2026-09-24_09-30-15__run-8f31.log`
/// (`docs/LOGGING.md` §5: timestamped, run-tagged, no database needed to
/// understand ownership).
///
/// `None` when `run_id` is not filesystem-safe or the timestamp is out of
/// range.
pub fn run_log_filename(unix_secs: i64, utc_offset_secs: i32, run_id: &str) -> Option<String> {
    run_file_name(unix_secs, utc_offset_secs, run_id, "log")
}

/// One run's file name under any of the per-run roots, e.g.
/// `2026-09-24_09-30-15__run-8f31.json` for a run's metadata.
///
/// The run id is the shared stem across a run's files on purpose: a log file
/// and the metadata describing it sort together in a directory listing and
/// name the same run without either being opened (`docs/DECISIONS.md` D-011).
pub fn run_file_name(
    unix_secs: i64,
    utc_offset_secs: i32,
    run_id: &str,
    extension: &str,
) -> Option<String> {
    if !is_filesystem_safe_component(run_id) || !is_filesystem_safe_component(extension) {
        return None;
    }
    let offset = FixedOffset::east_opt(utc_offset_secs)?;
    let stamp = to_local(unix_secs, &offset)?.format("%Y-%m-%d_%H-%M-%S");
    Some(format!("{stamp}__run-{run_id}.{extension}"))
}

/// Full path of one run's log file: `logs/<session>/<YYYY-MM>/<file>`.
pub fn run_log_path(
    logs_root: &Path,
    session_id: &str,
    unix_secs: i64,
    utc_offset_secs: i32,
    run_id: &str,
) -> Option<PathBuf> {
    run_file_path(
        logs_root,
        session_id,
        unix_secs,
        utc_offset_secs,
        run_id,
        "log",
    )
}

/// Full path of one run's metadata file:
/// `metadata/<session>/<YYYY-MM>/<file>` (`docs/LOGGING.md` §6).
///
/// It sits beside the log file it describes, under the same session and month,
/// so "find the run, then find its log" needs no database (D-011).
pub fn run_metadata_path(
    metadata_root: &Path,
    session_id: &str,
    unix_secs: i64,
    utc_offset_secs: i32,
    run_id: &str,
) -> Option<PathBuf> {
    run_file_path(
        metadata_root,
        session_id,
        unix_secs,
        utc_offset_secs,
        run_id,
        "json",
    )
}

/// `<root>/<session_id>/<YYYY-MM>/<run file>` — the layout every per-run file
/// shares, whatever the root it lives under.
fn run_file_path(
    root: &Path,
    session_id: &str,
    unix_secs: i64,
    utc_offset_secs: i32,
    run_id: &str,
    extension: &str,
) -> Option<PathBuf> {
    let dir = session_log_dir(root, session_id, unix_secs, utc_offset_secs)?;
    let file = run_file_name(unix_secs, utc_offset_secs, run_id, extension)?;
    Some(dir.join(file))
}

/// The local UTC offset in seconds right now, including any daylight saving
/// bias the OS is currently applying.
///
/// The path helpers take the offset explicitly so their own tests are clock-
/// independent; this is the one place that asks the OS, and the one place that
/// can fail to. Windows answers from the current time-zone state, so a log
/// written in summer lands in the month the user's clock was actually showing
/// (`docs/DECISIONS.md` D-011).
///
/// Off Windows, and on the odd Windows failure, the answer is UTC: a log filed
/// an hour either side of midnight is a far smaller problem than a path that
/// cannot be built at all.
pub fn local_utc_offset_secs() -> i32 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Time::{
            GetTimeZoneInformation, TIME_ZONE_ID_INVALID, TIME_ZONE_INFORMATION,
        };

        /// `GetTimeZoneInformation`'s "the daylight rule is in force" answer.
        /// `windows-sys` generates only `TIME_ZONE_ID_INVALID`, so the other
        /// return values are named here as the Windows headers define them.
        const TIME_ZONE_ID_DAYLIGHT: u32 = 2;

        let mut info: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
        let state = unsafe { GetTimeZoneInformation(&mut info) };
        if state == TIME_ZONE_ID_INVALID {
            return 0;
        }
        // `Bias` is minutes *west* of UTC (UTC+8 is -480), so the sign flips
        // into the offset the path helpers take; the daylight bias applies
        // only while the OS reports the daylight rule as active.
        let mut minutes = info.Bias + info.StandardBias;
        if state == TIME_ZONE_ID_DAYLIGHT {
            minutes += info.DaylightBias;
        }
        -(minutes as i32) * 60
    }
    #[cfg(not(windows))]
    {
        0
    }
}

fn to_local(unix_secs: i64, offset: &FixedOffset) -> Option<DateTime<FixedOffset>> {
    let utc = DateTime::from_timestamp(unix_secs, 0)?;
    Some(utc.with_timezone(offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path_str(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    #[test]
    fn app_paths_lay_out_roaming_config_and_local_data() {
        let paths = AppPaths::new(Path::new("C:/cfg"), Path::new("D:/data"));
        assert_eq!(
            path_str(&paths.config_file),
            "C:/cfg/LocalConsoleHub/config.yaml"
        );
        assert_eq!(path_str(&paths.logs_dir), "D:/data/LocalConsoleHub/logs");
        assert_eq!(
            path_str(&paths.metadata_dir),
            "D:/data/LocalConsoleHub/metadata"
        );
        assert_eq!(path_str(&paths.cache_dir), "D:/data/LocalConsoleHub/cache");
    }

    #[test]
    fn session_log_dir_groups_by_session_then_month() {
        // 2026-09-24T01:30:15Z == 2026-09-24T09:30:15+08:00.
        let dir = session_log_dir(Path::new("logs"), "comfyui", 1_790_213_415, 8 * 3600)
            .expect("timestamp in range");
        assert_eq!(path_str(&dir), "logs/comfyui/2026-09");
    }

    #[test]
    fn run_log_filename_matches_the_logging_spec_shape() {
        let name = run_log_filename(1_790_213_415, 8 * 3600, "8f31").expect("timestamp in range");
        assert_eq!(name, "2026-09-24_09-30-15__run-8f31.log");
    }

    #[test]
    fn run_log_filename_rejects_unsafe_run_ids() {
        // run ids become file name components; refuse path separators etc.
        assert!(run_log_filename(0, 0, "../escape").is_none());
        assert!(run_log_filename(0, 0, "").is_none());
        assert!(run_log_filename(0, 0, "with space").is_none());
        assert!(run_log_filename(0, 0, "8f31").is_some());
    }

    #[test]
    fn run_log_path_composes_dir_and_file() {
        let path = run_log_path(Path::new("logs"), "st", 0, 0, "ab12").expect("valid inputs");
        assert_eq!(
            path_str(&path),
            "logs/st/1970-01/1970-01-01_00-00-00__run-ab12.log"
        );
    }

    /// A run's metadata file is the same path shape under `metadata/`, with
    /// the same run-tagged stem — that is what lets a person match a log to
    /// the run that produced it by reading the two directories (D-011).
    #[test]
    fn run_metadata_path_mirrors_the_log_path_shape() {
        let path = run_metadata_path(
            Path::new("meta"),
            "comfyui",
            1_790_213_415,
            8 * 3600,
            "8f31",
        )
        .expect("valid inputs");
        assert_eq!(
            path_str(&path),
            "meta/comfyui/2026-09/2026-09-24_09-30-15__run-8f31.json"
        );
    }

    #[test]
    fn run_metadata_path_rejects_unsafe_components() {
        assert!(run_metadata_path(Path::new("meta"), "../up", 0, 0, "ab12").is_none());
        assert!(run_metadata_path(Path::new("meta"), "st", 0, 0, "../escape").is_none());
    }

    /// The offset the OS reports must be one the path helpers accept, and on a
    /// machine set to UTC it must be exactly zero — otherwise the month a log
    /// lands in would shift for reasons unrelated to the clock.
    #[test]
    fn the_reported_local_offset_builds_a_log_path() {
        let offset = local_utc_offset_secs();
        assert!(
            (-18 * 3600..=18 * 3600).contains(&offset),
            "implausible local offset {offset}"
        );
        let path = run_log_path(Path::new("logs"), "st", 1_790_213_415, offset, "ab12");
        assert!(path.is_some(), "offset {offset} produced no log path");
    }

    /// A component that would be redirected to a Windows device — `nul`,
    /// `com1`, in any casing — names no directory this Hub can create, so it
    /// is refused before it becomes a log path that only fails at write time.
    #[test]
    fn a_windows_device_name_is_not_a_safe_component() {
        for reserved in ["nul", "NUL", "con", "Com1", "lpt3", "aux", "prn"] {
            assert!(
                !is_filesystem_safe_component(reserved),
                "{reserved} is a device name, not a directory name"
            );
        }
        // The rule is about the whole component, not a substring: ordinary ids
        // that merely contain these letters stay legal.
        for ordinary in ["nulled", "console", "comfyui", "st-1"] {
            assert!(
                is_filesystem_safe_component(ordinary),
                "{ordinary} is an ordinary id"
            );
        }
    }
}
