use std::fmt::Write;

use pueue_lib::task::{Task, TaskStatus};

use crate::status::count_ahead;

/// A user-visible event emitted as the task transitions through states.
/// The `Line` payload is the exact string to print to the client's stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitEvent {
    Line(String),
    /// The task entered `Running`. Caller should start streaming the log.
    Started,
    /// The task reached `Done`. Caller should exit with the mapped code.
    Finished { result_label: String },
}

pub struct WaitState {
    my_id: usize,
    label: String,
    last_ahead: Option<usize>,
    last_state: Option<&'static str>,
    last_heartbeat_tick: u64,
    ticks_since_change: u64,
}

impl WaitState {
    pub fn new(my_id: usize, label: String) -> Self {
        Self {
            my_id,
            label,
            last_ahead: None,
            last_state: None,
            last_heartbeat_tick: 0,
            ticks_since_change: 0,
        }
    }

    /// Process one poll tick. `tick` is a monotonically increasing counter
    /// (seconds since wait started). Returns events to surface to the user.
    pub fn observe<'a, I: IntoIterator<Item = &'a Task>>(
        &mut self,
        tick: u64,
        tasks: I,
    ) -> Vec<WaitEvent> {
        let tasks: Vec<&Task> = tasks.into_iter().collect();
        let me = tasks.iter().find(|t| t.id == self.my_id).copied();

        let mut events = Vec::new();

        let state_label = match me.map(|t| &t.status) {
            Some(TaskStatus::Queued { .. }) => "Queued",
            Some(TaskStatus::Running { .. }) => "Running",
            Some(TaskStatus::Done { .. }) => "Done",
            Some(TaskStatus::Paused { .. }) => "Paused",
            _ => "Unknown",
        };

        // First tick: announce the queue position.
        if self.last_state.is_none() && state_label == "Queued" {
            let ahead = count_ahead(tasks.iter().copied(), self.my_id, crate::group::BUSYBEE_GROUP);
            self.last_ahead = Some(ahead);
            events.push(WaitEvent::Line(format!(
                "Task queued: {}. {} ahead.",
                self.label, ahead,
            )));
        }

        // Ahead-count change while still queued.
        if state_label == "Queued" {
            let ahead = count_ahead(tasks.iter().copied(), self.my_id, crate::group::BUSYBEE_GROUP);
            if Some(ahead) != self.last_ahead {
                let mut s = String::new();
                let _ = write!(s, "{ahead} ahead…");
                events.push(WaitEvent::Line(s));
                self.last_ahead = Some(ahead);
                self.ticks_since_change = 0;
            } else {
                self.ticks_since_change += 1;
            }
        }

        // State transition.
        if Some(state_label) != self.last_state {
            if state_label == "Running" {
                events.push(WaitEvent::Line("Running.".into()));
                events.push(WaitEvent::Started);
                self.ticks_since_change = 0;
            }
            if state_label == "Paused" {
                events.push(WaitEvent::Line("Queue paused, waiting…".into()));
            }
            if state_label == "Done" {
                let label = if let Some(Task { status: TaskStatus::Done { result, .. }, .. }) = me {
                    format!("{result:?}")
                } else {
                    "done".into()
                };
                events.push(WaitEvent::Finished { result_label: label });
            }
            // Task disappeared from pueue state (e.g., `pueue clean` or `pueue remove`).
            // Only fire this once we have previously observed the task; a missing task
            // on the very first tick means pueued hasn't seen our Add yet.
            if state_label == "Unknown" && self.last_state.is_some() {
                events.push(WaitEvent::Line("Task disappeared from queue.".into()));
                events.push(WaitEvent::Finished { result_label: "lost".into() });
            }
            self.last_state = Some(state_label);
        }

        // 30s heartbeat if still queued and quiet.
        if state_label == "Queued"
            && self.ticks_since_change >= 30
            && tick - self.last_heartbeat_tick >= 30
        {
            events.push(WaitEvent::Line(format!(
                "still queued ({} ahead)",
                self.last_ahead.unwrap_or(0),
            )));
            self.last_heartbeat_tick = tick;
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use pueue_lib::task::{Task, TaskResult, TaskStatus};

    fn mk(id: usize, status: TaskStatus, group: &str) -> Task {
        let mut t = Task::new(
            format!("cmd {id}"),
            std::path::PathBuf::from("/"),
            Default::default(),
            group.into(),
            status,
            Vec::new(),
            0,
            None,
        );
        t.id = id;
        t
    }

    #[test]
    fn first_tick_announces_queue_position() {
        let mut s = WaitState::new(2, "my build".into());
        let now = Local::now();
        let tasks = [
            mk(1, TaskStatus::Queued { enqueued_at: now }, "busybee"),
            mk(2, TaskStatus::Queued { enqueued_at: now }, "busybee"),
        ];
        let events = s.observe(0, tasks.iter());
        assert_eq!(events[0], WaitEvent::Line("Task queued: my build. 1 ahead.".into()));
    }

    #[test]
    fn ahead_count_decrease_emits_update() {
        let mut s = WaitState::new(2, "x".into());
        let now = Local::now();
        let before = [
            mk(1, TaskStatus::Queued { enqueued_at: now }, "busybee"),
            mk(2, TaskStatus::Queued { enqueued_at: now }, "busybee"),
        ];
        let _ = s.observe(0, before.iter());
        let after = [
            mk(1, TaskStatus::Running { enqueued_at: now, start: now }, "busybee"),
            mk(2, TaskStatus::Queued { enqueued_at: now }, "busybee"),
        ];
        let events = s.observe(1, after.iter());
        assert!(events.iter().any(|e| matches!(e, WaitEvent::Line(s) if s == "0 ahead…")));
    }

    #[test]
    fn transition_to_running_emits_started() {
        let mut s = WaitState::new(1, "x".into());
        let now = Local::now();
        let queued = [mk(1, TaskStatus::Queued { enqueued_at: now }, "busybee")];
        let _ = s.observe(0, queued.iter());
        let running = [mk(1, TaskStatus::Running { enqueued_at: now, start: now }, "busybee")];
        let events = s.observe(1, running.iter());
        assert!(events.iter().any(|e| matches!(e, WaitEvent::Started)));
        assert!(events.iter().any(|e| matches!(e, WaitEvent::Line(s) if s == "Running.")));
    }

    #[test]
    fn transition_to_done_emits_finished() {
        let mut s = WaitState::new(1, "x".into());
        let now = Local::now();
        let running = [mk(1, TaskStatus::Running { enqueued_at: now, start: now }, "busybee")];
        let _ = s.observe(0, running.iter());
        let done = [mk(1, TaskStatus::Done {
            enqueued_at: now, start: now, end: now, result: TaskResult::Success,
        }, "busybee")];
        let events = s.observe(1, done.iter());
        assert!(events.iter().any(|e| matches!(e, WaitEvent::Finished { .. })));
    }

    #[test]
    fn transition_to_missing_task_emits_finished() {
        let mut s = WaitState::new(1, "x".into());
        let now = Local::now();
        let running = [mk(1, TaskStatus::Running { enqueued_at: now, start: now }, "busybee")];
        let _ = s.observe(0, running.iter());
        // Task is now gone (e.g., `pueue clean`).
        let empty: [Task; 0] = [];
        let events = s.observe(1, empty.iter());
        assert!(events.iter().any(|e| matches!(e, WaitEvent::Finished { .. })));
    }
}
