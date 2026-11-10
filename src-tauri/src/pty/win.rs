//! Windows PTY backend — a direct ConPTY implementation over `windows-sys`.
//!
//! One pseudoconsole per session (`CreatePseudoConsole`), two anonymous pipes
//! around it, and the shell attached at process creation through the
//! `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` attribute list — the shape of the
//! documented ConPTY session, kept in this module so the PTY layer's public
//! surface never names a raw handle. Why direct ConPTY and not portable-pty is
//! recorded in `docs/DECISIONS.md` D-014.
//!
//! The conpty-facing ends of both pipes are closed as soon as the
//! pseudoconsole exists: the pseudoconsole holds what it needs, and keeping the
//! Hub's copies would hide EOF from a terminal that has ended. The Hub keeps
//! exactly the two ends a terminal host needs — the input write end (what
//! [`PtyBackend::write_input`] writes keystrokes into) and the output read end
//! (what the layer's reader thread renders from).
//!
//! ## The shell's tree, not just the shell
//!
//! The shell is created suspended and put into a job object before its first
//! instruction runs, so everything it starts is inside the tree from the start
//! (`super::super::process::tree`, `docs/DECISIONS.md` D-028). Closing the
//! terminal terminates that job — which is why the *console* is no longer what
//! a close relies on. Closing the pseudoconsole ends the console a process is
//! attached to, but a child that detached from it, or one left behind by a
//! shell that exited first, would outlive the session the user was told had
//! ended.
//!
//! Every operation here is a thin wrapper around one Win32 call, mirroring the
//! process layer's `win` backend.

#[cfg(test)]
use crate::process::tree::{fail_start_step, StartFailurePointForTest};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, SetConsoleCtrlHandler, COORD,
    HPCON,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, GetProcessId,
    InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

use crate::process::tree::Job;

/// The attribute that attaches the spawned process to the pseudoconsole.
/// Spelled here instead of imported so the code does not depend on which
/// windows-sys module carries the constant.
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;

/// Capability gate for the PTY layer: this backend can host terminals, so there
/// is nothing to refuse.
pub fn require_backend(_operation: &'static str) -> Result<(), super::PtyError> {
    Ok(())
}

/// One live ConPTY session: the pseudoconsole, the input write end, the shell's
/// process handle, and the job object that owns the shell's tree.
#[derive(Debug)]
pub struct PtyBackend {
    pid: u32,
    con: ConHandle,
    input: Mutex<File>,
    child: ChildHandle,
    /// The ownership token for the shell's tree. Dropping it terminates
    /// whatever the shell left behind, so it outlives the shell by design: a
    /// terminal whose shell exited is still a terminal whose children may not
    /// have (`docs/DECISIONS.md` D-028).
    job: Job,
}

impl PtyBackend {
    /// Start `spec`'s program attached to a new pseudoconsole, suspended, and
    /// take ownership of its process tree before it executes anything.
    ///
    /// On success the caller also receives the pty's output stream, which
    /// belongs to a reader thread of its own — the UI render thread must never
    /// be the only thread reading terminal output (`docs/DEVELOPMENT.md` §5).
    ///
    /// A shell the Hub cannot own is never left running: every failure after
    /// the process exists ends it here, because a process that was created
    /// outside a job is nobody else's to reclaim (#55).
    pub fn spawn(spec: &super::PtySpec) -> Result<(PtyBackend, OutputReader), String> {
        #[cfg(test)]
        if fail_start_step(StartFailurePointForTest::JobCreation) {
            // Shaped like the real failure it stands in for, so a test of this
            // path is a test of the message the user would actually see.
            return Err(ownership_failure(
                spec,
                "test-injected CreateJobObjectW failure",
            ));
        }

        // Before the process, not after: the job has to exist to be joined, and
        // the shell must not execute an instruction until it is in it.
        let job = Job::create().map_err(|reason| ownership_failure(spec, &reason))?;

        let (input_read, input_write) = pipe()?;
        let (output_read, output_write) = pipe()?;

        let size = COORD {
            X: spec.cols as i16,
            Y: spec.rows as i16,
        };
        // windows-sys 0.59 spells `HPCON` as `isize`, not a pointer: zero is
        // its null value.
        let mut con: HPCON = 0;
        let created = unsafe {
            CreatePseudoConsole(size, input_read.0 as _, output_write.0 as _, 0, &mut con)
        };
        if created != 0 {
            return Err(format!(
                "CreatePseudoConsole at {}x{} failed: HRESULT {created:#010x}",
                spec.cols, spec.rows
            ));
        }
        // The pseudoconsole duplicated what it needs; the Hub's copies of the
        // conpty-facing ends must go away now, or a terminal that has ended
        // could never be observed as ended on these pipes.
        drop(input_read);
        drop(output_write);
        let con = ConHandle(con as usize);

        let mut attributes = AttributeList::new(1)?;
        attributes.set_pseudoconsole(&con)?;

        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        // No handles are inherited — the pseudoconsole attribute carries the
        // console. Naming the stdio handles invalid stops the child from
        // inheriting the Hub's own redirected handles instead of the pty.
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
        startup.lpAttributeList = attributes.as_mut_ptr();

        let mut command = wide_command_line(&spec.program, &spec.args);
        let environment = environment_block();
        let cwd: Vec<u16> = spec
            .cwd
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // A terminal that inherits the "Ctrl+C is ignored" flag would take the
        // pty's `0x03` byte as a `CTRL_C_EVENT` and discard it, so a user could
        // not stop anything they started. It is inherited at process creation,
        // which is why this is cleared here rather than left to the session.
        clear_inherited_ctrl_c_ignore();

        let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let started = unsafe {
            CreateProcessW(
                // The application name is null: the command line names the
                // program, which is what makes a bare `powershell.exe`
                // resolvable through PATH.
                std::ptr::null(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                // Suspended: job membership is established before the shell's
                // first instruction, so nothing it starts can begin outside the
                // tree (#55).
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
                environment.as_ptr().cast(),
                cwd.as_ptr(),
                std::ptr::addr_of_mut!(startup.StartupInfo).cast(),
                &mut process,
            )
        };
        if started == 0 {
            let failure = last_error("CreateProcessW");
            return Err(format!(
                "CreateProcessW failed for `{}` in `{}`: {failure}",
                spec.program.display(),
                spec.cwd.display()
            ));
        }

        // The process handle is this session's anchor; taking the `ChildHandle`
        // now means every failure below closes it by dropping, instead of each
        // path remembering to.
        let child = ChildHandle(Arc::new(RawChild(process.hProcess as usize)));
        let thread = process.hThread as usize;

        let owned = join_tree(&job, process.hProcess as usize, thread);
        // The primary thread handle is used exactly once, by the resume above.
        unsafe { CloseHandle(thread as _) };
        if let Err(reason) = owned {
            // The shell is in no job (assignment failed) or in a job that is
            // about to be dropped (resume failed); either way nothing else can
            // reclaim it, so this start ends it and returns the cause.
            unsafe { TerminateProcess(process.hProcess, 1) };
            return Err(ownership_failure(spec, &reason));
        }

        let pid = unsafe { GetProcessId(process.hProcess) };
        if pid == 0 {
            // The job is already the shell's owner; dropping it below ends the
            // shell, and the message says which identity could not be read.
            return Err(format!(
                "{} for the shell of `{}` in `{}`",
                last_error("GetProcessId"),
                spec.program.display(),
                spec.cwd.display()
            ));
        }

        let backend = PtyBackend {
            pid,
            con,
            input: Mutex::new(input_write.into_file()),
            child,
            job,
        };
        let reader = OutputReader {
            file: output_read.into_file(),
        };
        Ok((backend, reader))
    }

    /// The shell's process id.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// A shareable handle to the shell process, for exit observation.
    pub fn child(&self) -> ChildHandle {
        self.child.clone()
    }

    /// Forward terminal input (keystrokes) into the pty.
    pub fn write_input(&self, bytes: &[u8]) -> io::Result<()> {
        // Poisoning tolerance, same stance as the layer's `lock`: a panic on
        // one path must not turn every later keystroke into a second panic.
        self.input
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .write_all(bytes)
    }

    /// Resize the pseudoconsole. New dimensions reach the shell with the next
    /// console query — nothing is re-flowed by this layer.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        let size = COORD {
            X: cols as i16,
            Y: rows as i16,
        };
        let resized = unsafe { ResizePseudoConsole(self.con.0 as _, size) };
        if resized != 0 {
            return Err(format!(
                "ResizePseudoConsole to {cols}x{rows} failed: HRESULT {resized:#010x}"
            ));
        }
        Ok(())
    }

    /// Terminate the terminal's process tree: the shell and everything it
    /// started.
    ///
    /// Terminating the job rather than the shell's own process is what makes a
    /// close a close of the *session*. A shell that handed a child off and
    /// exited leaves that child assigned to this job, and terminating a process
    /// that is already gone would end nothing; `TerminateJobObject` covers both
    /// because the shell is in the job too, so there is one path rather than
    /// one per shape of ending (spec #59 decision 13).
    pub fn terminate(&self) -> Result<(), String> {
        self.job.terminate()
    }

    /// Pids still assigned to the terminal's job, the shell included. An empty
    /// list means nothing of the terminal is left.
    pub fn tree_pids(&self) -> Result<Vec<u32>, String> {
        self.job.pids()
    }
}

/// The sentence every ownership failure in [`PtyBackend::spawn`] returns: which
/// program, in which directory, and what the Hub could not establish for it.
///
/// The PTY layer's own [`super::PtyError::Spawn`] already names the terminal
/// and its geometry, so the part left to say here is the process tree the start
/// owed and did not get (`docs/DEVELOPMENT.md` §9).
fn ownership_failure(spec: &super::PtySpec, reason: &str) -> String {
    format!(
        "could not take ownership of the process tree of `{}` in `{}`: {reason}",
        spec.program.display(),
        spec.cwd.display()
    )
}

/// Put the suspended shell into its job, then let it run.
///
/// Two steps and one teardown: a shell the Hub created that is in no job, or
/// that cannot be resumed, is a shell nothing can account for, so the caller
/// ends it rather than returning it.
fn join_tree(job: &Job, process: usize, thread: usize) -> Result<(), String> {
    #[cfg(test)]
    if fail_start_step(StartFailurePointForTest::JobAssignment) {
        return Err("test-injected AssignProcessToJobObject failure".to_owned());
    }
    job.assign(process)?;
    resume_primary_thread(thread)
}

/// Let a suspended process run, now that it is in its job.
///
/// `CREATE_SUSPENDED` adds exactly one suspend count. A different count means
/// another actor changed the thread, and the start fails closed — the job's
/// kill-on-close limit then tears the process down rather than the Hub resuming
/// a thread on a guess (the same rule the service backend applies).
fn resume_primary_thread(thread: usize) -> Result<(), String> {
    #[cfg(test)]
    if fail_start_step(StartFailurePointForTest::Resume) {
        return Err("test-injected ResumeThread failure".to_owned());
    }

    let previous = unsafe { ResumeThread(thread as _) };
    if previous == u32::MAX {
        return Err(last_error("ResumeThread"));
    }
    if previous != 1 {
        return Err(format!(
            "unexpected initial-thread suspend count: {previous}"
        ));
    }
    Ok(())
}

/// The pty's output stream, handed to the PTY layer's reader thread.
#[derive(Debug)]
pub struct OutputReader {
    file: File,
}

impl OutputReader {
    /// One blocking read of rendered terminal output. `Ok(0)` is EOF: the
    /// pseudoconsole has closed and the stream is over.
    pub fn read_chunk(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

/// Handle to the shell process started on the pty, shareable between the
/// session handle and the exit watcher thread.
#[derive(Debug, Clone)]
pub struct ChildHandle(Arc<RawChild>);

/// The raw process handle, closed when the last sharer goes away.
#[derive(Debug)]
struct RawChild(usize);

impl Drop for RawChild {
    fn drop(&mut self) {
        // Closing the process handle neither terminates the shell nor keeps it
        // alive; it only ends this session's ability to observe it.
        unsafe { CloseHandle(self.0 as _) };
    }
}

impl ChildHandle {
    /// Block until the shell's process object is signalled — its exit. Costs no
    /// CPU while the shell is alive (D-009 — no per-session polling).
    pub fn wait_signaled(&self) {
        let waited = unsafe { WaitForSingleObject(self.0 .0 as _, INFINITE) };
        debug_assert_eq!(waited, WAIT_OBJECT_0, "WaitForSingleObject failed");
    }

    /// The exit code once the shell has ended; `None` while it is still
    /// running.
    pub fn exit_code(&self) -> Option<u32> {
        let mut code: u32 = 0;
        let read = unsafe { GetExitCodeProcess(self.0 .0 as _, &mut code) };
        // An openable process object outlives the process, so `STILL_ACTIVE` is
        // how a live one answers — the same distinction the process layer
        // makes in its test helper.
        (read != 0 && code != STILL_ACTIVE as u32).then_some(code)
    }
}

/// The pseudoconsole handle. Dropping it closes the pseudoconsole, which ends
/// the console every process of the session lives on.
#[derive(Debug)]
struct ConHandle(usize);

impl Drop for ConHandle {
    fn drop(&mut self) {
        unsafe { ClosePseudoConsole(self.0 as _) };
    }
}

/// Clear the "Ctrl+C is ignored" flag this process was started with.
///
/// `SetConsoleCtrlHandler(NULL, TRUE)` is inherited by every child a process
/// creates, so a Hub launched by a service, an updater or any launcher that
/// ignores Ctrl+C would hand the flag to each shell it hosts. The Hub is a GUI
/// application with no console of its own to keep out of harm's way, so it
/// drops the flag before every spawn; a process that never had it takes the
/// no-op path.
fn clear_inherited_ctrl_c_ignore() {
    // A null handler with `FALSE` is the documented way to turn the flag back
    // off, and the flag is process-wide state rather than a resource this layer
    // owns — there is no failure a caller could act on.
    unsafe { SetConsoleCtrlHandler(None, 0) };
}

/// A freshly created anonymous pipe pair, as (read end, write end).
fn pipe() -> Result<(KernelHandle, KernelHandle), String> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    let created = unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) };
    if created == 0 {
        return Err(last_error("CreatePipe"));
    }
    Ok((KernelHandle(read as usize), KernelHandle(write as usize)))
}

/// RAII ownership of one kernel `HANDLE`, stored as an integer so it stays
/// `Send + Sync` whichever way `windows-sys` spells the type.
#[derive(Debug)]
struct KernelHandle(usize);

impl KernelHandle {
    /// Wrap the handle in a [`File`], transferring close ownership to it.
    fn into_file(self) -> File {
        let raw = self.0;
        std::mem::forget(self);
        unsafe { File::from_raw_handle(raw as _) }
    }
}

impl Drop for KernelHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0 as _) };
    }
}

/// The `PROC_THREAD_ATTRIBUTE_LIST` buffer that attaches a spawned process to
/// the pseudoconsole. The Win32 calls maintain the contents; the vector only
/// owns the bytes, in `u64` units so the buffer keeps pointer alignment.
struct AttributeList(Vec<u64>);

impl AttributeList {
    fn new(capacity: u32) -> Result<Self, String> {
        // Sizing call: with a null list the API only reports the bytes needed.
        let mut needed: usize = 0;
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), capacity, 0, &mut needed)
        };
        let mut words = vec![0u64; needed.div_ceil(size_of::<u64>())];
        let initialized = unsafe {
            InitializeProcThreadAttributeList(words.as_mut_ptr().cast(), capacity, 0, &mut needed)
        };
        if initialized == 0 {
            return Err(last_error("InitializeProcThreadAttributeList"));
        }
        Ok(AttributeList(words))
    }

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }

    fn set_pseudoconsole(&mut self, con: &ConHandle) -> Result<(), String> {
        let updated = unsafe {
            UpdateProcThreadAttribute(
                self.as_mut_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                con.0 as _,
                size_of::<usize>(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if updated == 0 {
            return Err(last_error("UpdateProcThreadAttribute"));
        }
        Ok(())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}

/// The child's environment: the Hub's own environment with the terminal
/// identity set, so console applications can query what kind of terminal they
/// are talking to (`TERM` is a property of the terminal host, not a policy of
/// whoever started the session).
fn environment_block() -> Vec<u16> {
    environment_block_from(std::env::vars_os())
}

/// The block for `variables`, with this layer's own terminal identity in it.
///
/// `CreateProcessW` documents the block as sorted by variable name,
/// case-insensitively, and a program that merges or displays its environment
/// in arrival order notices when it is not. The injection replaces any
/// inherited `TERM` rather than sitting beside it: Windows variable names are
/// case-insensitive, so a `term` or `Term` from whoever launched the Hub would
/// otherwise be a second variable the console host could pick over this one.
fn environment_block_from(variables: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<u16> {
    let mut entries: Vec<(OsString, OsString)> = variables
        .into_iter()
        .filter(|(key, _)| !key.to_string_lossy().eq_ignore_ascii_case("TERM"))
        .collect();
    entries.push((OsString::from("TERM"), OsString::from("xterm-256color")));
    entries.sort_by_cached_key(|(key, _)| key.to_string_lossy().to_uppercase());

    let mut block = Vec::new();
    for (key, value) in entries {
        block.extend(key.encode_wide());
        block.push('=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0); // the block ends with an empty entry
    block
}

/// The `CreateProcessW` command line: quoted program followed by quoted
/// arguments, all NUL-terminated UTF-16.
fn wide_command_line(program: &Path, args: &[String]) -> Vec<u16> {
    let mut line = quote_argument(&program.to_string_lossy());
    for argument in args {
        line.push(' ');
        line.push_str(&quote_argument(argument));
    }
    line.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Quote one argv entry for a `CreateProcessW` command line, following the
/// MSVCRT rules the C runtime uses to split it back into argv.
///
/// An entry needs quoting when it is empty or contains a space, tab or quote.
/// Inside the quotes a run of `n` backslashes before a `"` becomes `2n + 1`
/// backslashes, so the quote stays a literal quote rather than the closing
/// one; a run at the end of the entry becomes `2n` so the closing quote is not
/// swallowed either. That second rule is what makes an ordinary path with a
/// trailing backslash (`D:\My Dir\`) survive a round trip.
fn quote_argument(argument: &str) -> String {
    let tricky = argument.is_empty()
        || argument
            .bytes()
            .any(|byte| matches!(byte, b' ' | b'\t' | b'"'));
    if !tricky {
        return argument.to_owned();
    }

    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat('\\').take(backslashes));
                quoted.push(character);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat('\\').take(backslashes * 2));
    quoted.push('"');
    quoted
}

/// Describe the last Win32 failure as a bare cause — the raw error code and the
/// call it came from. The PTY layer names the operation it wrapped this into,
/// so repeating the operation here would read as `X failed: X failed (...)`
/// (`docs/DEVELOPMENT.md` §9).
fn last_error(call: &str) -> String {
    let code = unsafe { GetLastError() };
    format!("Win32 error {code} from {call}")
}

#[cfg(test)]
mod tests {
    use super::{environment_block_from, quote_argument};
    use std::ffi::OsString;

    /// The cases that make the backslash rules observable: a path with a
    /// space and a trailing backslash, and the same path with a literal quote
    /// after it.
    #[test]
    fn quote_argument_follows_the_msvcrt_backslash_rules() {
        assert_eq!(quote_argument("hello"), "hello");
        assert_eq!(quote_argument("hello world"), r#""hello world""#);
        assert_eq!(quote_argument(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_argument(r"D:\My Dir\"), r#""D:\My Dir\\""#);
        assert_eq!(quote_argument(r#"D:\My Dir\""#), r#""D:\My Dir\\\"""#);
        assert_eq!(quote_argument(""), r#""""#);
        assert_eq!(quote_argument(r"C:\"), r"C:\");
        assert_eq!(quote_argument("a b\\\\"), r#""a b\\\\""#);
    }

    #[test]
    fn the_environment_block_replaces_term_case_insensitively_and_sorts() {
        let block = environment_block_from(vec![
            (OsString::from("term"), OsString::from("dumb")),
            (OsString::from("Path"), OsString::from(r"C:\Windows")),
            (OsString::from("TERM"), OsString::from("vt100")),
            (OsString::from("Alpha"), OsString::from("1")),
        ]);

        let entries: Vec<String> = block
            .split(|unit| *unit == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| String::from_utf16(entry).expect("the block is UTF-16"))
            .collect();
        assert_eq!(
            entries,
            vec![
                "Alpha=1".to_owned(),
                r"Path=C:\Windows".to_owned(),
                "TERM=xterm-256color".to_owned(),
            ],
            "one TERM, in the order CreateProcessW documents"
        );
        assert_eq!(
            block[block.len() - 2],
            0,
            "the block ends with an empty entry"
        );
    }
}
