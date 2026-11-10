//! The Windows job object that owns one managed process tree.
//!
//! Both kinds of run the Hub hosts are trees rather than single processes: a
//! supervised service (T03) and an interactive terminal's shell (T02) each
//! start children of their own, and each promises that ending the run ends
//! those children too (`docs/DEVELOPMENT.md` §6, spec #59 decision 13). The
//! Win32 object behind that promise is the same one either way, so it lives
//! here — inside the layer the spec gives "stop / kill tree" to — instead of as
//! a second copy that could drift from this one on the one thing both promise.
//!
//! ## The process is created suspended
//!
//! A job owns what is assigned to it, and a process that is already running can
//! create descendants before `AssignProcessToJobObject` returns. Both backends
//! therefore create their process suspended and resume its primary thread only
//! once it is in the job; a process that cannot be assigned is terminated
//! rather than left running outside the tree (#55).
//!
//! ## The handle *is* the ownership token
//!
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` makes closing the last handle to a job
//! terminate whatever is still assigned to it, so [`Job`] is RAII: the tree
//! lives exactly as long as this token does.

#[cfg(test)]
use std::cell::Cell;
use std::mem::size_of;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

/// Processes one tree query asks for. A typical session's tree is far smaller;
/// this is the first guess, not a ceiling.
const INITIAL_TREE_PROCESSES: usize = 512;

/// The largest tree this layer will describe in one query.
///
/// A tree beyond this is not a tree the Hub can stop usefully, and an
/// unbounded retry loop against an object whose counter never settles is worse
/// than a clear failure. 65,536 ids is 512 KiB of the query buffer.
const MAX_TREE_PROCESSES: usize = 65_536;

#[cfg(test)]
thread_local! {
    static FAIL_START_STEP_FOR_TEST: Cell<Option<StartFailurePointForTest>> = const { Cell::new(None) };
}

/// The steps a start can be made to fail at, so the teardown each one owes is
/// observable rather than asserted about a code path no test can reach.
///
/// Shared by both backends because the steps are: each creates the job, assigns
/// a process that is not yet running, and resumes it. What differs is only
/// where each backend checks them, so what is here is the vocabulary and the
/// switch, not the sites.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartFailurePointForTest {
    JobCreation,
    JobAssignment,
    Resume,
}

/// Make the next start fail at `point`. Thread-local, so parallel tests cannot
/// inject into each other's spawns.
#[cfg(test)]
pub(crate) fn fail_start_step_for_test(point: StartFailurePointForTest) {
    FAIL_START_STEP_FOR_TEST.with(|fail| fail.set(Some(point)));
}

/// Whether this start is the one `fail_start_step_for_test` was aimed at.
///
/// Reading consumes the value: an injection that has been observed cannot be
/// seen by a later start on the same thread, which is what keeps it from
/// escaping the test that set it (tests sharing a thread under
/// `--test-threads=1` included).
#[cfg(test)]
pub(crate) fn fail_start_step(point: StartFailurePointForTest) -> bool {
    FAIL_START_STEP_FOR_TEST.with(|fail| {
        if fail.get() == Some(point) {
            fail.set(None);
            true
        } else {
            false
        }
    })
}

/// Handle to one run's job object.
///
/// Stored as an integer rather than as a `HANDLE` so the token stays `Send +
/// Sync` regardless of how `windows-sys` spells the type, and so nothing
/// outside the Windows backends names a raw handle.
#[derive(Debug)]
pub struct Job(usize);

impl Job {
    /// Create a job that terminates everything still in it when its last handle
    /// closes, and report what could not be configured as the Win32 cause.
    pub fn create() -> Result<Self, String> {
        Job::create_with(true)
    }

    /// Create a job that owns a run's tree **without** ending it when the last
    /// handle closes (#66).
    ///
    /// This is what makes a standalone-window application outlive the Hub that
    /// started it, and it is a property of the object from the moment it is
    /// created: spec #59 decision 12 requires that an entry nobody asked the
    /// Hub to manage is not merely *reported* as independent while sitting in a
    /// resource that dies with the supervising process.
    ///
    /// Ownership is unaffected. The job still knows exactly which processes
    /// belong to the run, so a stop can still end the tree and the tree can
    /// still be enumerated for window discovery — what it does not do is kill
    /// anything because a handle went away.
    pub fn create_independent() -> Result<Self, String> {
        Job::create_with(false)
    }

    fn create_with(kill_on_close: bool) -> Result<Self, String> {
        let created = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) } as usize;
        if created == 0 {
            return Err(last_error("CreateJobObjectW"));
        }
        let job = Job(created);

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = if kill_on_close {
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        } else {
            0
        };
        let sized = size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        let configured = unsafe {
            SetInformationJobObject(
                job.0 as _,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                sized,
            )
        };
        if configured == 0 {
            return Err(last_error("SetInformationJobObject"));
        }
        Ok(job)
    }

    /// Put a process into this job.
    ///
    /// `process` is a process handle. The caller must hand over a process that
    /// has not started executing user code yet: assigning one that is already
    /// running leaves the window in which it could have created descendants
    /// outside the tree.
    ///
    /// Assignment can fail — a process already in a job that forbids nesting is
    /// the usual reason — and the caller owes the process a teardown in that
    /// case, because it is not owned by anything here.
    pub fn assign(&self, process: usize) -> Result<(), String> {
        let assigned = unsafe { AssignProcessToJobObject(self.0 as _, process as _) };
        if assigned == 0 {
            return Err(last_error("AssignProcessToJobObject"));
        }
        Ok(())
    }

    /// Terminate everything still assigned to the job.
    ///
    /// Members exit with code 1; a caller distinguishes a forced stop from a
    /// natural exit through the report it gets back, never through the code.
    /// Terminating a job whose members have all exited already is a no-op that
    /// succeeds, so this is safe to repeat.
    pub fn terminate(&self) -> Result<(), String> {
        let terminated = unsafe { TerminateJobObject(self.0 as _, 1) };
        if terminated == 0 {
            return Err(last_error("TerminateJobObject"));
        }
        Ok(())
    }

    /// Processes currently assigned to the job, including the run's own
    /// process.
    ///
    /// A tree larger than [`INITIAL_TREE_PROCESSES`] is read by growing the
    /// query buffer to what the OS says it needs, up to
    /// [`MAX_TREE_PROCESSES`]. A buffer that is merely too small is not a
    /// failure: treating `ERROR_MORE_DATA` as one would make a large tree
    /// impossible to confirm ended — and the stop ladder asks this question
    /// precisely to confirm the tree is gone.
    pub fn pids(&self) -> Result<Vec<u32>, String> {
        let mut capacity = INITIAL_TREE_PROCESSES;
        loop {
            // The id list is variable-length: two `u32` counters followed by
            // the entries. Sizing the buffer in `usize` units keeps it aligned
            // for them.
            let mut buffer = vec![0usize; 1 + capacity];
            let mut returned_bytes: u32 = 0;
            let queried = unsafe {
                QueryInformationJobObject(
                    self.0 as _,
                    JobObjectBasicProcessIdList,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * size_of::<usize>()) as u32,
                    &mut returned_bytes,
                )
            };

            if queried != 0 {
                let list = unsafe { &*buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
                // An id list is variable-length: the OS reports how many
                // entries are valid.
                let listed = list.NumberOfProcessIdsInList as usize;
                let count = listed.min(capacity);
                let first = list.ProcessIdList.as_ptr();
                let ids = (0..count).map(|index| unsafe { *first.add(index) as u32 });
                return Ok(ids.collect());
            }

            let code = unsafe { GetLastError() };
            if !matches!(code, ERROR_MORE_DATA | ERROR_INSUFFICIENT_BUFFER) {
                return Err(last_error("QueryInformationJobObject"));
            }

            // The header is filled even when the list did not fit, so the
            // job's own count is the exact size to try next. A driver that
            // leaves it empty falls back to doubling.
            let reported = unsafe {
                let list = &*buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
                list.NumberOfAssignedProcesses as usize
            };
            let Some(next) = next_tree_capacity(capacity, reported) else {
                return Err(format!(
                    "the job lists more than {MAX_TREE_PROCESSES} processes (Win32 error {code} \
                     from QueryInformationJobObject)"
                ));
            };
            capacity = next;
        }
    }
}

/// The buffer size for another query, or `None` when the tree cannot fit.
///
/// The reported count is exact when the OS filled the header; doubling is
/// what guarantees progress when it did not (or when the tree grew between
/// the two queries).
fn next_tree_capacity(current: usize, reported: usize) -> Option<usize> {
    let next = reported
        .max(current.saturating_mul(2))
        .min(MAX_TREE_PROCESSES);
    (next > current).then_some(next)
}

impl Drop for Job {
    fn drop(&mut self) {
        // With the kill-on-close limit set — every job [`Job::create`] makes —
        // the token is RAII: a handle going away terminates whatever is still
        // assigned, so a run cannot outlive the thing that accounts for it.
        // A job from [`Job::create_independent`] has no such limit, so this is
        // only a handle going away.
        unsafe { CloseHandle(self.0 as _) };
    }
}

/// Describe the last Win32 failure as a bare cause — the raw error code and the
/// call it came from. The caller names the *operation* it wrapped this into, so
/// repeating the operation here would read as `X failed: X failed (...)`
/// (`docs/DEVELOPMENT.md` §9).
fn last_error(call: &str) -> String {
    let code = unsafe { GetLastError() };
    format!("Win32 error {code} from {call}")
}

#[cfg(test)]
mod tests {
    use super::{next_tree_capacity, MAX_TREE_PROCESSES};

    #[test]
    fn a_full_buffer_grows_to_what_the_os_reported() {
        assert_eq!(next_tree_capacity(512, 2_000), Some(2_000));
    }

    #[test]
    fn an_empty_report_still_doubles() {
        assert_eq!(next_tree_capacity(512, 0), Some(1_024));
    }

    #[test]
    fn growth_stops_at_the_ceiling() {
        assert_eq!(next_tree_capacity(32_768, 90_000), Some(MAX_TREE_PROCESSES));
        assert_eq!(next_tree_capacity(MAX_TREE_PROCESSES, 90_000), None);
    }
}
