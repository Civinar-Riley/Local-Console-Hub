//! An application the user started outside the Hub (#67, spec #59 decision 11).
//!
//! Clicking an entry whose application is *already running* must not start a
//! second copy. When the Hub started that copy itself this is
//! [`crate::session::core::SessionCore::activate`]'s business and it is
//! settled: the Hub holds the run. When the user started it by hand — from
//! Explorer, from a terminal, from another shortcut — the Hub has to *find* it,
//! and finding is a claim about evidence rather than about a name.
//!
//! ## What counts as evidence, and what does not
//!
//! Decision 11 lists three things a Hub-external instance can be recognised by:
//! a verifiable **executable target**, the **launch context**, and the
//! **process/window identity**. This module uses all three, in that order of
//! strength:
//!
//! 1. **Target.** The configuration's command is resolved the same way the
//!    display advice resolves it ([`crate::app::recommend`]), and a process
//!    counts only when the file it is running *is* that file — the full image
//!    path, or the same path appearing in its command line (which is how a
//!    batch launcher's host process names the script it is running).
//! 2. **Context.** The arguments the process was started with are compared with
//!    the ones the configuration passes. A configuration that claims arguments
//!    and a process that was started with different ones is the same program
//!    being used for something else, and that is not the entry the user clicked.
//! 3. **Identity.** A pid is not an identity: Windows hands the same number to
//!    a later process. Everything remembered about an instance carries the
//!    creation time Windows reports, and is re-checked before anything is done
//!    with it ([`crate::process::ProcessIdentity`]).
//!
//! What is deliberately **not** evidence: the file name on its own (two
//! applications are allowed to be called `python.exe`), a window title, and a
//! listening port. Those are the cheap matches decision 11 names and rules out,
//! and a candidate this module cannot confirm is reported as exactly that
//! rather than promoted to a match.
//!
//! ## Which entries this applies to
//!
//! Entries that keep the application's own window ([`DisplayMode::Window`]).
//! A Hub-hosted entry is displayed *inside* the Hub, from a process the Hub
//! started with pipes and a job object of its own; a process already running
//! outside has neither, so "associating" it would put a row on screen with
//! nothing behind it. What the user asks for when they click a window entry —
//! 唤起已有窗口 — is also only meaningful there.
//!
//! ## The three answers
//!
//! [`Outside`] has three cases and no fourth. Nothing found means the Hub
//! starts the application as it always did. One confirmed instance means the
//! Hub associates it and brings its window forward, and starts nothing. And
//! anything else — several candidates, or one it could not confirm — is
//! handed to the user to decide, because choosing between "this is the same
//! instance" and "this is a different one" is exactly the judgement a name or
//! a port cannot make for them.

use std::path::{Path, PathBuf};

use crate::config::{DisplayMode, SessionConfig, SessionType};
use crate::process::{self, ProcessIdentity, ProcessReading};
use crate::session::core::split_command;
use crate::window::{self, TopLevelWindow};

/// What the Hub found outside itself when the user asked to open an application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outside {
    /// Nothing that could be the application is running.
    None,
    /// The process table could not be read, so the Hub does not know whether
    /// anything is running.
    ///
    /// A case of its own rather than folded into [`Outside::None`], because the
    /// two lead to opposite actions: "nothing is there" starts the
    /// application, and "I could not look" must not (spec #59 decision 11 —
    /// 不盲目重复启动).
    Unreadable,
    /// Exactly one instance, and it is the one the configuration describes.
    Certain(ExternalInstance),
    /// Something might be the application, and the Hub will not guess which —
    /// or whether.
    Ambiguous(Ambiguity),
}

/// An instance of the application that the Hub did not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalInstance {
    /// The process, in the form that survives the number being reused.
    pub identity: ProcessIdentity,
    /// The image that is actually running, as it was read. A batch launcher's
    /// host process is what this names, which is why it is reported rather
    /// than assumed to be the configured program.
    pub image: Option<PathBuf>,
}

/// Why the Hub cannot decide, and what it did find.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ambiguity {
    /// A sentence for the user, saying what could not be established.
    pub reason: String,
    /// What the Hub found, so the choice is between real things rather than
    /// between "yes" and "no".
    pub candidates: Vec<Candidate>,
}

/// One process that might be the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub pid: u32,
    /// The creation time the user's answer will be checked against; `None` for
    /// a process the Hub could not open, which is also why such a candidate
    /// cannot be associated.
    pub created_at: Option<u64>,
    pub file_name: String,
    /// The full image path, when it could be read.
    pub image_path: Option<PathBuf>,
    /// The caption of the window it presents, when it has one.
    pub title: Option<String>,
    pub has_window: bool,
    /// Whether the Hub confirmed this process is the configured program.
    pub verified: bool,
    /// Whether the arguments it was started with are the configured ones.
    /// `None` when the configuration claims none.
    pub arguments_agree: Option<bool>,
    /// Why this one is uncertain, in the words the dialog shows.
    pub reason: String,
}

impl Candidate {
    /// Whether associating this candidate is something the Hub can do safely.
    ///
    /// Two things have to hold, and neither is about the user's patience. The
    /// process has to be **confirmed** as the configured program — a process
    /// this Hub could not inspect is not one it will agree to remember. And it
    /// has to carry a creation time, because an association without one would
    /// be a standing claim about a process number, which is what the identity
    /// exists to make impossible (decision 11).
    ///
    /// Unconfirmable candidates are still listed, because knowing that
    /// *something* is running is what keeps the Hub from silently starting a
    /// second copy; what they are not offered is the "associate" half of the
    /// choice.
    pub fn associable(&self) -> bool {
        self.verified && self.created_at.is_some()
    }
}

/// Whether this entry can be associated with an instance the Hub did not start.
pub fn applies_to(config: &SessionConfig) -> bool {
    config.session_type == SessionType::Service && config.display == DisplayMode::Window
}

/// Look for the configured application outside the Hub (#67).
pub fn find(config: &SessionConfig) -> Outside {
    let Some(target) = Target::of(config) else {
        // A command the Hub cannot split or resolve is one it cannot start
        // either, and that failure already has its own message. Inventing an
        // "ambiguous" out of it would replace it with a worse one.
        return Outside::None;
    };
    let Some(readings) = process::processes_named(&target.file_names) else {
        return Outside::Unreadable;
    };
    let mut found = classify(&target, &readings);
    decorate(&mut found, &readings);
    found
}

/// Re-verify a candidate the user picked, now.
///
/// The window answers with the pid and creation time it was shown, and both are
/// read again here: the table those came from is history, and the user may have
/// taken a moment to answer. Anything that no longer matches — a process that
/// has ended, or a number Windows has since handed to somebody else — is
/// refused rather than acted on (decision 11).
pub fn confirm(
    config: &SessionConfig,
    pid: u32,
    created_at: u64,
) -> Result<ExternalInstance, String> {
    let Some(target) = Target::of(config) else {
        return Err(
            "this entry's command cannot be resolved, so the Hub has nothing to confirm an \
             instance against"
                .to_owned(),
        );
    };
    let Some(readings) = process::processes_named(&target.file_names) else {
        return Err(
            "the process table could not be read, so the Hub cannot confirm the instance you \
             picked"
                .to_owned(),
        );
    };
    let Some(reading) = readings
        .iter()
        .find(|reading| reading.pid == pid && reading.created_at == Some(created_at))
    else {
        return Err(format!(
            "process {pid} is no longer the one that was found a moment ago; open the \
             application again and pick it once more"
        ));
    };

    match verdict(&target, reading) {
        Verdict::Match { .. } => Ok(ExternalInstance {
            identity: reading
                .identity()
                .expect("a reading that matched on identity carries one"),
            image: reading.image_path.clone(),
        }),
        Verdict::Unknown { reason } => Err(format!(
            "process {pid} could not be confirmed as the configured program: {reason}; the Hub \
             did not associate it"
        )),
        Verdict::No => Err(format!(
            "process {pid} is not running the program this entry names; the Hub did not \
             associate it"
        )),
    }
}

/// The launch method a configuration describes, resolved to something a process
/// table can be compared against.
struct Target {
    /// The file the configuration's program resolves to.
    program: PathBuf,
    /// The executable names the process table is narrowed by.
    ///
    /// Reading a process's image path means opening it, so the table is filtered
    /// by name first — the one place a name decides anything, and only what to
    /// *look at*.
    ///
    /// The program's own name plus, for a batch script, the interpreter Windows
    /// runs it with. A `.bat` is not a process: Explorer hands it to `cmd.exe`,
    /// and the process that is really running the configuration is a `cmd.exe`
    /// whose *command line* names the script. Filtering by the script's name
    /// alone never sees it, and the entry starts a second copy of something
    /// already running — which is the failure this module exists to prevent.
    /// (The #67 review caught exactly that: `classify` had the rule and the
    /// test called `classify` directly, so the filter in front of it was never
    /// exercised.)
    file_names: Vec<String>,
    /// The arguments the configuration passes to the program.
    args: Vec<String>,
}

impl Target {
    fn of(config: &SessionConfig) -> Option<Target> {
        let command = config.command.as_deref()?.trim();
        if command.is_empty() {
            return None;
        }
        let (token, args) = split_command(command).ok()?;
        let cwd = config
            .cwd
            .as_deref()
            .map(|cwd| cwd.to_string_lossy().into_owned());
        let program =
            crate::app::recommend::resolve_program(&token.to_string_lossy(), cwd.as_deref())?;

        let mut file_names = vec![program.file_name()?.to_string_lossy().into_owned()];
        if crate::app::recommend::is_batch(&program) {
            // The shell a batch file is handed to. Named once, here, because
            // this is where "what am I looking for in the process table?" is
            // decided.
            file_names.push("cmd.exe".to_owned());
        }
        Some(Target {
            program,
            file_names,
            args,
        })
    }
}

/// What one process row says about the configuration.
enum Verdict {
    /// This process runs the configured program.
    Match { arguments: Arguments },
    /// This process might, and the Hub could not find out.
    Unknown { reason: String },
    /// This process runs something else.
    No,
}

/// The argument half of the context check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arguments {
    /// The configuration passes none, so there is nothing claimed and nothing
    /// to disagree with.
    Unclaimed,
    /// The process was started with the arguments the configuration names.
    Agree,
    /// It was not — or the Hub could not read enough to say that it was.
    Disagree,
}

fn verdict(target: &Target, reading: &ProcessReading) -> Verdict {
    let confirms_target = match reading.image_path.as_deref() {
        Some(image) => same_path(image, &target.program) || names_program(reading, &target.program),
        // Nothing was read, so nothing about the target is established — and
        // "could not check" must not become "matched" or "did not match"
        // (decision 11).
        None => false,
    };

    if confirms_target {
        return Verdict::Match {
            arguments: arguments(target, reading),
        };
    }
    match reading.image_path {
        // Read, and it is a different file — which is exactly the "同名" case
        // decision 11 says is not evidence, so it is not a candidate either.
        // The one shape this cannot settle is the batch host: a `.bat` is not a
        // process, so the script the configuration names travels as an
        // *argument* of `cmd.exe`, and a shell started the way users commonly
        // spell it (`run.bat`, `cmd /c run.bat`) leaves that argument relative.
        // Which directory it resolves against is the host's own working
        // directory, and the command line does not carry it — `No` there would
        // start a second copy of an application that may well already be
        // running, so the question goes to the user instead of to a guess
        // (decision 11). An ordinary program's image *is* the program, so for
        // it a relative token proves nothing and the row stays `No`.
        Some(_) => {
            if is_batch_host(reading) && names_program_relatively(reading, &target.program) {
                Verdict::Unknown {
                    reason: "它的命令行用相对路径提到这个名字，Hub 无法确认它解析到哪个目录"
                        .to_owned(),
                }
            } else {
                Verdict::No
            }
        }
        None => Verdict::Unknown {
            reason: "无法读取这个进程的映像路径（通常是权限不足）".to_owned(),
        },
    }
}

/// Whether this reading is a batch interpreter: the one process whose command
/// line can be running a *script* rather than the program its image names.
fn is_batch_host(reading: &ProcessReading) -> bool {
    reading
        .image_path
        .as_deref()
        .and_then(Path::file_name)
        .is_some_and(|name| name.eq_ignore_ascii_case("cmd.exe"))
}

/// Whether the command line names `program` as one of its tokens.
///
/// This is how a batch launcher is recognised: the process is `cmd.exe`, and
/// the script the user configured is an argument. It is a *full path* match for
/// the same reason the image check is — a script with the same name in another
/// directory is a different script.
fn names_program(reading: &ProcessReading, program: &Path) -> bool {
    let Some(line) = reading.command_line.as_deref() else {
        return false;
    };
    tokens_of(line)
        .iter()
        .any(|token| same_path(Path::new(token), program))
}

/// Whether a batch host's command line names `program`'s file name only
/// relatively.
///
/// The complement of [`names_program`] for the batch shape: a shell started
/// the way users commonly spell it — `run.bat`, `.\run.bat`, `cmd /c run.bat`
/// from the configured directory — leaves the script's name relative on the
/// host's command line. Resolving that token needs the host process's working
/// directory, which the command line does not carry, so this is a question the
/// Hub asks rather than an identity it claims. [`is_batch_host`] gates it: an
/// ordinary program's command line carries its own image's name, which a bare
/// spelling proves nothing about.
fn names_program_relatively(reading: &ProcessReading, program: &Path) -> bool {
    let Some(name) = program.file_name() else {
        return false;
    };
    let Some(line) = reading.command_line.as_deref() else {
        return false;
    };
    tokens_of(line).iter().any(|token| {
        let token = Path::new(token);
        !token.is_absolute()
            && token
                .file_name()
                .is_some_and(|named| named.eq_ignore_ascii_case(name))
    })
}

/// Whether the process was started with the arguments the configuration passes.
fn arguments(target: &Target, reading: &ProcessReading) -> Arguments {
    if target.args.is_empty() {
        return Arguments::Unclaimed;
    }
    let Some(line) = reading.command_line.as_deref() else {
        return Arguments::Disagree;
    };
    let tokens = tokens_of(line);
    // The program is skipped by finding where the command line names it: the
    // first token for a program started directly, a later one for a script the
    // configuration handed to a host process. Locating it by *name* after the
    // target has already been confirmed is the whole of what that weaker
    // comparison is allowed to do.
    let Some(index) = tokens.iter().position(|token| {
        let token = Path::new(token);
        same_path(token, &target.program) || same_file_name(token, &target.program)
    }) else {
        return Arguments::Disagree;
    };
    if tokens[index + 1..] == target.args[..] {
        Arguments::Agree
    } else {
        Arguments::Disagree
    }
}

/// Split a command line the way a *start* splits the configured one.
///
/// A command line Windows reports and a command a user typed are the same
/// grammar, and using the one tokenizer this crate already trusts for
/// `command`/`shell` is what keeps "the arguments agree" from meaning something
/// slightly different on each side.
fn tokens_of(line: &str) -> Vec<String> {
    match split_command(line) {
        Ok((program, args)) => {
            let mut tokens = vec![program.to_string_lossy().into_owned()];
            tokens.extend(args);
            tokens
        }
        Err(_) => Vec::new(),
    }
}

/// The decision, over readings the caller has already narrowed by name.
fn classify(target: &Target, readings: &[ProcessReading]) -> Outside {
    let mut matched = Vec::new();
    let mut unknown = Vec::new();
    for reading in readings {
        match verdict(target, reading) {
            Verdict::Match { arguments } => matched.push((reading, arguments)),
            Verdict::Unknown { reason } => unknown.push((reading, reason)),
            Verdict::No => {}
        }
    }

    // Exactly one instance, confirmed as the configured program, started the
    // way the configuration starts it, and carrying an identity the Hub can
    // check again later. Anything less is a question rather than a decision.
    if unknown.is_empty() {
        if let [(reading, arguments)] = matched[..] {
            if arguments != Arguments::Disagree {
                if let Some(identity) = reading.identity() {
                    return Outside::Certain(ExternalInstance {
                        identity,
                        image: reading.image_path.clone(),
                    });
                }
            }
        }
    }

    if matched.is_empty() && unknown.is_empty() {
        return Outside::None;
    }

    let mut candidates: Vec<Candidate> = matched
        .iter()
        .map(|(reading, arguments)| Candidate {
            pid: reading.pid,
            created_at: reading.created_at,
            file_name: reading.file_name.clone(),
            image_path: reading.image_path.clone(),
            title: None,
            has_window: false,
            verified: true,
            arguments_agree: match arguments {
                Arguments::Unclaimed => None,
                Arguments::Agree => Some(true),
                Arguments::Disagree => Some(false),
            },
            reason: if reading.created_at.is_none() {
                "它是同一个程序，但 Hub 读不到它的创建时间，无法安全地记住这个实例".to_owned()
            } else if *arguments == Arguments::Disagree {
                "它是同一个程序，但启动参数与配置不同".to_owned()
            } else {
                "还有别的进程在运行同一个程序".to_owned()
            },
        })
        .collect();
    candidates.extend(unknown.iter().map(|(reading, reason)| Candidate {
        pid: reading.pid,
        created_at: reading.created_at,
        file_name: reading.file_name.clone(),
        image_path: None,
        title: None,
        has_window: false,
        verified: false,
        arguments_agree: None,
        reason: reason.clone(),
    }));

    Outside::Ambiguous(Ambiguity {
        reason: reason_for(&candidates),
        candidates,
    })
}

/// Why the Hub is asking, said in terms of what it found.
///
/// Read off the candidates rather than counted on the way in, so the sentence
/// and the list under it are the same facts: a dialog that explained one thing
/// and listed another would be worse than no explanation.
fn reason_for(candidates: &[Candidate]) -> String {
    let confirmed = candidates
        .iter()
        .filter(|candidate| candidate.verified)
        .count();
    let unreadable = candidates
        .iter()
        .filter(|candidate| !candidate.verified)
        .count();
    let different_arguments = candidates
        .iter()
        .any(|candidate| candidate.arguments_agree == Some(false));
    let unidentifiable = candidates
        .iter()
        .any(|candidate| candidate.verified && candidate.created_at.is_none());

    let mut parts = Vec::new();
    if confirmed > 1 {
        parts.push(format!(
            "有 {confirmed} 个进程都在运行这条配置指定的程序，Hub 无法确定哪一个才是你要打开的"
        ));
    } else if different_arguments {
        parts.push(
            "有一个进程在运行这条配置指定的程序，但它启动时用的参数与配置不同，Hub 无法确认\
             它就是这条配置要启动的那个"
                .to_owned(),
        );
    } else if unidentifiable {
        parts.push(
            "有一个进程在运行这条配置指定的程序，但 Hub 读不到它的创建时间，无法安全地记住\
             这个关联"
                .to_owned(),
        );
    }
    if unreadable > 0 {
        parts.push(format!(
            "另有 {unreadable} 个同名进程，Hub 无法确认它们是不是同一个程序"
        ));
    }
    if parts.is_empty() {
        // Unreachable: the caller only asks when there is something to ask
        // about. A reason is still owed rather than an empty sentence.
        return "Hub 无法确认正在运行的是不是这条配置的应用。".to_owned();
    }
    format!("{}。", parts.join("；"))
}

/// Give every candidate its window, so the choice is between things the user
/// can recognise.
///
/// A window is never what *finds* a candidate — every pid here comes from a
/// confirmed program — so this only makes the list readable, and a candidate
/// without one is still a candidate.
fn decorate(outside: &mut Outside, readings: &[ProcessReading]) {
    let Outside::Ambiguous(ambiguity) = outside else {
        return;
    };
    for candidate in &mut ambiguity.candidates {
        let mut pids = vec![candidate.pid];
        pids.extend(process::descendants(candidate.pid));
        // The console lookup is by number, so it is only made while the number
        // still answers for the process the candidate was read from.
        let current = readings
            .iter()
            .find(|reading| reading.pid == candidate.pid)
            .and_then(ProcessReading::identity)
            .is_some_and(|identity| identity.matches());
        let processes = window::Processes {
            pids,
            lead: candidate.pid,
            lead_is_current: current,
        };
        if let Some(window) = window::application_window(&processes) {
            candidate.title = titled(&window);
            candidate.has_window = true;
        }
    }
}

/// The window's caption, or `None` for a window without one.
fn titled(window: &TopLevelWindow) -> Option<String> {
    (!window.title.is_empty()).then(|| window.title.clone())
}

/// Whether two paths name the same file.
///
/// Case-insensitive and separator-insensitive, because Windows is: `D:/Tools/x`
/// and `d:\tools\x` are one file, and a configuration that spells it the second
/// way must still recognise a process started with the first.
fn same_path(left: &Path, right: &Path) -> bool {
    normalized(left) == normalized(right)
}

/// Whether two paths share a file name, for locating a program inside a command
/// line whose first token may be a bare name.
fn same_file_name(left: &Path, right: &Path) -> bool {
    match (left.file_name(), right.file_name()) {
        (Some(left), Some(right)) => {
            left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
        }
        _ => false,
    }
}

fn normalized(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\").to_lowercase();
    text.trim_end_matches('\\').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{validate_entry, RawSessionConfig};

    /// A reading the way `process::processes_named` would report one.
    fn reading(pid: u32, image: Option<&str>, line: Option<&str>) -> ProcessReading {
        ProcessReading {
            pid,
            created_at: Some(1_000 + pid as u64),
            file_name: image
                .map(|path| {
                    Path::new(path)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .unwrap_or_else(|| "python.exe".to_owned()),
            image_path: image.map(PathBuf::from),
            command_line: line.map(str::to_owned),
        }
    }

    fn target(program: &str, args: &[&str]) -> Target {
        let program = PathBuf::from(program);
        // The same names `Target::of` would filter the table by, so a
        // synthetic case cannot pass through a filter the product applies.
        let mut file_names = vec![program
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()];
        if crate::app::recommend::is_batch(&program) {
            file_names.push("cmd.exe".to_owned());
        }
        Target {
            program,
            file_names,
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    fn certain(outside: &Outside) -> &ExternalInstance {
        match outside {
            Outside::Certain(instance) => instance,
            other => panic!("expected the one confirmed instance, got {other:?}"),
        }
    }

    fn ambiguous(outside: &Outside) -> &Ambiguity {
        match outside {
            Outside::Ambiguous(ambiguity) => ambiguity,
            other => panic!("expected a question, got {other:?}"),
        }
    }

    /// The ordinary case: the configuration's program is running, started its
    /// own way, and nobody else is using it.
    #[test]
    fn one_process_running_the_configured_program_is_the_instance() {
        let readings = [reading(
            42,
            Some("C:\\Tools\\python.exe"),
            Some("\"C:\\Tools\\python.exe\" main.py"),
        )];

        let found = classify(&target("C:\\Tools\\python.exe", &["main.py"]), &readings);

        assert_eq!(certain(&found).identity.pid(), 42);
    }

    /// The same file name somewhere else is a different program. This is the
    /// "同名不能单独证明" rule, and it is the one a name-based match gets wrong.
    #[test]
    fn a_same_named_program_in_another_directory_is_not_a_match() {
        let readings = [reading(
            42,
            Some("D:\\Other\\python.exe"),
            Some("\"D:\\Other\\python.exe\" main.py"),
        )];

        let found = classify(&target("C:\\Tools\\python.exe", &["main.py"]), &readings);

        assert_eq!(found, Outside::None);
    }

    /// Two instances of the same program is a question, not a choice the Hub
    /// may make on the user's behalf.
    #[test]
    fn two_instances_of_the_program_ask_the_user() {
        let readings = [
            reading(
                42,
                Some("C:\\Tools\\python.exe"),
                Some("python.exe main.py"),
            ),
            reading(
                43,
                Some("C:\\Tools\\python.exe"),
                Some("python.exe main.py"),
            ),
        ];

        let found = classify(&target("C:\\Tools\\python.exe", &["main.py"]), &readings);

        let ambiguity = ambiguous(&found);
        assert_eq!(ambiguity.candidates.len(), 2);
        assert!(
            ambiguity.reason.contains("2 个进程"),
            "{}",
            ambiguity.reason
        );
        assert!(ambiguity.candidates.iter().all(Candidate::associable));
    }

    /// The same program started for something else is not the entry's
    /// instance: the launch context is part of what identifies an application
    /// (decision 11).
    #[test]
    fn a_process_started_with_different_arguments_is_asked_about() {
        let readings = [reading(
            42,
            Some("C:\\Tools\\python.exe"),
            Some("python.exe train.py"),
        )];

        let found = classify(&target("C:\\Tools\\python.exe", &["main.py"]), &readings);

        let ambiguity = ambiguous(&found);
        assert_eq!(ambiguity.candidates.len(), 1);
        assert_eq!(ambiguity.candidates[0].arguments_agree, Some(false));
        assert!(ambiguity.candidates[0].verified);
        assert!(
            ambiguity.candidates[0].associable(),
            "the user may still know it is the same application"
        );
        assert!(
            ambiguity.reason.contains("参数与配置不同"),
            "{}",
            ambiguity.reason
        );
    }

    /// A configuration that claims no arguments has nothing to disagree with:
    /// the program is the whole of what it says.
    #[test]
    fn a_configuration_without_arguments_accepts_the_program_alone() {
        let readings = [reading(
            42,
            Some("C:\\Tools\\app.exe"),
            Some("\"C:\\Tools\\app.exe\" --whatever"),
        )];

        let found = classify(&target("C:\\Tools\\app.exe", &[]), &readings);

        assert_eq!(certain(&found).identity.pid(), 42);
    }

    /// A batch launcher is hosted by `cmd.exe`, so its image is not the
    /// script — the script is what the command line names, and that is the
    /// evidence (the 秋叶启动器 shape the spec calls out).
    #[test]
    fn a_script_named_by_a_host_process_is_recognised() {
        let readings = [reading(
            42,
            Some("C:\\Windows\\System32\\cmd.exe"),
            Some("\"C:\\Windows\\System32\\cmd.exe\" /d /c \"D:\\ComfyUI\\run_nvidia_gpu.bat\""),
        )];

        let found = classify(&target("D:\\ComfyUI\\run_nvidia_gpu.bat", &[]), &readings);

        assert_eq!(certain(&found).identity.pid(), 42);
    }

    /// The same script started the way a user types it — `run.bat` from the
    /// configured directory, so the host's command line names it *relatively* —
    /// is a question rather than a second copy: which directory that token
    /// resolves against is the host's own working directory, and the command
    /// line does not carry it. A guess here is exactly the silent duplicate
    /// start decision 11 forbids.
    #[test]
    fn a_script_named_relatively_by_a_host_process_is_asked_about() {
        let readings = [reading(
            42,
            Some("C:\\Windows\\System32\\cmd.exe"),
            Some("\"C:\\Windows\\System32\\cmd.exe\" /d /c run.bat"),
        )];

        let found = classify(&target("D:\\ComfyUI\\run.bat", &[]), &readings);

        let ambiguity = ambiguous(&found);
        assert_eq!(ambiguity.candidates.len(), 1);
        assert!(
            !ambiguity.candidates[0].associable(),
            "a relative token is not an identity the Hub can confirm"
        );
        assert!(
            ambiguity.candidates[0].reason.contains("相对路径"),
            "{}",
            ambiguity.candidates[0].reason
        );
    }

    /// And a host running a *differently named* script stays "not it": the name
    /// the command line carries is not this entry's at all.
    #[test]
    fn a_host_running_another_script_by_name_is_not_a_candidate() {
        let readings = [reading(
            42,
            Some("C:\\Windows\\System32\\cmd.exe"),
            Some("\"C:\\Windows\\System32\\cmd.exe\" /d /c other.bat"),
        )];

        let found = classify(&target("D:\\ComfyUI\\run.bat", &[]), &readings);

        assert_eq!(found, Outside::None);
    }

    /// A process the Hub may not inspect is "could not confirm", never "not
    /// it" — the difference between the two is the whole reason the Hub asks
    /// instead of starting a second copy.
    #[test]
    fn a_process_that_cannot_be_read_is_asked_about_and_not_associable() {
        let readings = [reading(42, None, None)];

        let found = classify(&target("C:\\Tools\\python.exe", &["main.py"]), &readings);

        let ambiguity = ambiguous(&found);
        assert_eq!(ambiguity.candidates.len(), 1);
        assert!(!ambiguity.candidates[0].verified);
        assert!(
            !ambiguity.candidates[0].associable(),
            "a process with no creation time has no identity to remember"
        );
        assert!(
            ambiguity.reason.contains("无法确认"),
            "{}",
            ambiguity.reason
        );
    }

    /// Nothing running is the ordinary answer, and it is not a question.
    #[test]
    fn nothing_found_is_not_a_question() {
        assert_eq!(
            classify(&target("C:\\Tools\\python.exe", &["main.py"]), &[]),
            Outside::None
        );
    }

    /// A confirmed instance and an unreadable one at the same time is still a
    /// question: the Hub cannot claim the confirmed one is the only one.
    #[test]
    fn an_unreadable_process_beside_a_confirmed_one_still_asks() {
        let readings = [
            reading(
                42,
                Some("C:\\Tools\\python.exe"),
                Some("python.exe main.py"),
            ),
            reading(43, None, None),
        ];

        let found = classify(&target("C:\\Tools\\python.exe", &["main.py"]), &readings);

        assert_eq!(ambiguous(&found).candidates.len(), 2);
    }

    /// Paths are compared the way Windows compares them.
    #[test]
    fn paths_match_across_case_and_separators() {
        assert!(same_path(
            Path::new("D:/Tools/App.exe"),
            Path::new("d:\\tools\\app.exe"),
        ));
        assert!(!same_path(
            Path::new("D:\\Tools\\App.exe"),
            Path::new("D:\\Tools\\Other.exe"),
        ));
    }

    /// Only an entry that keeps its own window can be associated, because only
    /// there is "唤起已有窗口" something the Hub can do (#67, decision 11).
    #[test]
    fn only_window_entries_are_associated() {
        let mut config = config_for();
        assert!(!applies_to(&config), "the default display is Hub-internal");

        config.display = DisplayMode::Window;
        assert!(applies_to(&config));

        config.session_type = SessionType::Terminal;
        assert!(!applies_to(&config), "a terminal has no window of its own");
    }

    /// A service entry that validates, built through the config layer so the
    /// fixture cannot drift from what a real one is.
    fn config_for() -> SessionConfig {
        let raw = RawSessionConfig {
            id: "comfyui".to_owned(),
            name: "ComfyUI".to_owned(),
            r#type: "service".to_owned(),
            cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
            command: Some("app.exe".to_owned()),
            url: None,
            port: None,
            purpose: None,
            close_impact: None,
            shell: None,
            initial_command: None,
            display: None,
            lifecycle: None,
            logging: None,
        };
        validate_entry(0, &raw).expect("the fixture validates")
    }

    /// The order of the three checks is what keeps a name from standing in for
    /// evidence: a *different* file that shares the name is not a candidate at
    /// all, so an unrelated same-named program can never be listed, associated
    /// or stopped (H11's 无关同名应用存活).
    #[test]
    fn an_unrelated_same_named_program_is_not_even_a_candidate() {
        let readings = [
            reading(
                7,
                Some("D:\\Other\\tool\\python.exe"),
                Some("python.exe -c x"),
            ),
            reading(
                8,
                Some("D:\\ComfyUI\\python.exe"),
                Some("python.exe main.py"),
            ),
        ];

        let found = classify(&target("D:\\ComfyUI\\python.exe", &["main.py"]), &readings);

        assert_eq!(certain(&found).identity.pid(), 8);
    }

    /// A verdict on a reading with no identity cannot be promoted to the
    /// confirmed instance, because there would be nothing to check later.
    #[test]
    fn a_match_without_a_creation_time_is_not_confirmed() {
        let mut readable = reading(42, Some("C:\\Tools\\app.exe"), None);
        readable.created_at = None;
        let readings = [readable];

        let found = classify(&target("C:\\Tools\\app.exe", &[]), &readings);

        assert_eq!(ambiguous(&found).candidates.len(), 1);
        assert!(!ambiguous(&found).candidates[0].associable());
    }
}

/// The half of #67 that a synthetic reading cannot prove: that the Hub can read
/// a real process table, and that what it reads is enough to recognise an
/// application it did not start.
///
/// These run real processes on Windows (the spec's own boundary for process
/// questions — see `docs/VERIFICATION.md`), which is why they are grouped: each
/// one starts a copy of a real executable under a name nothing else on the
/// machine uses, so "exactly one candidate" is a fact about this test rather
/// than about the machine it runs on.
#[cfg(all(test, windows))]
pub(crate) mod native_tests {
    use super::*;
    use crate::config::{validate_entry, LifecycleOwner, RawSessionConfig};
    use std::process::{Child, Command, Stdio};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// How long a fixture process lives: longer than any assertion, shorter
    /// than a forgotten one would leak for.
    const LIFETIME: &[&str] = &["-n", "120", "127.0.0.1"];

    /// A private copy of a real executable, under a name of its own.
    ///
    /// Shared with `app::activation`'s own suite, which pins what a *click*
    /// does with what this module finds: one fixture, so the two halves of #67
    /// are tested against the same shape of application.
    pub(crate) struct Fixture(PathBuf);

    impl Fixture {
        pub(crate) fn new(tag: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("the clock is after the epoch")
                .as_nanos();
            let directory = std::env::temp_dir()
                .join(format!("lch-external-{tag}-{}-{nonce}", std::process::id()));
            std::fs::create_dir_all(&directory).expect("the fixture directory is created");

            // A real Windows program, copied so its *file name* is unique:
            // `ping.exe` is on every Windows install and has a well-known
            // command line, and the copy is what makes any candidate found
            // below this test's own.
            let system = std::env::var_os("SystemRoot").expect("Windows names its own directory");
            let source = PathBuf::from(system).join("System32").join("ping.exe");
            let program = directory.join(format!("lch-ext-{tag}-{nonce}.exe"));
            std::fs::copy(&source, &program).expect("the fixture program is copied");
            Fixture(program)
        }

        /// Wait until `pid` is listed in the process table, so a test asks
        /// about a machine that has settled rather than one mid-churn.
        ///
        /// The process table is a reading of a *live* system: it is taken
        /// while processes are being created and destroyed, including by the
        /// rest of this test suite. The product takes that reading when the
        /// user clicks an entry — seconds after the application appeared — so
        /// what a test has to wait for is the reading, not the spawn.
        /// A running fixture process, already visible in the process table.
        ///
        /// The process is started again when it died *before it could be
        /// seen*. On a loaded Windows session that is how `STATUS_DLL_INIT_FAILED`
        /// (0xC0000142) presents — the machine could not initialise a new
        /// process, which is the condition D-035 (#82) describes and says
        /// nothing about the behaviour under test. Retrying the apparatus is
        /// not the same as tolerating a wrong answer: every assertion below
        /// still runs against a process the OS really has.
        pub(crate) fn start_listed(&self) -> KillOnDrop {
            start_listed(&self.program_name(), || self.start())
        }

        /// The file name of the copied program.
        pub(crate) fn program_name(&self) -> String {
            self.0
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        }

        /// A script beside the fixture program, for the shapes that are not the
        /// program itself (a batch file is run by a shell, not by the loader).
        pub(crate) fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self
                .0
                .parent()
                .expect("the fixture program has a directory")
                .join(name);
            std::fs::write(&path, contents).expect("the fixture script is written");
            path
        }

        /// A window entry whose command is `command`, run in the fixture's
        /// directory.
        pub(crate) fn config_for(&self, command: String) -> SessionConfig {
            let raw = RawSessionConfig {
                id: "fixture".to_owned(),
                name: "Fixture".to_owned(),
                r#type: "service".to_owned(),
                cwd: Some(
                    self.0
                        .parent()
                        .expect("the fixture program has a directory")
                        .to_string_lossy()
                        .into_owned(),
                ),
                command: Some(command),
                url: None,
                port: None,
                purpose: None,
                close_impact: None,
                shell: None,
                initial_command: None,
                display: Some(DisplayMode::Window),
                lifecycle: Some(LifecycleOwner::Managed),
                logging: None,
            };
            validate_entry(0, &raw).expect("the fixture entry validates")
        }

        pub(crate) fn start(&self) -> Child {
            Command::new(&self.0)
                .args(LIFETIME)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("the fixture process starts")
        }

        /// A window entry that starts exactly `start`'s command.
        ///
        /// Management is on so that a test which starts the Hub's own copy can
        /// end it again; nothing about the association search reads this.
        pub(crate) fn config(&self) -> SessionConfig {
            self.config_for(format!("\"{}\" {}", self.0.display(), LIFETIME.join(" ")))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(directory) = self.0.parent() {
                let _ = std::fs::remove_dir_all(directory);
            }
        }
    }

    /// A running process named `name`, started by `spawn`, once the process
    /// table lists it.
    ///
    /// The process table is a reading of a *live* system. The product takes
    /// that reading when the user clicks an entry — seconds after the
    /// application appeared — so what a test waits for is the reading, not the
    /// spawn. A process that dies at startup is started again (see
    /// `Fixture::start_listed`), and one that never appears within the attempt
    /// is a failure rather than something to paper over.
    pub(crate) fn start_listed(name: &str, mut spawn: impl FnMut() -> Child) -> KillOnDrop {
        for _ in 0..START_ATTEMPTS {
            let mut child = KillOnDrop(spawn());
            let pid = child.0.id();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                if process::processes_named(&[name.to_owned()])
                    .unwrap_or_default()
                    .iter()
                    .any(|reading| reading.pid == pid)
                {
                    return child;
                }
                if matches!(child.0.try_wait(), Ok(Some(_))) {
                    // The machine could not get it up, so try again.
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the process {pid} never appeared in the process table as `{name}`"
                );
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
        panic!("this machine could not start a process to look at");
    }

    /// How many times a fixture process is started before the machine is
    /// declared unable to run one.
    const START_ATTEMPTS: usize = 5;

    /// Kill a fixture process the test started outside any supervisor, so a
    /// failing assertion cannot leave a pinger behind.
    pub(crate) struct KillOnDrop(pub Child);

    impl KillOnDrop {
        /// The process id of the one it guards.
        pub(crate) fn pid(&self) -> u32 {
            self.0.id()
        }

        /// Whether it is still running, asked of this test's own handle rather
        /// than of anything the Hub thinks.
        pub(crate) fn is_alive(&mut self) -> bool {
            self.0
                .try_wait()
                .expect("the fixture process is waitable")
                .is_none()
        }
    }

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// The reliable case of H11: the application really is running, and the Hub
    /// finds it through the process table rather than through anything it
    /// remembers.
    #[test]
    fn a_real_process_is_found_by_its_program_and_arguments() {
        let fixture = Fixture::new("found");
        let child = fixture.start_listed();

        let found = find(&fixture.config());

        let instance = match &found {
            Outside::Certain(instance) => instance,
            other => panic!("expected the running instance, got {other:?}"),
        };
        assert_eq!(instance.identity.pid(), child.0.id());
        assert!(
            instance.image.is_some(),
            "the image path of a process this user may inspect is readable"
        );
        assert!(
            instance.identity.matches(),
            "the identity the Hub kept answers for the process it was read from"
        );
    }

    /// A batch script is run by a *shell*, so the process that appears in the
    /// table is `cmd.exe` and the script is an argument of it. This is the
    /// shape of the launcher the spec names (秋叶启动器), and the one the whole
    /// search used to miss: the table was filtered by the script's file name,
    /// no process is called that, and the answer was "nothing is running" — so
    /// the Hub started a second copy of an application already up. Caught by
    /// the #67 review, because the synthetic case had called `classify`
    /// directly and never went through the filter in front of it.
    #[test]
    fn a_batch_script_running_under_a_shell_is_found() {
        let fixture = Fixture::new("batch");
        let script = fixture.write(
            "stay.cmd",
            "@echo off
ping -n 120 127.0.0.1 > NUL
",
        );
        // The process the search must find is the shell, not the script.
        let child = start_listed("cmd.exe", || {
            Command::new("cmd.exe")
                .args(["/d", "/c"])
                .arg(&script)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("the shell starts")
        });

        let found = find(&fixture.config_for(script.to_string_lossy().into_owned()));

        let instance = match &found {
            Outside::Certain(instance) => instance,
            other => panic!("expected the running launcher, got {other:?}"),
        };
        assert_eq!(
            instance.identity.pid(),
            child.0.id(),
            "the instance is the shell that is running the script"
        );
    }

    /// Nothing running is the ordinary answer — and it is *not* a question, so
    /// the entry starts the application as it always did.
    #[test]
    fn nothing_running_is_no_instance_at_all() {
        let fixture = Fixture::new("absent");

        assert_eq!(find(&fixture.config()), Outside::None);
    }

    /// Two copies is a question, and both candidates are ones the user could
    /// answer with — the Hub does not pick the one that happens to be older.
    #[test]
    fn two_real_processes_are_a_question_with_two_candidates() {
        let fixture = Fixture::new("twice");
        let first = fixture.start_listed();
        let second = fixture.start_listed();

        let found = find(&fixture.config());

        let ambiguity = match &found {
            Outside::Ambiguous(ambiguity) => ambiguity,
            other => panic!("expected a question, got {other:?}"),
        };
        let pids: Vec<u32> = ambiguity.candidates.iter().map(|c| c.pid).collect();
        assert!(pids.contains(&first.0.id()), "{pids:?}");
        assert!(pids.contains(&second.0.id()), "{pids:?}");
        assert!(
            ambiguity.candidates.iter().all(Candidate::associable),
            "{:?}",
            ambiguity.candidates
        );
    }

    /// Answering the question is a read taken *now*: the identity the user was
    /// shown is what the Hub checks before associating anything, so a number
    /// that no longer matches is refused rather than acted on (decision 11).
    #[test]
    fn choosing_a_candidate_is_re_verified_against_its_creation_time() {
        let fixture = Fixture::new("confirm");
        let child = fixture.start_listed();
        let config = fixture.config();

        // What the window would have been shown: the pid and the creation time
        // of the one instance the Hub found.
        let found = find(&config);
        let instance = match &found {
            Outside::Certain(instance) => instance,
            other => panic!("expected the running instance, got {other:?}"),
        };
        let pid = instance.identity.pid();
        let created_at = instance.identity.created_at();

        let confirmed =
            confirm(&config, pid, created_at).expect("the running process is confirmed");
        assert_eq!(confirmed.identity.pid(), child.0.id());

        // The same pid with a creation time that is not its own is the shape of
        // "Windows reused the number": refused, and nothing is associated.
        let refused = confirm(&config, pid, created_at.wrapping_add(1))
            .expect_err("a pid whose creation time does not match is not confirmed");
        assert!(refused.contains("no longer the one"), "{refused}");
    }
}
