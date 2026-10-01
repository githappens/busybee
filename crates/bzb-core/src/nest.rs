//! Nested gating: a child `busybee` whose parent already holds a lease
//! passes through instead of queueing behind its own ancestor.
//!
//! Pure: the client reads [`LEASE_ENV`] and a status snapshot, this decides
//! whether to skip the daemon. See `docs/design/bzbd.md` §Nesting.

use crate::protocol::LeaseView;

/// Written into every admitted task so a nested `busybee` can see that it is
/// already running under a lease. The daemon fills the id at admission; the
/// classifier never emits this variable, and a config row may not take it.
pub const LEASE_ENV: &str = "BUSYBEE_LEASE";

/// The line a nested client prints before it `exec`s. Named so tests and the
/// output contract quote the same string.
pub fn passthrough_line(id: u64) -> String {
    format!("busybee: nested under lease {id}, passing through")
}

/// Whether a blocking client that saw `marker` in [`LEASE_ENV`] should skip
/// the queue and run the command itself. `None` means submit as normal: no
/// marker, a marker that is not a lease id, or a lease that is not live
/// (queued, or already gone — a stale export).
pub fn passthrough_parent(marker: Option<&str>, leases: &[LeaseView]) -> Option<u64> {
    let id = marker?.parse::<u64>().ok()?;
    leases
        .iter()
        .find(|lease| lease.id == id && lease.state != "queued")
        .map(|lease| lease.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::LeaseView;

    fn lease(id: u64, state: &str) -> LeaseView {
        LeaseView {
            id,
            label: String::new(),
            tool: "true".into(),
            class: "none".into(),
            cores: 0,
            state: state.into(),
            elapsed_ms: 0,
            ahead: None,
            pueue_task_id: None,
        }
    }

    #[test]
    fn no_marker_submits_normally() {
        let leases = [lease(1, "running")];
        assert_eq!(passthrough_parent(None, &leases), None);
        assert_eq!(passthrough_parent(Some(""), &leases), None);
        assert_eq!(passthrough_parent(Some("nope"), &leases), None);
    }

    #[test]
    fn a_running_parent_passes_through() {
        let leases = [lease(3, "running"), lease(4, "queued")];
        assert_eq!(passthrough_parent(Some("3"), &leases), Some(3));
    }

    /// An adopted lease is still on the machine; a child inside it would
    /// deadlock the same way as under a connected parent.
    #[test]
    fn an_orphaned_parent_passes_through() {
        assert_eq!(
            passthrough_parent(Some("7"), &[lease(7, "orphaned")]),
            Some(7)
        );
    }

    #[test]
    fn a_queued_lease_is_not_a_parent_we_can_be_inside() {
        assert_eq!(passthrough_parent(Some("4"), &[lease(4, "queued")]), None);
    }

    #[test]
    fn a_stale_id_does_not_disable_gating() {
        assert_eq!(passthrough_parent(Some("99"), &[lease(1, "running")]), None);
        assert_eq!(passthrough_parent(Some("1"), &[]), None);
    }

    #[test]
    fn the_passthrough_line_matches_the_output_contract() {
        assert_eq!(
            passthrough_line(3),
            "busybee: nested under lease 3, passing through"
        );
    }
}
