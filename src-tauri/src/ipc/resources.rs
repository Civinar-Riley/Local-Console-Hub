//! Window command for one managed session's CPU and memory (#108).
//!
//! The command reads. It does not start a session, stop one, or end a process
//! in the tree. A session that holds no live run — or whose process no longer
//! has the creation time the session holds — comes back with no check time and
//! no members. That empty list is not a resource table, and it is not an
//! earlier reading restamped as a check that just finished.
//!
//! A field that could not be read is null. Null is not zero.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::State;

use crate::resources::{read_session, MemberReading, SessionReading};
use crate::session::core::SessionCore;

/// One confirmed process, as the window renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceMemberDto {
    pub pid: u32,
    /// `"own"` or `"tree"`.
    pub role: &'static str,
    /// Hundredths of one percent of the machine. Null when the rate could not
    /// be computed. Zero is a measured zero.
    pub cpu_percent_hundredths: Option<u32>,
    /// Working set in bytes. Null when it could not be read.
    pub memory_bytes: Option<u64>,
}

/// One attempt to read a session.
///
/// `checked_at_ms` is set only when the session's own process still matched.
/// `members` is empty whenever `checked_at_ms` is absent. `tree_unavailable`
/// is true only on a finished check whose tree could not be listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResourcesDto {
    pub session_id: String,
    pub checked_at_ms: Option<u64>,
    pub tree_unavailable: bool,
    pub members: Vec<ResourceMemberDto>,
}

/// Read CPU and memory for one managed session's own process tree (#108).
///
/// The window asks when the user looks. Cancelling sends nothing. This
/// command does not stop the session or any process it finds.
#[tauri::command(async)]
pub fn session_resources(core: State<'_, SessionCore>, session_id: String) -> SessionResourcesDto {
    resources_for(core.inner(), &session_id)
}

fn resources_for(core: &SessionCore, session_id: &str) -> SessionResourcesDto {
    let Some(session) = core
        .listening_processes()
        .into_iter()
        .find(|session| session.id == session_id)
    else {
        return idle(session_id);
    };
    to_dto(&read_session(&session))
}

fn idle(session_id: &str) -> SessionResourcesDto {
    SessionResourcesDto {
        session_id: session_id.to_owned(),
        checked_at_ms: None,
        tree_unavailable: false,
        members: Vec::new(),
    }
}

fn to_dto(reading: &SessionReading) -> SessionResourcesDto {
    let Some(checked_at) = reading.checked_at else {
        return idle(&reading.session_id);
    };
    SessionResourcesDto {
        session_id: reading.session_id.clone(),
        checked_at_ms: Some(unix_millis(checked_at)),
        tree_unavailable: reading.tree_unavailable,
        members: reading.members.iter().map(member_dto).collect(),
    }
}

fn member_dto(member: &MemberReading) -> ResourceMemberDto {
    ResourceMemberDto {
        pid: member.pid,
        role: member.role.as_str(),
        cpu_percent_hundredths: member.cpu_percent_hundredths,
        memory_bytes: member.memory_bytes,
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

    #[test]
    fn an_unreadable_field_serializes_as_null_not_zero() {
        let dto = ResourceMemberDto {
            pid: 4,
            role: "own",
            cpu_percent_hundredths: None,
            memory_bytes: None,
        };
        let value = serde_json::to_value(&dto).expect("member serializes");
        assert!(value.get("cpuPercentHundredths").unwrap().is_null());
        assert!(value.get("memoryBytes").unwrap().is_null());
        assert!(value.get("cpu_percent_hundredths").is_none());
        assert!(value.get("memory_bytes").is_none());
    }

    #[test]
    fn a_session_without_a_finished_check_serializes_no_table() {
        let dto = SessionResourcesDto {
            session_id: "gone".to_owned(),
            checked_at_ms: None,
            tree_unavailable: false,
            members: Vec::new(),
        };
        let value = serde_json::to_value(&dto).expect("session serializes");
        assert!(value["checkedAtMs"].is_null());
        assert_eq!(value["members"].as_array().unwrap().len(), 0);
        assert_eq!(value["treeUnavailable"].as_bool(), Some(false));
    }
}
