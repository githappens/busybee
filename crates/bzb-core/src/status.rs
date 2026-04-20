use pueue_lib::task::{Task, TaskStatus};

/// A minimal, display-friendly view of the busybee group's state, derived
/// from a full pueue `State`. Stays agnostic of the ratatui layer.
#[derive(Debug, Clone)]
pub struct QueueSnapshot {
    pub running: Option<TaskView>,
    pub queued: Vec<TaskView>,
}

#[derive(Debug, Clone)]
pub struct TaskView {
    pub id: usize,
    pub label: Option<String>,
    pub command: String,
    pub path: std::path::PathBuf,
}

impl QueueSnapshot {
    /// Build a snapshot from pueue tasks, filtered to `group`.
    pub fn from_tasks<'a, I: IntoIterator<Item = &'a Task>>(tasks: I, group: &str) -> Self {
        let mut running = None;
        let mut queued = Vec::new();
        let mut tasks: Vec<&Task> = tasks
            .into_iter()
            .filter(|t| t.group == group)
            .collect();
        tasks.sort_by_key(|t| t.id);
        for t in tasks {
            let view = TaskView {
                id: t.id,
                label: t.label.clone(),
                command: t.command.clone(),
                path: t.path.clone(),
            };
            match t.status {
                TaskStatus::Running { .. } => running = Some(view),
                TaskStatus::Queued { .. } => queued.push(view),
                _ => {}
            }
        }
        Self { running, queued }
    }
}

/// Count the tasks ahead of `my_id` in the busybee queue for blocking-mode
/// status messages. "Ahead" = queued in the same group with a strictly
/// smaller task id (pueue's natural FIFO within a group).
pub fn count_ahead<'a, I: IntoIterator<Item = &'a Task>>(
    tasks: I,
    my_id: usize,
    group: &str,
) -> usize {
    tasks
        .into_iter()
        .filter(|t| t.group == group)
        .filter(|t| matches!(t.status, TaskStatus::Queued { .. }))
        .filter(|t| t.id < my_id)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use pueue_lib::task::{Task, TaskStatus};

    fn queued(id: usize, group: &str) -> Task {
        let mut t = Task::new(
            format!("cmd {id}"),
            std::path::PathBuf::from("/"),
            Default::default(),
            group.into(),
            TaskStatus::Queued { enqueued_at: Local::now() },
            Vec::new(),
            0,
            None,
        );
        t.id = id;
        t
    }

    fn running(id: usize, group: &str) -> Task {
        let mut t = queued(id, group);
        let now = Local::now();
        t.status = TaskStatus::Running { enqueued_at: now, start: now };
        t
    }

    #[test]
    fn count_ahead_is_zero_when_first() {
        let tasks = [queued(1, "busybee"), queued(2, "busybee")];
        assert_eq!(count_ahead(&tasks, 1, "busybee"), 0);
    }

    #[test]
    fn count_ahead_counts_lower_queued_ids_same_group() {
        let tasks = [queued(1, "busybee"), queued(2, "busybee"), queued(3, "busybee")];
        assert_eq!(count_ahead(&tasks, 3, "busybee"), 2);
    }

    #[test]
    fn count_ahead_ignores_other_groups() {
        let tasks = [queued(1, "default"), queued(2, "busybee")];
        assert_eq!(count_ahead(&tasks, 2, "busybee"), 0);
    }

    #[test]
    fn count_ahead_ignores_running_tasks() {
        let tasks = [running(1, "busybee"), queued(2, "busybee")];
        assert_eq!(count_ahead(&tasks, 2, "busybee"), 0);
    }

    #[test]
    fn snapshot_separates_running_and_queued() {
        let tasks = [running(1, "busybee"), queued(2, "busybee"), queued(3, "busybee")];
        let snap = QueueSnapshot::from_tasks(tasks.iter(), "busybee");
        assert_eq!(snap.running.as_ref().map(|v| v.id), Some(1));
        assert_eq!(snap.queued.len(), 2);
        assert_eq!(snap.queued[0].id, 2); // sorted by id
    }

    #[test]
    fn snapshot_carries_task_cwd() {
        let now = Local::now();
        let mut t = Task::new(
            "cmake --build".into(),
            std::path::PathBuf::from("/tmp/work"),
            Default::default(),
            "busybee".into(),
            TaskStatus::Running { enqueued_at: now, start: now },
            Vec::new(),
            0,
            None,
        );
        t.id = 1;
        let tasks = [t];
        let snap = QueueSnapshot::from_tasks(tasks.iter(), "busybee");
        assert_eq!(
            snap.running.as_ref().unwrap().path,
            std::path::PathBuf::from("/tmp/work"),
        );
    }
}
