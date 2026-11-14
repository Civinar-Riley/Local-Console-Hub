//! Rotation and retention (`docs/LOGGING.md` §9).
//!
//! Logs must not grow without limit, and cleanup must not be a single
//! indiscriminate sweep: §9 requires it per session, "so one high-output
//! service cannot push out every other session's logs". That is why the policy
//! is a **per-session** budget rather than a total one — a noisy session hits
//! its own ceiling and stops deleting anything of its own, and no other
//! session's history is touched.
//!
//! ## Planning is separate from deleting
//!
//! [`cleanup_plan`] is a pure function over a list of files; [`enforce`] is the
//! part that walks the disk and unlinks. Keeping them apart is what makes the
//! rule testable without a filesystem, and it is what lets a future UI show
//! "this will delete 12 files / 40 MB" before anything is removed
//! (`docs/LOGGING.md` §10 lists "clean old logs" as a UI action, and a
//! destructive action needs to be able to say what it will do first).
//!
//! ## What cleanup will not touch
//!
//! Only files under the Hub's own logs root, and only `*.log` under a
//! `<session>/<month>/` directory. An application-owned log linked with
//! `source: external` lives wherever the application put it, is not under this
//! root, and is never a candidate — spec §15 forbids deleting another
//! application's logs, and D-005 is the reason they were linked rather than
//! copied in the first place.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use super::error::{io_error, LogError};

/// How long a session's logs are kept, and how much disk they may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Files last modified more than this many days ago are removed.
    /// `0` disables the age rule — it cannot mean "delete everything", which
    /// is never what a user asking for retention means.
    pub max_age_days: u32,
    /// Per session, the most bytes its log files may occupy.
    pub max_session_bytes: u64,
}

/// A month of logs from a few long-running services is a handful of megabytes;
/// this is the "you forgot about us" ceiling, not a normal operating limit.
pub const DEFAULT_RETENTION: RetentionPolicy = RetentionPolicy {
    max_age_days: 30,
    max_session_bytes: 256 * 1024 * 1024,
};

impl Default for RetentionPolicy {
    fn default() -> Self {
        DEFAULT_RETENTION
    }
}

/// One candidate file, as the planner sees it.
///
/// Carries its session explicitly rather than leaving the planner to parse it
/// out of the path: the grouping rule ("per session") is the policy, and it
/// should not depend on where a caller happened to put the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFile {
    pub session_id: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub modified: SystemTime,
}

/// What a cleanup did, or would do.
///
/// Serializable because it is the answer to a user action: the Logs tab shows
/// what a sweep removed (or, through [`cleanup_preview`], what it would remove), and
/// `failures` is how a log directory the Hub may not write to reaches the
/// person who can fix it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupReport {
    /// Files that were (or would be) removed, in the order they were chosen.
    pub removed: Vec<PathBuf>,
    /// Total size of those files.
    pub freed_bytes: u64,
    /// Files the OS refused to remove. Reported rather than swallowed: a log
    /// directory the user cannot clean is a problem worth knowing about.
    pub failures: Vec<LogError>,
}

impl CleanupReport {
    pub fn removed_count(&self) -> usize {
        self.removed.len()
    }
}

/// Which files a cleanup would remove, newest-first per session.
///
/// The rules, in the order they are applied:
///
/// 1. A file older than the age limit goes.
/// 2. Within a session, files are kept newest first until the byte budget is
///    exhausted; everything older than that goes.
/// 3. The newest file of a session is always kept, however large the session
///    has become — deleting the run a user just made to satisfy a byte count
///    would be the opposite of useful.
pub fn cleanup_plan(files: &[LogFile], policy: &RetentionPolicy, now: SystemTime) -> Vec<PathBuf> {
    let max_age = (policy.max_age_days > 0)
        .then(|| Duration::from_secs(u64::from(policy.max_age_days) * 86_400));

    let mut by_session: std::collections::BTreeMap<&str, Vec<&LogFile>> = Default::default();
    for file in files {
        by_session
            .entry(file.session_id.as_str())
            .or_default()
            .push(file);
    }

    let mut doomed = Vec::new();
    for (_, mut session_files) in by_session {
        session_files.sort_by_key(|file| std::cmp::Reverse(file.modified));

        let mut kept_bytes = 0u64;
        let mut overdue = false;
        for (index, file) in session_files.iter().enumerate() {
            // Rule 3 first: the newest entry is kept before either budget is
            // consulted, and it does not consume the byte budget it would have
            // been the only member of.
            if index == 0 {
                continue;
            }
            if let Some(max_age) = max_age {
                let age = now.duration_since(file.modified).unwrap_or(Duration::ZERO);
                if age > max_age {
                    overdue = true;
                    doomed.push(file.path.clone());
                    continue;
                }
            }
            if overdue {
                // An older file cannot be younger than one already past the
                // age limit, so once the limit is crossed the rest are gone
                // without another check.
                doomed.push(file.path.clone());
                continue;
            }
            if kept_bytes + file.bytes > policy.max_session_bytes {
                doomed.push(file.path.clone());
                continue;
            }
            kept_bytes += file.bytes;
        }
    }
    doomed
}

/// What a cleanup would remove, without removing anything.
///
/// The step a destructive action owes its user: `docs/LOGGING.md` §10 lists
/// "clean old logs" as a UI action, and a UI that cannot say how many files
/// and how many bytes a sweep would take *before* taking them is asking for
/// consent it has not described. Returns the same shape [`enforce`] does, so
/// the confirmation and the result are read by the same code.
///
/// `protected` names files the walk must never select. `docs/LOGGING.md` §9
/// promises an application-owned log is never deleted, and normally location
/// keeps that promise — an external log lives wherever its application put it,
/// not under the Hub's own root. A config is free to point one *into* that
/// tree, though, and the walk would then find it; this is the parameter that
/// makes the promise hold wherever the file sits.
pub fn cleanup_preview(
    logs_dir: &Path,
    session_id: Option<&str>,
    policy: &RetentionPolicy,
    now: SystemTime,
    protected: &[PathBuf],
) -> CleanupReport {
    let doomed = select(logs_dir, session_id, policy, now, protected);
    CleanupReport {
        freed_bytes: doomed.iter().map(|(_, bytes)| bytes).sum(),
        removed: doomed.into_iter().map(|(path, _)| path).collect(),
        failures: Vec::new(),
    }
}

/// Remove the files [`cleanup_plan`] selects for one session, or for every
/// session when `session_id` is `None`.
pub fn enforce(
    logs_dir: &Path,
    session_id: Option<&str>,
    policy: &RetentionPolicy,
    now: SystemTime,
    protected: &[PathBuf],
) -> CleanupReport {
    let mut report = CleanupReport::default();
    for (path, bytes) in select(logs_dir, session_id, policy, now, protected) {
        match fs::remove_file(&path) {
            Ok(()) => {
                report.freed_bytes += bytes;
                report.removed.push(path);
            }
            Err(error) => report
                .failures
                .push(io_error("removing a log file", &path, error)),
        }
    }
    report
}

/// The files a sweep selects, each with the size to report it by.
///
/// One walk feeds both [`cleanup_preview`] and [`enforce`], so what a user was told
/// would go and what goes cannot be two different sets.
fn select(
    logs_dir: &Path,
    session_id: Option<&str>,
    policy: &RetentionPolicy,
    now: SystemTime,
    protected: &[PathBuf],
) -> Vec<(PathBuf, u64)> {
    let files = scan(logs_dir, session_id, protected);
    let sizes: std::collections::BTreeMap<&Path, u64> = files
        .iter()
        .map(|file| (file.path.as_path(), file.bytes))
        .collect();

    cleanup_plan(&files, policy, now)
        .into_iter()
        .map(|path| {
            let bytes = sizes.get(path.as_path()).copied().unwrap_or(0);
            (path, bytes)
        })
        .collect()
}

/// Every Hub-written log file under `logs_dir`, optionally for one session.
///
/// `.log` is the only extension looked at, so the metadata files that live
/// beside them — and anything else under the root — cannot be swept up by
/// accident. An entry whose size or timestamp cannot be read is skipped rather
/// than guessed at: a file retention cannot measure is one it must not delete.
/// A `protected` path is skipped for the same reason in reverse: it was
/// measured, and it belongs to someone else.
fn scan(logs_dir: &Path, session_id: Option<&str>, protected: &[PathBuf]) -> Vec<LogFile> {
    let mut files = Vec::new();

    for found in super::layout::session_run_files(logs_dir, "log", session_id) {
        if protected
            .iter()
            .any(|path| names_same_file(path, &found.path))
        {
            continue;
        }
        let Ok(metadata) = fs::metadata(&found.path) else {
            continue;
        };
        files.push(LogFile {
            session_id: found.session_id,
            path: found.path,
            bytes: metadata.len(),
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    files
}

/// Whether two paths name the same file, matched the way the filesystem that
/// holds it does.
///
/// The two sides come from different places — the walked tree, and the
/// configuration's own spelling of the same location — and on Windows those
/// agree only case-insensitively, component by component: a path typed
/// `C:\Users\…` protects a file the walk reports as `c:\users\…` just the
/// same, and a separator typed `/` names the same file as `\`. A compare that
/// missed either would treat an application's own log as nobody's and sweep
/// it.
fn names_same_file(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        let lowered = |path: &Path| -> Vec<String> {
            path.components()
                .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
                .collect()
        };
        lowered(left) == lowered(right)
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::test_support::TempDir;

    fn policy() -> RetentionPolicy {
        RetentionPolicy {
            max_age_days: 30,
            max_session_bytes: 1_000,
        }
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    fn days_ago(days: u64) -> SystemTime {
        now() - Duration::from_secs(days * 86_400)
    }

    fn file(session: &str, name: &str, bytes: u64, age_days: u64) -> LogFile {
        LogFile {
            session_id: session.to_owned(),
            path: PathBuf::from(name),
            bytes,
            modified: days_ago(age_days),
        }
    }

    #[test]
    fn a_recent_session_within_its_budget_is_left_alone() {
        let files = vec![
            file("svc", "new.log", 100, 0),
            file("svc", "old.log", 100, 1),
        ];

        assert!(cleanup_plan(&files, &policy(), now()).is_empty());
    }

    /// §9: retention is per session. A session over its budget must not be able
    /// to push another session's logs out.
    #[test]
    fn one_session_exceeding_its_budget_does_not_delete_another_session() {
        let files = vec![
            file("noisy", "noisy-new.log", 900, 0),
            file("noisy", "noisy-old.log", 900, 1),
            file("quiet", "quiet.log", 900, 2),
        ];
        let policy = RetentionPolicy {
            max_age_days: 30,
            max_session_bytes: 800,
        };

        let doomed = cleanup_plan(&files, &policy, now());

        assert_eq!(doomed, vec![PathBuf::from("noisy-old.log")]);
    }

    #[test]
    fn files_older_than_the_age_limit_are_removed() {
        let files = vec![
            file("svc", "new.log", 10, 0),
            file("svc", "ancient.log", 10, 45),
        ];

        let doomed = cleanup_plan(&files, &policy(), now());

        assert_eq!(doomed, vec![PathBuf::from("ancient.log")]);
    }

    /// The newest run is never a candidate: "clean up old logs" must not delete
    /// the log the user is currently looking at.
    #[test]
    fn the_newest_file_of_a_session_is_always_kept() {
        let files = vec![file("svc", "only.log", 10_000_000, 999)];

        assert!(cleanup_plan(&files, &policy(), now()).is_empty());
    }

    /// The two rules compose: an ancient file goes for its age, and one that
    /// is young enough but pushes the session past its budget goes as well.
    #[test]
    fn an_ancient_file_is_removed_and_the_budget_still_applies() {
        let files = vec![
            file("svc", "new.log", 800, 0),
            file("svc", "big.log", 1_100, 10),
            file("svc", "ancient.log", 800, 60),
        ];

        let doomed = cleanup_plan(&files, &policy(), now());

        assert_eq!(
            doomed,
            vec![PathBuf::from("big.log"), PathBuf::from("ancient.log")]
        );
    }

    #[test]
    fn a_zero_day_limit_disables_the_age_rule_rather_than_deleting_everything() {
        let files = vec![
            file("svc", "new.log", 10, 0),
            file("svc", "old.log", 10, 400),
        ];
        let policy = RetentionPolicy {
            max_age_days: 0,
            max_session_bytes: 1_000,
        };

        assert!(cleanup_plan(&files, &policy, now()).is_empty());
    }

    /// Deletion really happens, and only to the files the plan named.
    ///
    /// The two files are written a moment apart on purpose: which one is the
    /// session's newest is what decides who survives a byte budget, and a test
    /// that left that to the filesystem's timestamp resolution would pass or
    /// fail at random.
    #[test]
    fn enforcing_removes_the_planned_files_and_reports_what_it_freed() {
        let dir = TempDir::new();
        let kept = dir.join("svc/2026-09/2026-09-24_09-30-15__run-aaaa.log");
        let doomed = dir.join("svc/2026-09/2026-09-24_08-30-15__run-bbbb.log");
        fs::create_dir_all(kept.parent().expect("a month folder")).expect("creatable");
        fs::write(&doomed, b"0123456789").expect("writable");
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&kept, b"0123456789").expect("writable");
        // The doomed file is over the byte budget, not over the age limit.
        let policy = RetentionPolicy {
            max_age_days: 30,
            max_session_bytes: 5,
        };

        let report = enforce(dir.path(), None, &policy, now(), &[]);

        assert_eq!(report.removed_count(), 1, "{:?}", report.removed);
        assert!(report.freed_bytes > 0);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(kept.exists(), "the newest run's log was deleted");
        assert!(!doomed.exists());
    }

    /// Only Hub-written `.log` files are candidates, and only under the logs
    /// root: an application-owned log linked with `source: external` lives
    /// elsewhere and must survive a cleanup (spec §15).
    #[test]
    fn scanning_ignores_anything_that_is_not_a_month_of_logs() {
        let dir = TempDir::new();
        let logs = dir.join("logs");
        let month = logs.join("svc/2026-09");
        fs::create_dir_all(&month).expect("creatable");
        fs::write(month.join("run-old.log"), b"0123456789").expect("writable");
        std::thread::sleep(Duration::from_millis(20));
        fs::write(month.join("run-new.log"), b"0123456789").expect("writable");
        fs::write(month.join("run.json"), b"x").expect("writable");
        fs::write(logs.join("svc/notes.txt"), b"x").expect("writable");
        let outside = dir.join("app/access.log");
        fs::create_dir_all(outside.parent().expect("a parent")).expect("creatable");
        fs::write(&outside, b"application-owned").expect("writable");

        let policy = RetentionPolicy {
            max_age_days: 30,
            max_session_bytes: 5,
        };
        let report = enforce(&logs, None, &policy, now(), &[]);

        assert_eq!(report.removed_count(), 1, "{:?}", report.removed);
        assert!(report.removed[0].ends_with("run-old.log"));
        assert!(outside.exists(), "an external log was touched");
        assert!(
            month.join("run.json").exists(),
            "a non-log file was touched"
        );
        assert!(logs.join("svc/notes.txt").exists());
    }

    #[test]
    fn cleaning_one_session_leaves_the_others_alone() {
        let dir = TempDir::new();
        let logs = dir.join("logs");
        for session in ["noisy", "quiet"] {
            let path = logs.join(format!("{session}/2026-09/run-old.log"));
            fs::create_dir_all(path.parent().expect("a month folder")).expect("creatable");
            fs::write(&path, b"0123456789").expect("writable");
            std::thread::sleep(Duration::from_millis(20));
            fs::write(path.with_file_name("run-new.log"), b"0123456789").expect("writable");
        }
        let policy = RetentionPolicy {
            max_age_days: 30,
            max_session_bytes: 5,
        };

        let report = enforce(&logs, Some("quiet"), &policy, now(), &[]);

        assert_eq!(report.removed_count(), 1);
        assert!(!logs.join("quiet/2026-09/run-old.log").exists());
        assert!(logs.join("noisy/2026-09/run-old.log").exists());
    }

    #[test]
    fn a_missing_logs_root_is_an_empty_cleanup_not_a_failure() {
        let dir = TempDir::new();

        let report = enforce(&dir.join("does-not-exist"), None, &policy(), now(), &[]);

        assert_eq!(report, CleanupReport::default());
    }

    /// A preview names what a sweep would take and takes nothing.
    ///
    /// This is the property a destructive action depends on: the user is shown
    /// the file count and the bytes before agreeing, and the disk is exactly as
    /// it was until they do.
    #[test]
    fn a_preview_reports_what_a_sweep_would_take_and_deletes_nothing() {
        let dir = TempDir::new();
        let kept = dir.join("svc/2026-09/run-new.log");
        let doomed = dir.join("svc/2026-09/run-old.log");
        fs::create_dir_all(kept.parent().expect("a month folder")).expect("creatable");
        fs::write(&doomed, b"0123456789").expect("writable");
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&kept, b"0123456789").expect("writable");
        let policy = RetentionPolicy {
            max_age_days: 30,
            max_session_bytes: 5,
        };

        let preview = cleanup_preview(dir.path(), None, &policy, now(), &[]);

        assert_eq!(preview.removed, vec![doomed.clone()]);
        assert_eq!(preview.freed_bytes, 10);
        assert!(preview.failures.is_empty());
        assert!(
            doomed.exists(),
            "a preview deleted the file it was only describing"
        );
        assert!(kept.exists());
    }

    /// What the user was told would go is what goes.
    ///
    /// The two share one walk on purpose; a preview computed by a second code
    /// path could promise a different set than the sweep removes.
    #[test]
    fn a_preview_and_the_sweep_that_follows_it_agree() {
        let dir = TempDir::new();
        for name in ["run-a.log", "run-b.log", "run-c.log"] {
            let path = dir.join("svc/2026-09").join(name);
            fs::create_dir_all(path.parent().expect("a month folder")).expect("creatable");
            fs::write(&path, b"0123456789").expect("writable");
            std::thread::sleep(Duration::from_millis(20));
        }
        let policy = RetentionPolicy {
            max_age_days: 30,
            max_session_bytes: 5,
        };

        let preview = cleanup_preview(dir.path(), None, &policy, now(), &[]);
        let swept = enforce(dir.path(), None, &policy, now(), &[]);

        assert_eq!(preview.removed, swept.removed);
        assert_eq!(preview.freed_bytes, swept.freed_bytes);
    }

    /// §9's promise, kept by construction rather than by location: a file the
    /// caller protects is not a candidate however old it is or wherever it sits.
    ///
    /// Three files, because a session's newest is kept whatever happens: the
    /// protected file is the *oldest*, so the only thing that can save it is
    /// being named as an application's own.
    #[test]
    fn a_protected_path_is_never_a_candidate() {
        let dir = TempDir::new();
        let month = dir.join("svc/2026-09");
        fs::create_dir_all(&month).expect("a month folder");
        let application_owned = month.join("app-access.log");
        let hub_stale = month.join("run-old.log");
        let hub_current = month.join("run-new.log");
        for path in [&application_owned, &hub_stale, &hub_current] {
            fs::write(path, b"0123456789").expect("writable");
        }
        age_by_days(&application_owned, 60);
        age_by_days(&hub_stale, 45);
        let policy = RetentionPolicy {
            max_age_days: 30,
            max_session_bytes: 5,
        };

        let protected = std::slice::from_ref(&application_owned);
        let preview = cleanup_preview(dir.path(), None, &policy, now(), protected);
        let swept = enforce(dir.path(), None, &policy, now(), protected);

        assert_eq!(preview.removed, vec![hub_stale.clone()]);
        assert_eq!(swept.removed, preview.removed);
        assert!(!hub_stale.exists(), "the Hub's own stale log was not taken");
        assert!(application_owned.exists(), "a protected path was deleted");
        assert!(hub_current.exists(), "the newest run's log was taken");
    }

    /// The protected path is the *configuration's* spelling of the file, the
    /// walked path is the tree's, and on Windows the two agree only
    /// case-insensitively — a casing difference must not turn an application's
    /// own log into nobody's.
    #[cfg(windows)]
    #[test]
    fn a_protected_path_is_matched_the_way_windows_matches_paths() {
        let dir = TempDir::new();
        let month = dir.join("svc/2026-09");
        fs::create_dir_all(&month).expect("a month folder");
        let application_owned = month.join("app-access.log");
        let hub_stale = month.join("run-old.log");
        let hub_current = month.join("run-new.log");
        for path in [&application_owned, &hub_stale, &hub_current] {
            fs::write(path, b"0123456789").expect("writable");
        }
        age_by_days(&application_owned, 60);
        age_by_days(&hub_stale, 45);
        let policy = RetentionPolicy {
            max_age_days: 30,
            max_session_bytes: 5,
        };

        // Same file, spelled differently — as a hand-written config would.
        let protected = vec![application_owned.to_string_lossy().to_uppercase().into()];
        let preview = cleanup_preview(dir.path(), None, &policy, now(), &protected);
        let swept = enforce(dir.path(), None, &policy, now(), &protected);

        assert_eq!(preview.removed, vec![hub_stale.clone()]);
        assert_eq!(swept.removed, preview.removed);
        assert!(
            application_owned.exists(),
            "the application's log was swept because the config spelled it differently"
        );
        assert!(hub_current.exists(), "the newest run's log was taken");
    }

    /// Move a file's modification time into the past, the way retention
    /// measures age.
    fn age_by_days(path: &Path, days: u64) {
        let when = SystemTime::now() - Duration::from_secs(days * 86_400);
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("the file is openable for write")
            .set_modified(when)
            .expect("the timestamp is settable");
    }

    /// A plan nobody has confirmed is not a sweep: a directory with nothing
    /// past keeping previews as empty rather than as an error.
    #[test]
    fn a_preview_of_a_session_within_its_budget_is_empty() {
        let dir = TempDir::new();
        let path = dir.join("svc/2026-09/run-only.log");
        fs::create_dir_all(path.parent().expect("a month folder")).expect("creatable");
        fs::write(&path, b"0123456789").expect("writable");

        let preview = cleanup_preview(dir.path(), Some("svc"), &policy(), now(), &[]);

        assert_eq!(preview, CleanupReport::default());
        assert!(path.exists());
    }
}
