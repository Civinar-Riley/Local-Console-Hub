//! Window commands for listening ports (#98, #99).
//!
//! Both read. They collect the OS owner tables, snapshot running sessions, and
//! attribute each row. They do not bind a socket, send HTTP, start a session,
//! or stop a process. A failed collection is part of the payload: it is not an
//! empty success, and it does not name a session.
//!
//! `list_listeners` is every row. `port_occupancy` is only the rows on one
//! session's configured port, and only when that session is about to be
//! started. The window asks with it before `activate_session`. Cancel never
//! reaches `activate`.

use std::net::IpAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::State;

use crate::listen::{
    attribute, Attribution, CollectError, ListenAddress, ListenRecord, Protocol, Readable,
    SessionProcess,
};
use crate::session::core::SessionCore;
use crate::session::state::SessionStatus;

/// One attributed listener, as the window renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListenerRowDto {
    /// `"TCP"` or `"UDP"`.
    pub protocol: String,
    pub address: String,
    pub port: u16,
    /// `None` when the owner pid could not be read.
    pub pid: Option<u32>,
    pub process_name: Option<String>,
    pub program_path: Option<String>,
    /// `"session"`, `"external"`, or `"unavailable"`.
    pub attribution: &'static str,
    /// Set only for [`Attribution::Session`]. The window looks the name up.
    pub session_id: Option<String>,
    /// Decimal creation time of `pid`, in the 100-ns ticks Windows reports.
    /// `None` when the pid or the time could not be read. A later confirm
    /// repeats this value; reading it does not end the process.
    pub created_at: Option<String>,
}

/// This attempt's outcome.
///
/// `checked_at_ms` is set only when the collection succeeded, including when
/// nothing is listening. `failure` is set only when it did not. `rows` on a
/// failure is empty, and that empty list is not a successful check.
/// `in_progress` is false on return: the command finishes the attempt it started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListenerListDto {
    pub checked_at_ms: Option<u64>,
    pub in_progress: bool,
    pub failure: Option<String>,
    pub rows: Vec<ListenerRowDto>,
}

/// List listening ports and who holds each one.
#[tauri::command(async)]
pub fn list_listeners(core: State<'_, SessionCore>) -> ListenerListDto {
    listeners_from(core.inner())
}

fn listeners_from(core: &SessionCore) -> ListenerListDto {
    match crate::listen::collect() {
        Ok(records) => {
            let sessions = core.listening_processes();
            let attributed = attribute(&records, &sessions);
            ListenerListDto {
                checked_at_ms: Some(unix_millis(SystemTime::now())),
                in_progress: false,
                failure: None,
                rows: attributed
                    .into_iter()
                    .map(|row| row_dto(row.record, row.attribution))
                    .collect(),
            }
        }
        Err(error) => failed_attempt(&error),
    }
}

fn failed_attempt(error: &CollectError) -> ListenerListDto {
    ListenerListDto {
        checked_at_ms: None,
        in_progress: false,
        failure: Some(error.to_string()),
        rows: Vec::new(),
    }
}

fn row_dto(record: ListenRecord, attribution: Attribution) -> ListenerRowDto {
    let (kind, session_id) = match attribution {
        Attribution::Session(id) => ("session", Some(id)),
        Attribution::External => ("external", None),
        Attribution::Unavailable => ("unavailable", None),
    };
    let pid = known(record.pid);
    let created_at = pid
        .and_then(crate::process::creation_time_of)
        .map(|ticks| ticks.to_string());
    ListenerRowDto {
        protocol: match record.protocol {
            Protocol::Tcp => "TCP".to_owned(),
            Protocol::Udp => "UDP".to_owned(),
        },
        address: format_address(&record.address),
        port: record.port,
        pid,
        process_name: known(record.process_name),
        program_path: known(record.program_path).map(|path| path.to_string_lossy().into_owned()),
        attribution: kind,
        session_id,
        created_at,
    }
}

fn known<T>(value: Readable<T>) -> Option<T> {
    match value {
        Readable::Known(value) => Some(value),
        Readable::Unavailable => None,
    }
}

fn format_address(address: &ListenAddress) -> String {
    match address.ip {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) if address.scope_id == 0 => ip.to_string(),
        IpAddr::V6(ip) => format!("{ip}%{}", address.scope_id),
    }
}

/// Who holds one session's configured port, before that session is started (#99).
///
/// `prompt` is false when there is nothing to ask: no configured port, the
/// session is not in a state `activate` would start from, or a successful
/// collection found nothing on that port. A failed collection sets `prompt`
/// and `failure` and leaves `rows` empty. That empty list is not a successful
/// check, and it does not name a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortOccupancyDto {
    pub prompt: bool,
    pub port: Option<u16>,
    /// Set only when the owner tables could not be read.
    pub failure: Option<String>,
    /// Listeners on the configured port. Empty when `prompt` is false, and
    /// empty when `failure` is set.
    pub rows: Vec<ListenerRowDto>,
}

/// Read who is listening on a session's configured port (#99).
///
/// The window calls this first. Continuing is a later `activate_session`;
/// cancelling sends nothing here and nothing afterwards. This command does not
/// start the session, stop another session, or end the occupying process.
#[tauri::command(async)]
pub fn port_occupancy(
    core: State<'_, SessionCore>,
    session_id: String,
) -> Result<PortOccupancyDto, String> {
    occupancy_for(core.inner(), &session_id)
}

fn occupancy_for(core: &SessionCore, session_id: &str) -> Result<PortOccupancyDto, String> {
    let config = core
        .session_config(session_id)
        .ok_or_else(|| format!("unknown session `{session_id}`"))?;
    let runtime = core
        .snapshot(session_id)
        .ok_or_else(|| format!("unknown session `{session_id}`"))?;
    let Some(port) = config.port else {
        return Ok(clear(None));
    };
    if !startable(runtime.status) {
        return Ok(clear(Some(port)));
    }
    Ok(decide_occupancy(
        port,
        crate::listen::collect(),
        &core.listening_processes(),
        session_id,
    ))
}

/// `Stopped`, `Exited`, and `Error` are the states from which `activate` calls
/// `start`. `Starting` and `Running` only report the current run. `Stopping`
/// is refused. Neither of those is a moment to ask about the port.
fn startable(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Stopped | SessionStatus::Exited | SessionStatus::Error
    )
}

fn clear(port: Option<u16>) -> PortOccupancyDto {
    PortOccupancyDto {
        prompt: false,
        port,
        failure: None,
        rows: Vec::new(),
    }
}

/// Occupancy of `port` from one collection.
///
/// `starting_id` is the session that is not running yet. It is removed before
/// attribution, so a verified process is never reported as that session's own
/// occupant. Another session's verified process still names that session.
/// Rows are filtered by port number only: TCP and UDP, on any address.
fn decide_occupancy(
    port: u16,
    collected: Result<Vec<ListenRecord>, CollectError>,
    sessions: &[SessionProcess],
    starting_id: &str,
) -> PortOccupancyDto {
    let owners: Vec<SessionProcess> = sessions
        .iter()
        .filter(|session| session.id != starting_id)
        .cloned()
        .collect();
    match collected {
        Err(error) => PortOccupancyDto {
            prompt: true,
            port: Some(port),
            failure: Some(error.to_string()),
            rows: Vec::new(),
        },
        Ok(records) => {
            let rows = attribute(&records, &owners)
                .into_iter()
                .filter(|row| row.record.port == port)
                .map(|row| row_dto(row.record, row.attribution))
                .collect::<Vec<_>>();
            PortOccupancyDto {
                prompt: !rows.is_empty(),
                port: Some(port),
                failure: None,
                rows,
            }
        }
    }
}

fn unix_millis(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use crate::config::{
        DisplayMode, EffectiveLogMode, EffectiveLogging, LifecycleOwner, LogSource, SessionConfig,
        SessionType,
    };
    use crate::listen::ListenAddress;
    use crate::process::ProcessIdentity;

    #[test]
    fn a_failed_attempt_has_no_success_time_and_no_rows() {
        let dto = failed_attempt(&CollectError::Unsupported);
        assert!(dto.checked_at_ms.is_none());
        assert!(dto.rows.is_empty());
        assert!(!dto.in_progress);
        assert_eq!(
            dto.failure.as_deref(),
            Some("listening-port collection is not supported on this platform")
        );

        let value = serde_json::to_value(&dto).expect("the failure serializes");
        assert!(value.get("checkedAtMs").unwrap().is_null());
        assert!(value.get("failure").unwrap().is_string());
        assert!(value.get("checked_at_ms").is_none(), "snake_case leaked");
        assert!(value.get("rows").unwrap().as_array().unwrap().is_empty());
    }

    #[test]
    fn an_address_keeps_family_and_a_non_zero_scope() {
        let v4 = ListenAddress {
            ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            scope_id: 0,
        };
        let v6 = ListenAddress {
            ip: IpAddr::V6(Ipv6Addr::LOCALHOST),
            scope_id: 0,
        };
        let scoped = ListenAddress {
            ip: IpAddr::V6(Ipv6Addr::LOCALHOST),
            scope_id: 12,
        };
        assert_eq!(format_address(&v4), "127.0.0.1");
        assert_eq!(format_address(&v6), "::1");
        assert_eq!(format_address(&scoped), "::1%12");
    }

    #[test]
    fn unavailable_fields_stay_absent_and_external_has_no_session() {
        let record = ListenRecord {
            protocol: Protocol::Udp,
            address: ListenAddress {
                ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                scope_id: 0,
            },
            port: 1,
            pid: Readable::Unavailable,
            process_name: Readable::Unavailable,
            program_path: Readable::Unavailable,
        };
        let dto = row_dto(record, Attribution::External);
        assert_eq!(dto.protocol, "UDP");
        assert!(dto.pid.is_none());
        assert!(dto.process_name.is_none());
        assert!(dto.program_path.is_none());
        assert!(dto.created_at.is_none());
        assert_eq!(dto.attribution, "external");
        assert!(dto.session_id.is_none());
        assert_ne!(dto.attribution, "session");
    }

    #[cfg(windows)]
    #[test]
    fn a_socket_with_no_managed_session_is_listed_as_external_and_then_closed() {
        use std::net::{TcpListener, UdpSocket};

        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind tcp");
        let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind udp");
        let tcp_port = tcp.local_addr().expect("tcp address").port();
        let udp_port = udp.local_addr().expect("udp address").port();

        let core = SessionCore::without_listener();
        let dto = listeners_from(&core);
        assert!(
            dto.failure.is_none(),
            "the tables were readable: {:?}",
            dto.failure
        );
        assert!(dto.checked_at_ms.is_some());
        assert!(!dto.in_progress);
        assert!(
            core.listening_processes().is_empty(),
            "this test does not start a session"
        );

        let tcp_row = dto
            .rows
            .iter()
            .find(|row| row.protocol == "TCP" && row.port == tcp_port && row.address == "127.0.0.1")
            .expect("the tcp socket is in the list");
        let udp_row = dto
            .rows
            .iter()
            .find(|row| row.protocol == "UDP" && row.port == udp_port && row.address == "127.0.0.1")
            .expect("the udp socket is in the list");
        assert_eq!(tcp_row.attribution, "external");
        assert!(tcp_row.session_id.is_none());
        assert_eq!(udp_row.attribution, "external");
        assert_eq!(tcp_row.pid, Some(std::process::id()));
        assert!(tcp_row.process_name.is_some());
        let created =
            crate::process::creation_time_of(std::process::id()).map(|ticks| ticks.to_string());
        assert_eq!(
            tcp_row.created_at, created,
            "the row carries the live process identity"
        );
        assert_eq!(udp_row.created_at, tcp_row.created_at);

        drop(tcp);
        drop(udp);
    }

    fn configured(id: &str, port: Option<u16>) -> SessionConfig {
        SessionConfig {
            id: id.to_owned(),
            name: format!("Service {id}"),
            session_type: SessionType::Service,
            cwd: Some(std::env::current_dir().expect("the test process has a working directory")),
            command: Some("cmd.exe /c exit 0".to_owned()),
            url: None,
            port,
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

    fn listener(
        protocol: Protocol,
        port: u16,
        ip: IpAddr,
        pid: Readable<u32>,
        name: Readable<String>,
    ) -> ListenRecord {
        ListenRecord {
            protocol,
            address: ListenAddress { ip, scope_id: 0 },
            port,
            pid,
            process_name: name,
            program_path: Readable::Unavailable,
        }
    }

    #[test]
    fn occupancy_is_asked_only_when_activate_would_start() {
        assert!(startable(SessionStatus::Stopped));
        assert!(startable(SessionStatus::Exited));
        assert!(startable(SessionStatus::Error));
        assert!(!startable(SessionStatus::Starting));
        assert!(!startable(SessionStatus::Running));
        assert!(!startable(SessionStatus::Stopping));
    }

    #[test]
    fn a_session_with_no_configured_port_does_not_prompt_or_start() {
        let core = SessionCore::without_listener();
        core.register(configured("plain", None)).expect("register");
        let before = core.snapshot("plain").expect("the session exists");

        let dto = occupancy_for(&core, "plain").expect("the read succeeds");

        assert!(!dto.prompt);
        assert!(dto.port.is_none());
        assert!(dto.failure.is_none());
        assert!(dto.rows.is_empty());
        let value = serde_json::to_value(&dto).expect("the clear result serializes");
        assert_eq!(value["prompt"].as_bool(), Some(false));
        assert!(value["port"].is_null());
        assert!(value["failure"].is_null());
        assert!(value.get("sessionId").is_none());
        let after = core.snapshot("plain").expect("the session still exists");
        assert_eq!(&after, &before);
        assert_eq!(after.status, SessionStatus::Stopped);
        assert!(after.run_id.is_none());
    }

    #[test]
    fn an_unknown_session_is_refused_without_starting_anything() {
        let core = SessionCore::without_listener();
        let error = occupancy_for(&core, "missing").expect_err("there is no such session");
        assert!(error.contains("missing"));
        assert!(core.snapshots().is_empty());
    }

    #[test]
    fn a_failed_collection_is_unavailable_not_an_empty_success_or_a_session() {
        let session = SessionProcess {
            id: "svc".to_owned(),
            identity: ProcessIdentity::for_test(7, 11),
            tree: Some(vec![ProcessIdentity::for_test(7, 11)]),
        };
        let failed = decide_occupancy(8080, Err(CollectError::Unsupported), &[session], "next");
        assert!(failed.prompt);
        assert!(failed.rows.is_empty());
        assert_eq!(failed.port, Some(8080));
        assert!(failed.failure.is_some());
        let value = serde_json::to_value(&failed).expect("the failure serializes");
        assert_eq!(value["prompt"].as_bool(), Some(true));
        assert!(value["rows"].as_array().unwrap().is_empty());
        assert!(value["failure"].is_string());
        assert!(value.get("sessionId").is_none());
        assert!(value.get("session_id").is_none());

        let empty = decide_occupancy(8080, Ok(Vec::new()), &[], "next");
        assert!(!empty.prompt, "nothing listening is not a prompt");
        assert!(empty.failure.is_none());
        assert!(empty.rows.is_empty());
        assert_ne!(failed, empty);
    }

    #[test]
    fn a_missing_field_is_unavailable_and_every_listener_on_the_port_is_kept() {
        let session = SessionProcess {
            id: "svc".to_owned(),
            identity: ProcessIdentity::for_test(7, 11),
            tree: Some(vec![ProcessIdentity::for_test(7, 11)]),
        };
        // The process name is the session's, and the port is the one being
        // started. Neither is ownership. The pid was not read, so the row is
        // not that session.
        let records = vec![
            listener(
                Protocol::Tcp,
                9,
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                Readable::Unavailable,
                Readable::Known("svc.exe".to_owned()),
            ),
            listener(
                Protocol::Tcp,
                8080,
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                Readable::Unavailable,
                Readable::Unavailable,
            ),
            listener(
                Protocol::Udp,
                8080,
                IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                Readable::Known(u32::MAX),
                Readable::Known("svc.exe".to_owned()),
            ),
        ];

        let dto = decide_occupancy(8080, Ok(records), &[session], "next");

        assert!(dto.prompt);
        assert!(dto.failure.is_none());
        assert_eq!(dto.rows.len(), 2, "listeners on the port are not collapsed");
        assert!(dto.rows.iter().all(|row| row.port == 8080));
        assert_eq!(dto.rows[0].protocol, "TCP");
        assert_eq!(dto.rows[0].address, "0.0.0.0");
        assert!(dto.rows[0].pid.is_none());
        assert!(dto.rows[0].process_name.is_none());
        assert!(dto.rows[0].program_path.is_none());
        assert_eq!(dto.rows[1].protocol, "UDP");
        assert_eq!(dto.rows[1].address, "::");
        assert_eq!(dto.rows[1].pid, Some(u32::MAX));
        for row in &dto.rows {
            assert_eq!(row.attribution, "unavailable");
            assert!(row.session_id.is_none());
            assert_ne!(row.attribution, "session");
            assert_ne!(row.session_id.as_deref(), Some("svc"));
            assert_ne!(row.session_id.as_deref(), Some("next"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_session_being_started_is_not_named_as_its_own_occupant() {
        use crate::process::creation_time_of;

        let pid = std::process::id();
        let created = creation_time_of(pid).expect("the test process has a creation time");
        let identity = ProcessIdentity::recorded(pid, created);
        assert!(identity.matches(), "the identity is the live test process");
        let record = listener(
            Protocol::Tcp,
            9,
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            Readable::Known(pid),
            Readable::Known("hub.exe".to_owned()),
        );
        let next = SessionProcess {
            id: "next".to_owned(),
            identity,
            tree: Some(Vec::new()),
        };
        let holder = SessionProcess {
            id: "holder".to_owned(),
            identity,
            tree: Some(Vec::new()),
        };

        let only_self = decide_occupancy(
            9,
            Ok(vec![record.clone()]),
            std::slice::from_ref(&next),
            "next",
        );
        assert!(only_self.prompt);
        assert_eq!(only_self.rows.len(), 1);
        assert_eq!(only_self.rows[0].address, "0.0.0.0");
        assert_eq!(only_self.rows[0].attribution, "external");
        assert!(only_self.rows[0].session_id.is_none());
        assert_ne!(only_self.rows[0].session_id.as_deref(), Some("next"));

        let named = decide_occupancy(9, Ok(vec![record]), &[next, holder], "next");
        assert_eq!(named.rows[0].attribution, "session");
        assert_eq!(named.rows[0].session_id.as_deref(), Some("holder"));
        assert_ne!(named.rows[0].session_id.as_deref(), Some("next"));
    }

    #[cfg(windows)]
    #[test]
    fn a_real_bind_with_no_managed_owner_is_external_and_the_session_stays_stopped() {
        use std::net::{TcpListener, UdpSocket};

        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind tcp");
        let port = tcp.local_addr().expect("tcp address").port();
        let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).expect("bind udp on the same port");

        let core = SessionCore::without_listener();
        core.register(configured("next", Some(port)))
            .expect("register");
        let before = core.snapshot("next").expect("registered stopped");
        assert!(
            core.listening_processes().is_empty(),
            "registering does not start a session"
        );

        let dto = occupancy_for(&core, "next").expect("occupancy");

        assert!(dto.prompt);
        assert!(dto.failure.is_none());
        assert_eq!(dto.port, Some(port));
        let ours: Vec<_> = dto
            .rows
            .iter()
            .filter(|row| row.pid == Some(std::process::id()))
            .collect();
        assert!(
            ours.iter()
                .any(|row| row.protocol == "TCP" && row.address == "127.0.0.1"),
            "tcp is reported on {port}: {ours:?}"
        );
        assert!(
            ours.iter()
                .any(|row| row.protocol == "UDP" && row.address == "127.0.0.1"),
            "udp is reported on {port}: {ours:?}"
        );
        assert!(ours.len() >= 2, "tcp and udp are both listed: {ours:?}");
        for row in &ours {
            assert_eq!(row.attribution, "external");
            assert!(row.session_id.is_none());
            assert!(row
                .process_name
                .as_ref()
                .is_some_and(|name| !name.is_empty()));
            assert!(row
                .program_path
                .as_ref()
                .is_some_and(|path| !path.is_empty()));
            assert_ne!(row.attribution, "session");
            assert_ne!(row.session_id.as_deref(), Some("next"));
        }
        assert_eq!(core.snapshot("next").as_ref(), Some(&before));
        assert_eq!(before.status, SessionStatus::Stopped);
        assert!(before.run_id.is_none());
        assert!(core.listening_processes().is_empty());
        assert_eq!(core.summary().running, 0);
        assert!(tcp.local_addr().is_ok(), "the occupant was not closed");
        assert!(udp.local_addr().is_ok(), "the occupant was not closed");
        drop(tcp);
        drop(udp);
    }

    #[cfg(windows)]
    #[test]
    fn a_managed_sessions_verified_process_is_named_and_nothing_is_started_or_stopped() {
        use std::net::{TcpListener, UdpSocket};

        use crate::process::{creation_time_of, ExternalProcess};

        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind tcp");
        let port = tcp.local_addr().expect("tcp address").port();
        let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).expect("bind udp on the same port");

        let core = SessionCore::without_listener();
        core.register(configured("next", Some(port)))
            .expect("register the session that would start");
        core.register(configured("holder", Some(port)))
            .expect("register the session that already holds the port");

        let pid = std::process::id();
        let created = creation_time_of(pid).expect("the test process has a creation time");
        let identity = ProcessIdentity::recorded(pid, created);
        let process = ExternalProcess::open(identity).expect("open the test process");
        core.adopt("holder", identity, process)
            .expect("associate the already-running process");

        struct Release<'a>(&'a SessionCore);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                let _ = self.0.release_adopted("holder");
            }
        }
        let _release = Release(&core);

        assert_eq!(
            core.snapshot("holder").expect("holder").status,
            SessionStatus::Running
        );
        let before_next = core.snapshot("next").expect("next");
        assert_eq!(before_next.status, SessionStatus::Stopped);
        assert!(before_next.run_id.is_none());

        let dto = occupancy_for(&core, "next").expect("occupancy of the session being started");

        assert!(dto.prompt);
        assert!(dto.failure.is_none());
        let ours: Vec<_> = dto.rows.iter().filter(|row| row.pid == Some(pid)).collect();
        assert!(
            ours.iter()
                .any(|row| row.protocol == "TCP" && row.address == "127.0.0.1"),
            "tcp names the other session: {ours:?}"
        );
        assert!(
            ours.iter()
                .any(|row| row.protocol == "UDP" && row.address == "127.0.0.1"),
            "udp names the other session: {ours:?}"
        );
        for row in &ours {
            assert_eq!(row.attribution, "session");
            assert_eq!(row.session_id.as_deref(), Some("holder"));
            assert!(row
                .process_name
                .as_ref()
                .is_some_and(|name| !name.is_empty()));
            assert!(row
                .program_path
                .as_ref()
                .is_some_and(|path| !path.is_empty()));
            assert_ne!(row.session_id.as_deref(), Some("next"));
        }
        assert!(dto
            .rows
            .iter()
            .all(|row| row.session_id.as_deref() != Some("next")));

        let after_next = core.snapshot("next").expect("next was not removed");
        assert_eq!(&after_next, &before_next);
        assert_eq!(after_next.status, SessionStatus::Stopped);
        assert!(
            after_next.run_id.is_none(),
            "cancel is not this test, but the read creates no run"
        );
        assert_eq!(
            core.snapshot("holder").expect("holder").status,
            SessionStatus::Running,
            "the occupying session was not stopped"
        );
        assert_eq!(core.listening_processes().len(), 1);
        assert!(tcp.local_addr().is_ok(), "the occupant was not closed");
        assert!(udp.local_addr().is_ok(), "the occupant was not closed");

        let running = occupancy_for(&core, "holder").expect("a running session is readable");
        assert!(
            !running.prompt,
            "activate would not start a running session, so there is no prompt"
        );
        assert!(running.failure.is_none());
        assert!(running.rows.is_empty());
        assert_eq!(
            core.snapshot("holder").expect("holder").status,
            SessionStatus::Running
        );
        assert_eq!(
            core.snapshot("next").expect("next").status,
            SessionStatus::Stopped
        );

        drop(tcp);
        drop(udp);
    }

    /// A listener this test starts, the collection after it exits, and the
    /// process Windows actually gives that pid to next (#105).
    ///
    /// The session snapshot keeps the creation time read while the first
    /// process was alive. The second process is the one `spawn` returned when
    /// its pid was that number, and every row passed to attribution is one
    /// [`crate::listen::collect`] returned. A creation time this test invents
    /// is not a stand-in for that.
    #[cfg(windows)]
    #[test]
    fn a_real_pid_reuse_is_not_the_old_session() {
        reuse::a_listener_exits_and_a_reused_pid_is_not_the_old_session();
    }

    #[cfg(windows)]
    mod reuse {
        use std::fs;
        use std::path::{Path, PathBuf};
        use std::process::{Command, Stdio};
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        use std::sync::{Arc, Mutex};
        use std::thread;
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        use crate::listen::{
            attribute, Attribution, ListenRecord, Protocol, Readable, SessionProcess,
            EXTERNAL_LABEL, UNAVAILABLE_LABEL,
        };
        use crate::process::{creation_time_of, ProcessIdentity};

        use super::decide_occupancy;

        const HOLDER: &str = "holder";
        /// Threads that only this test starts, so the pid hunt can finish
        /// inside the bound without raising the suite-wide `--test-threads`.
        const HUNTERS: usize = 8;
        /// Long enough that a busy machine can recycle one pid, and short
        /// enough that a run where Windows does not reuse it fails instead of
        /// hanging. The hunt returns as soon as a spawned process receives it.
        /// A quiet run has finished well inside a minute; the extra room is
        /// only for when this test shares the machine with the rest of the suite.
        const REUSE_BOUND: Duration = Duration::from_secs(180);
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        const LISTENER_SOURCE: &str = r#"
fn main() {
    let path = std::env::args().nth(1).expect("port file");
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind");
    let port = listener.local_addr().expect("address").port();
    std::fs::write(&path, port.to_string()).expect("write port");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
        let _ = listener.local_addr();
    }
}
"#;

        /// The process `spawn` returned. Drop terminates that process through
        /// the handle, not through a pid lookup, so a number Windows has
        /// already reused cannot be what gets killed.
        struct OwnedProcess(Option<std::process::Child>);

        impl OwnedProcess {
            fn spawn(exe: &Path, port_file: &Path) -> Self {
                use std::os::windows::process::CommandExt;

                let child = Command::new(exe)
                    .arg(port_file)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn()
                    .expect("spawn a listener this test owns");
                OwnedProcess(Some(child))
            }

            fn pid(&self) -> u32 {
                self.0
                    .as_ref()
                    .expect("the owned process is still held")
                    .id()
            }

            fn stop(&mut self) {
                if let Some(mut child) = self.0.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }

        impl Drop for OwnedProcess {
            fn drop(&mut self) {
                self.stop();
            }
        }

        struct TempTree(PathBuf);

        impl TempTree {
            fn new() -> Self {
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or(0);
                let path = std::env::temp_dir()
                    .join(format!("lch-pid-reuse-{}-{nanos}", std::process::id()));
                fs::create_dir_all(&path).expect("create a temp dir for the listener helper");
                TempTree(path)
            }
        }

        impl Drop for TempTree {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        fn compile_listener(dir: &Path) -> PathBuf {
            let source = dir.join("listener.rs");
            let exe = dir.join("listener.exe");
            fs::write(&source, LISTENER_SOURCE).expect("write the listener helper source");
            let output = Command::new("rustc")
                .args(["-O", "-o"])
                .arg(&exe)
                .arg(&source)
                .output()
                .expect("rustc is on PATH so the listener helper can be built");
            assert!(
                output.status.success(),
                "the listener helper failed to compile: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            exe
        }

        fn wait_port(path: &Path) -> u16 {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(text) = fs::read_to_string(path) {
                    if let Ok(port) = text.trim().parse::<u16>() {
                        if port != 0 {
                            return port;
                        }
                    }
                }
                if Instant::now() >= deadline {
                    panic!(
                        "the listener did not publish its port at {}",
                        path.display()
                    );
                }
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn collected() -> Vec<ListenRecord> {
            let mut last = None;
            for _ in 0..10 {
                match crate::listen::collect() {
                    Ok(records) => return records,
                    Err(error) => {
                        last = Some(error.to_string());
                        thread::sleep(Duration::from_millis(30));
                    }
                }
            }
            panic!(
                "reading the owner tables failed: {}",
                last.unwrap_or_else(|| "no attempt".to_owned())
            );
        }

        fn lists(records: &[ListenRecord], pid: u32, port: u16) -> bool {
            records.iter().any(|record| {
                record.protocol == Protocol::Tcp
                    && record.port == port
                    && record.pid == Readable::Known(pid)
            })
        }

        fn wait_until_listed(pid: u32, port: u16) -> Vec<ListenRecord> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let records = collected();
                if lists(&records, pid, port) {
                    return records;
                }
                if Instant::now() >= deadline {
                    panic!("pid {pid} was not listening on {port}");
                }
                thread::sleep(Duration::from_millis(30));
            }
        }

        fn wait_until_unlisted(pid: u32, port: u16) -> Vec<ListenRecord> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let records = collected();
                if !lists(&records, pid, port) {
                    return records;
                }
                if Instant::now() >= deadline {
                    panic!("pid {pid} was still listed on {port} after it exited");
                }
                thread::sleep(Duration::from_millis(30));
            }
        }

        fn wait_until_released(pid: u32, created: u64) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match creation_time_of(pid) {
                    Some(live) if live == created => {
                        if Instant::now() >= deadline {
                            panic!(
                                "pid {pid} still reports the exited process; its handle was not released"
                            );
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                    _ => return,
                }
            }
        }

        fn holder(pid: u32, created: u64) -> SessionProcess {
            let identity = ProcessIdentity::recorded(pid, created);
            SessionProcess {
                id: HOLDER.to_owned(),
                identity,
                tree: Some(vec![identity]),
            }
        }

        fn ours(
            records: &[crate::listen::AttributedRecord],
            pid: u32,
            port: u16,
        ) -> Vec<&crate::listen::AttributedRecord> {
            records
                .iter()
                .filter(|row| {
                    row.record.protocol == Protocol::Tcp
                        && row.record.port == port
                        && row.record.pid == Readable::Known(pid)
                })
                .collect()
        }

        fn assert_held(records: &[ListenRecord], session: &SessionProcess, pid: u32, port: u16) {
            let attributed = attribute(records, std::slice::from_ref(session));
            let rows = ours(&attributed, pid, port);
            assert!(
                !rows.is_empty(),
                "collect did not list the listener this test started"
            );
            assert!(
                rows.iter().any(|row| matches!(
                    &row.record.process_name,
                    Readable::Known(name) if name.eq_ignore_ascii_case("listener.exe")
                )),
                "the collected row did not name the listener process: {rows:?}"
            );
            for row in &rows {
                assert_eq!(row.attribution, Attribution::Session(HOLDER.to_owned()));
                assert_eq!(row.attribution.label(), HOLDER);
                assert_ne!(row.attribution, Attribution::External);
                assert_ne!(row.attribution, Attribution::Unavailable);
            }

            let dto = decide_occupancy(
                port,
                Ok(records.to_vec()),
                std::slice::from_ref(session),
                "next",
            );
            assert!(dto.failure.is_none());
            assert!(dto.prompt);
            let listed: Vec<_> = dto.rows.iter().filter(|row| row.pid == Some(pid)).collect();
            assert!(
                !listed.is_empty(),
                "occupancy dropped the listener: {dto:?}"
            );
            for row in listed {
                assert_eq!(row.attribution, "session");
                assert_eq!(row.session_id.as_deref(), Some(HOLDER));
                assert_ne!(row.session_id.as_deref(), Some("next"));
            }
        }

        fn assert_not_held(
            records: &[ListenRecord],
            session: &SessionProcess,
            pid: u32,
            port: u16,
        ) {
            assert!(
                !lists(records, pid, port),
                "the exited listener is still in the collection"
            );
            let attributed = attribute(records, std::slice::from_ref(session));
            assert!(
                attributed
                    .iter()
                    .all(|row| row.attribution != Attribution::Session(HOLDER.to_owned())),
                "a collected row is still the old session: {attributed:?}"
            );
            let dto = decide_occupancy(
                port,
                Ok(records.to_vec()),
                std::slice::from_ref(session),
                "next",
            );
            assert!(dto.failure.is_none());
            assert!(dto.rows.iter().all(|row| row.pid != Some(pid)));
            assert!(dto
                .rows
                .iter()
                .all(|row| row.session_id.as_deref() != Some(HOLDER)));
        }

        /// Spawn listeners until one receives `target`, or the bound elapses.
        ///
        /// Every child is one this function spawned. A miss is terminated
        /// through its handle before the next spawn. `Err` is the number of
        /// spawns when Windows did not reuse the pid inside the bound.
        fn await_pid_reuse(
            exe: &Path,
            dir: &Path,
            target: u32,
        ) -> Result<(OwnedProcess, u16), u32> {
            let found: Arc<Mutex<Option<(OwnedProcess, PathBuf)>>> = Arc::new(Mutex::new(None));
            let stop = Arc::new(AtomicBool::new(false));
            let attempts = Arc::new(AtomicU32::new(0));
            let seq = Arc::new(AtomicU32::new(0));
            let started = Instant::now();

            thread::scope(|scope| {
                for _ in 0..HUNTERS {
                    let found = Arc::clone(&found);
                    let stop = Arc::clone(&stop);
                    let attempts = Arc::clone(&attempts);
                    let seq = Arc::clone(&seq);
                    scope.spawn(move || {
                        while !stop.load(Ordering::Relaxed) && started.elapsed() < REUSE_BOUND {
                            let n = seq.fetch_add(1, Ordering::Relaxed);
                            let port_file = dir.join(format!("reuse-{n}.port"));
                            let matched = {
                                let child = OwnedProcess::spawn(exe, &port_file);
                                attempts.fetch_add(1, Ordering::Relaxed);
                                if child.pid() == target {
                                    stop.store(true, Ordering::Relaxed);
                                    let mut slot =
                                        found.lock().expect("the reuse lock is not poisoned");
                                    if slot.is_none() {
                                        *slot = Some((child, port_file.clone()));
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                }
                            };
                            if matched {
                                return;
                            }
                            let _ = fs::remove_file(&port_file);
                        }
                    });
                }
            });

            let spawned = attempts.load(Ordering::Relaxed);
            let won = found.lock().expect("the reuse lock is not poisoned").take();
            match won {
                Some((child, port_file)) => {
                    let port = wait_port(&port_file);
                    let _ = fs::remove_file(&port_file);
                    Ok((child, port))
                }
                None => Err(spawned),
            }
        }

        pub(super) fn a_listener_exits_and_a_reused_pid_is_not_the_old_session() {
            let tree = TempTree::new();
            let exe = compile_listener(&tree.0);
            let first_port_file = tree.0.join("first.port");
            let mut listener = OwnedProcess::spawn(&exe, &first_port_file);
            let pid = listener.pid();
            let port = wait_port(&first_port_file);
            let created = creation_time_of(pid).expect("the listener has a creation time");
            let session = holder(pid, created);
            assert!(
                session.identity.matches(),
                "the snapshot is the process that is listening"
            );

            let records = wait_until_listed(pid, port);
            assert_held(&records, &session, pid, port);

            listener.stop();
            wait_until_released(pid, created);
            let records = wait_until_unlisted(pid, port);
            assert!(
                records
                    .iter()
                    .all(|record| record.pid != Readable::Known(pid)),
                "after exit, the next collection still lists pid {pid}"
            );
            assert!(
                !session.identity.matches(),
                "the exited process is still the pid's current identity"
            );
            assert_not_held(&records, &session, pid, port);

            let (mut winner, reused_port) =
                await_pid_reuse(&exe, &tree.0, pid).unwrap_or_else(|spawned| {
                    panic!(
                        "Windows did not reuse pid {pid} within {REUSE_BOUND:?} ({spawned} spawns). \
                         The exit collection above no longer listed it and no longer \
                         attributed it to {HOLDER}. No simulated listening row was used."
                    );
                });
            assert_eq!(
                winner.pid(),
                pid,
                "the kept process is the one that received the pid"
            );
            let live = creation_time_of(pid);
            let records = wait_until_listed(pid, reused_port);
            let attributed = attribute(&records, std::slice::from_ref(&session));
            let rows = ours(&attributed, pid, reused_port);
            assert!(
                !rows.is_empty(),
                "collect did not list the process that received the reused pid"
            );
            assert!(
                rows.iter().any(|row| matches!(
                    &row.record.process_name,
                    Readable::Known(name) if name.eq_ignore_ascii_case("listener.exe")
                )),
                "the reused pid's collected row is not the process this test spawned: {rows:?}"
            );

            match live {
                None => {
                    for row in &rows {
                        assert_eq!(row.attribution, Attribution::Unavailable);
                        assert_eq!(row.attribution.label(), UNAVAILABLE_LABEL);
                        assert_ne!(row.attribution, Attribution::Session(HOLDER.to_owned()));
                        assert_ne!(row.attribution, Attribution::External);
                    }
                }
                Some(live_created) => {
                    assert_ne!(
                        live_created, created,
                        "the process now using pid {pid} is still the one that exited"
                    );
                    assert!(
                        !session.identity.matches(),
                        "the old session identity matches the process that reused the pid"
                    );
                    for row in &rows {
                        assert_eq!(row.attribution, Attribution::External);
                        assert_eq!(row.attribution.label(), EXTERNAL_LABEL);
                        assert_ne!(row.attribution, Attribution::Session(HOLDER.to_owned()));
                        assert_ne!(row.attribution, Attribution::Unavailable);
                    }
                }
            }
            assert!(
                attributed
                    .iter()
                    .all(|row| row.attribution != Attribution::Session(HOLDER.to_owned())),
                "a collected row is still the old session after the pid was reused"
            );

            let dto = decide_occupancy(
                reused_port,
                Ok(records),
                std::slice::from_ref(&session),
                "next",
            );
            assert!(dto.failure.is_none());
            let listed: Vec<_> = dto.rows.iter().filter(|row| row.pid == Some(pid)).collect();
            assert!(!listed.is_empty(), "occupancy dropped the reused listener");
            for row in listed {
                assert_ne!(row.attribution, "session");
                assert_ne!(row.session_id.as_deref(), Some(HOLDER));
                assert!(row.session_id.is_none());
                match live {
                    None => assert_eq!(row.attribution, "unavailable"),
                    Some(_) => assert_eq!(row.attribution, "external"),
                }
            }

            winner.stop();
        }
    }
}
