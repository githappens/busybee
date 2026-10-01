//! Pure state machine for the lines a client prints while its lease is queued
//! (spec §Client output contract); the caller prefixes `busybee: `.

/// Quiet ticks (seconds) before, and between, heartbeats.
const HEARTBEAT_TICKS: u64 = 30;

#[derive(Debug, Default)]
pub struct QueueLines {
    last_ahead: Option<usize>,
    ticks_since_change: u64,
    last_heartbeat_tick: u64,
}

impl QueueLines {
    pub fn new() -> Self {
        Self::default()
    }

    /// The first `Queued` event announces the position; later ones only when
    /// it moved.
    pub fn queued(&mut self, ahead: usize) -> Option<String> {
        let line = match self.last_ahead {
            None => format!("queued ({ahead} ahead)"),
            Some(previous) if previous == ahead => return None,
            Some(_) => format!("{ahead} ahead…"),
        };
        self.last_ahead = Some(ahead);
        self.ticks_since_change = 0;
        Some(line)
    }

    /// A second passed while queued; heartbeat every [`HEARTBEAT_TICKS`] of
    /// quiet.
    pub fn tick(&mut self, tick: u64) -> Option<String> {
        let ahead = self.last_ahead?;
        self.ticks_since_change += 1;
        if self.ticks_since_change < HEARTBEAT_TICKS
            || tick.saturating_sub(self.last_heartbeat_tick) < HEARTBEAT_TICKS
        {
            return None;
        }
        self.last_heartbeat_tick = tick;
        Some(format!("still queued ({ahead} ahead)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_queued_event_announces_the_position() {
        assert_eq!(
            QueueLines::new().queued(2).as_deref(),
            Some("queued (2 ahead)")
        );
    }

    #[test]
    fn a_queue_position_that_moved_is_reported() {
        let mut lines = QueueLines::new();
        lines.queued(2);
        assert_eq!(lines.queued(1).as_deref(), Some("1 ahead…"));
        assert_eq!(lines.queued(0).as_deref(), Some("0 ahead…"));
    }

    /// bzbd re-notifies every waiting lease whenever the queue changes.
    #[test]
    fn a_queue_position_that_did_not_move_stays_quiet() {
        let mut lines = QueueLines::new();
        lines.queued(2);
        assert_eq!(lines.queued(2), None);
    }

    #[test]
    fn a_quiet_queue_says_so_every_thirty_ticks() {
        let mut lines = QueueLines::new();
        lines.queued(3);
        for tick in 1..HEARTBEAT_TICKS {
            assert_eq!(lines.tick(tick), None, "spoke up at tick {tick}");
        }
        assert_eq!(
            lines.tick(HEARTBEAT_TICKS).as_deref(),
            Some("still queued (3 ahead)")
        );
        for tick in HEARTBEAT_TICKS + 1..2 * HEARTBEAT_TICKS {
            assert_eq!(lines.tick(tick), None, "spoke up at tick {tick}");
        }
        assert_eq!(
            lines.tick(2 * HEARTBEAT_TICKS).as_deref(),
            Some("still queued (3 ahead)")
        );
    }

    #[test]
    fn movement_postpones_the_heartbeat() {
        let mut lines = QueueLines::new();
        lines.queued(3);
        for tick in 1..HEARTBEAT_TICKS {
            lines.tick(tick);
        }
        lines.queued(2);
        assert_eq!(lines.tick(HEARTBEAT_TICKS), None);
    }

    #[test]
    fn nothing_is_said_before_the_first_queued_event() {
        let mut lines = QueueLines::new();
        for tick in 1..=2 * HEARTBEAT_TICKS {
            assert_eq!(lines.tick(tick), None, "spoke up at tick {tick}");
        }
    }
}
