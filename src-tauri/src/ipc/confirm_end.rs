//! End one occupying process after the user has confirmed that identity (#110).
//!
//! The window sends the pid and the creation time it just showed. This command
//! reads the creation time again and ends the process only when they are the
//! same process. A process that has exited, or a pid Windows has reused, is
//! left alone, and the result says nothing was ended.
//!
//! The command does not take a port or a process name. It does not start,
//! stop, or restart a session, and it does not adopt the process into one.

use serde::Serialize;
use tauri::State;

use crate::process::{self, ConfirmedEnd, ProcessIdentity};
use crate::session::core::SessionCore;

const NOTHING_ENDED: &str = "没有结束。进程已经退出，或这个 PID 已经是另一个进程。";
const NOTHING_ENDED_MANAGED: &str = "没有结束。这是受管会话的进程，这里不停止它。";
const ENDED: &str = "已结束刚才核对过的这个进程。";

/// What the confirmed end did.
///
/// `ended` is false when nothing was signalled. `message` then says that
/// nothing was ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmEndDto {
    pub ended: bool,
    pub message: String,
}

/// End the confirmed process tree, or explain that nothing was ended (#110).
///
/// Cancelling never calls this. Refresh, start, and restart do not either.
#[tauri::command(async)]
pub fn confirm_end_occupant(
    core: State<'_, SessionCore>,
    pid: u32,
    created_at: String,
) -> ConfirmEndDto {
    confirm_end_for(core.inner(), pid, &created_at)
}

fn confirm_end_for(core: &SessionCore, pid: u32, created_at: &str) -> ConfirmEndDto {
    let Some(created_at) = created_at.parse::<u64>().ok() else {
        return nothing(NOTHING_ENDED);
    };
    let identity = ProcessIdentity::recorded(pid, created_at);
    if !identity.matches() {
        return nothing(NOTHING_ENDED);
    }
    if occupies_managed_session(core, identity) {
        return nothing(NOTHING_ENDED_MANAGED);
    }
    match process::end_confirmed_tree(identity) {
        ConfirmedEnd::Ended => ConfirmEndDto {
            ended: true,
            message: ENDED.to_owned(),
        },
        ConfirmedEnd::NothingEnded => nothing(NOTHING_ENDED),
    }
}

/// The confirmed process, or one of its current descendants, is a managed
/// session's own process or a member of that session's tree.
///
/// Ending it here would stop that session outside the session's own stop.
fn occupies_managed_session(core: &SessionCore, identity: ProcessIdentity) -> bool {
    let sessions = core.listening_processes();
    if sessions.is_empty() {
        return false;
    }
    let mut named = vec![identity];
    for pid in process::descendants(identity.pid()) {
        if let Some(created) = process::creation_time_of(pid) {
            named.push(ProcessIdentity::recorded(pid, created));
        }
    }
    sessions.iter().any(|session| {
        named.iter().any(|candidate| {
            *candidate == session.identity
                || session
                    .tree
                    .as_ref()
                    .is_some_and(|tree| tree.iter().any(|member| member == candidate))
        })
    })
}

fn nothing(message: &str) -> ConfirmEndDto {
    ConfirmEndDto {
        ended: false,
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_end_serializes_the_refusal() {
        let value = serde_json::to_value(nothing(NOTHING_ENDED)).expect("serializes");
        assert_eq!(value["ended"], false);
        assert!(value["message"].as_str().unwrap().contains("没有结束"));
        assert!(value.get("created_at").is_none());
        assert!(value.get("port").is_none());
        assert!(value.get("processName").is_none());
    }

    #[cfg(windows)]
    mod windows {
        use super::*;
        use std::process::Stdio;

        use crate::config::{
            DisplayMode, EffectiveLogMode, EffectiveLogging, LifecycleOwner, LogSource,
            SessionConfig, SessionType,
        };
        use crate::process::{creation_time_of, ExternalProcess};
        use crate::session::state::SessionStatus;

        struct Sleeper(std::process::Child);

        impl Sleeper {
            fn spawn() -> Self {
                use std::os::windows::process::CommandExt;
                // `ping` can exit immediately on a runner that blocks it. The
                // pid would then be free for the next spawn, and a later
                // creation-time read would describe that new process.
                let child = std::process::Command::new("powershell.exe")
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        "Start-Sleep -Seconds 120",
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .creation_flags(0x0800_0000)
                    .spawn()
                    .expect("the test process starts");
                Sleeper(child)
            }

            fn pid(&self) -> u32 {
                self.0.id()
            }

            fn identity(&mut self) -> ProcessIdentity {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                loop {
                    let still_ours = self
                        .0
                        .try_wait()
                        .expect("the process is waitable")
                        .is_none();
                    if !still_ours {
                        panic!(
                            "pid {} exited before its creation time was read",
                            self.pid()
                        );
                    }
                    if let Some(created) = creation_time_of(self.pid()) {
                        let identity = ProcessIdentity::recorded(self.pid(), created);
                        if identity.matches() {
                            return identity;
                        }
                    }
                    if std::time::Instant::now() >= deadline {
                        panic!("creation time of {} was not readable", self.pid());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }

            fn alive(&mut self) -> bool {
                self.0
                    .try_wait()
                    .expect("the process is waitable")
                    .is_none()
            }
        }

        impl Drop for Sleeper {
            fn drop(&mut self) {
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &self.0.id().to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = self.0.wait();
            }
        }

        fn configured(id: &str) -> SessionConfig {
            SessionConfig {
                id: id.to_owned(),
                name: format!("Service {id}"),
                session_type: SessionType::Service,
                cwd: Some(
                    std::env::current_dir().expect("the test process has a working directory"),
                ),
                command: Some("cmd.exe /c exit 0".to_owned()),
                url: None,
                port: None,
                purpose: None,
                close_impact: None,
                shell: None,
                initial_command: None,
                display: DisplayMode::Internal,
                lifecycle: LifecycleOwner::Managed,
                logging: EffectiveLogging {
                    mode: EffectiveLogMode::Off,
                    source: LogSource::None,
                    external_path: None,
                },
            }
        }

        fn wait_dead(sleeper: &mut Sleeper) -> bool {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if !sleeper.alive() {
                    return true;
                }
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }

        #[test]
        fn confirm_end_ends_only_the_verified_process_and_leaves_sessions_alone() {
            let core = SessionCore::without_listener();
            core.register(configured("holder"))
                .expect("register holder");
            core.register(configured("next"))
                .expect("register the session that stays stopped");

            let mut target = Sleeper::spawn();
            let mut kept = Sleeper::spawn();
            let mut managed = Sleeper::spawn();
            assert!(
                target.alive() && kept.alive() && managed.alive(),
                "a sleeper exited before the confirm"
            );
            assert_ne!(target.pid(), kept.pid());
            assert_ne!(target.pid(), managed.pid());
            assert_ne!(kept.pid(), managed.pid());
            let managed_identity = managed.identity();
            let process =
                ExternalProcess::open(managed_identity).expect("open the managed process");
            core.adopt("holder", managed_identity, process)
                .expect("associate the process this test started");

            struct Release<'a>(&'a SessionCore);
            impl Drop for Release<'_> {
                fn drop(&mut self) {
                    let _ = self.0.release_adopted("holder");
                }
            }
            let _release = Release(&core);

            let before_next = core.snapshot("next").expect("next exists");
            assert_eq!(before_next.status, SessionStatus::Stopped);
            assert!(before_next.run_id.is_none());
            assert_eq!(
                core.snapshot("holder").expect("holder").status,
                SessionStatus::Running
            );

            let target_identity = target.identity();
            let kept_identity = kept.identity();
            let ended = confirm_end_for(
                &core,
                target.pid(),
                &target_identity.created_at().to_string(),
            );
            assert!(ended.ended, "{}", ended.message);
            assert!(!ended.message.contains("没有结束"));
            assert!(
                wait_dead(&mut target),
                "the verified process is still running"
            );

            let refused = confirm_end_for(
                &core,
                kept.pid(),
                &kept_identity.created_at().wrapping_add(1).to_string(),
            );
            assert!(!refused.ended);
            assert!(refused.message.contains("没有结束"));
            assert!(
                kept.alive(),
                "a reused creation time ended the live process"
            );
            assert!(kept_identity.matches());

            let unreadable = confirm_end_for(&core, kept.pid(), "not-a-time");
            assert!(!unreadable.ended);
            assert!(unreadable.message.contains("没有结束"));
            assert!(kept.alive());

            let managed_end = confirm_end_for(
                &core,
                managed.pid(),
                &managed_identity.created_at().to_string(),
            );
            assert!(!managed_end.ended);
            assert!(managed_end.message.contains("没有结束"));
            assert!(
                managed_identity.matches(),
                "the managed session's process was ended"
            );

            let after_next = core.snapshot("next").expect("next was not removed");
            assert_eq!(after_next, before_next);
            assert_eq!(after_next.status, SessionStatus::Stopped);
            assert!(
                after_next.run_id.is_none(),
                "the external process was adopted"
            );
            assert_eq!(
                core.snapshot("holder").expect("holder").status,
                SessionStatus::Running,
                "the other managed session was stopped"
            );
            let listening = core.listening_processes();
            assert_eq!(listening.len(), 1);
            assert_eq!(listening[0].id, "holder");
            assert_eq!(listening[0].identity.pid(), managed.pid());
            assert_ne!(listening[0].identity.pid(), target.pid());
            assert_ne!(listening[0].identity.pid(), kept.pid());
            assert_eq!(core.summary().running, 1);
        }
    }
}
