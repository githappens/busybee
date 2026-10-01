//! Pure: a child `busybee` whose parent holds a live lease passes through
//! instead of queueing behind its own ancestor (spec §Nesting).

use crate::protocol::LeaseView;

/// Set by the daemon at admission in every task's environment.
pub const LEASE_ENV: &str = "BUSYBEE_LEASE";

/// What a nested client prints before it `exec`s.
pub fn passthrough_line(id: u64) -> String {
    format!("busybee: nested under lease {id}, passing through")
}

/// The parent lease to pass through under, or `None` to submit as normal: no
/// or unparseable marker, or a lease that is queued or gone (stale export).
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
