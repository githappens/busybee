//! Takes back what the previous daemon left running, before the socket exists
//! (see `docs/design/bzbd.md` §Failure and recovery, "bzbd dies").

use std::{
    collections::{BTreeMap, BTreeSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use bzb_core::{classify::Class, group::BUSYBEE_GROUP, jobserver::Jobserver};
use pueue_lib::task::{Task, TaskStatus};
use serde::{Deserialize, Serialize};

use crate::{leases::claim_orphans, submit::Pueue};

/// One lease in `leases.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) id: u64,
    pub(crate) label: String,
    pub(crate) argv: Vec<String>,
    pub(crate) class: Class,
    pub(crate) cores_held: u32,
    pub(crate) pueue_task_id: Option<usize>,
    pub(crate) started_at_unix_ms: u64,
    /// Written before the submission goes out, so a restart can match an
    /// unanswered submission to the task pueued started for it.
    pub(crate) submitted_at_unix_ms: Option<u64>,
    /// The fifo the task was pointed at; kept on disk while the task runs,
    /// since a sub-make opens it by path.
    pub(crate) fifo: Option<PathBuf>,
    /// A teardown pueued has not yet confirmed: the task may still hold its
    /// tokens, so a restarted daemon must finish it before admitting.
    pub(crate) killing: bool,
}

pub(crate) struct Recovered {
    /// Already short of what `adopted` and `killing` hold.
    pub(crate) jobserver: Jobserver,
    pub(crate) adopted: Vec<Record>,
    pub(crate) killing: Vec<Record>,
    /// Tokens held beyond a pool that shrank under running tasks; the actor
    /// withholds that many releases.
    pub(crate) debt: u32,
}

pub(crate) async fn recover(dir: &Path, leases_path: &Path, pool_size: u32) -> Result<Recovered> {
    let records = load(leases_path)?;
    // Do not start a pueued just to find nothing to check.
    let (adopted, killing) = if records.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let state = Pueue::default().status().await.with_context(|| {
            format!(
                "cannot cross-check the {} lease(s) in {} against pueued",
                records.len(),
                leases_path.display()
            )
        })?;
        let reconciled = reconcile(records, &state.tasks);
        for (record, reason) in &reconciled.dropped {
            tracing::warn!(
                lease = record.id,
                label = record.label,
                "dropping a recorded lease: {reason}"
            );
        }
        for record in &reconciled.adopted {
            tracing::info!(
                lease = record.id,
                task = record.pueue_task_id,
                cores_held = record.cores_held,
                "adopting a lease the previous daemon left running"
            );
        }
        for record in &reconciled.killing {
            tracing::warn!(
                lease = record.id,
                task = record.pueue_task_id,
                cores_held = record.cores_held,
                "resuming a teardown the previous daemon left in flight"
            );
        }
        (reconciled.adopted, reconciled.killing)
    };

    let referenced: BTreeSet<PathBuf> = adopted
        .iter()
        .chain(&killing)
        .filter_map(|r| r.fifo.clone())
        .collect();
    for path in sweep_fifos(dir, &referenced)? {
        tracing::info!(fifo = %path.display(), "removed a stale fifo");
    }

    let jobserver =
        Jobserver::create(dir, pool_size).context("cannot create the jobserver fifo")?;
    let held: u32 = adopted.iter().chain(&killing).map(|r| r.cores_held).sum();
    let taken = jobserver
        .acquire(held.min(pool_size), Duration::ZERO)
        .context("cannot hold back the adopted leases' tokens")?;
    let debt = held - taken;
    if debt > 0 {
        tracing::error!(
            held,
            pool_size,
            debt,
            "the adopted leases hold more tokens than the pool has; it starts empty"
        );
    }
    Ok(Recovered {
        jobserver,
        adopted,
        killing,
        debt,
    })
}

/// None when the file was never written.
fn load(path: &Path) -> Result<Vec<Record>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("cannot decode the leases in {}", path.display())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

struct Reconciled {
    adopted: Vec<Record>,
    killing: Vec<Record>,
    dropped: Vec<(Record, String)>,
}

/// Keeps the records whose task pueued reports running. A record with a
/// submission time but no task id is first matched to its task the way the
/// poll matches an unanswered submission.
fn reconcile(mut records: Vec<Record>, tasks: &BTreeMap<usize, Task>) -> Reconciled {
    let claimed: BTreeSet<usize> = records.iter().filter_map(|r| r.pueue_task_id).collect();
    claim_orphans(&mut records, tasks, claimed);

    let mut reconciled = Reconciled {
        adopted: Vec::new(),
        killing: Vec::new(),
        dropped: Vec::new(),
    };
    for record in records {
        let reason = match record.pueue_task_id {
            None if record.submitted_at_unix_ms.is_some() => {
                "its submission went unanswered, and pueued has no task for it".to_string()
            }
            None => "it was never admitted, and its client went with the daemon".to_string(),
            Some(task_id) => match tasks.get(&task_id) {
                None => format!("pueue task {task_id} is gone"),
                // pueued's state was reset while no daemon ran: the id is now
                // somebody else's task, which `busybee cancel` must not reach.
                Some(task)
                    if task.group != BUSYBEE_GROUP
                        || task.label.as_deref() != Some(&record.label) =>
                {
                    format!(
                        "pueue task {task_id} is not the recorded task: it is {:?} in group {:?}, \
                         so pueued's state was reset",
                        task.label, task.group
                    )
                }
                Some(task) => match &task.status {
                    TaskStatus::Running { .. } if record.killing => {
                        reconciled.killing.push(record);
                        continue;
                    }
                    TaskStatus::Running { .. } => {
                        reconciled.adopted.push(record);
                        continue;
                    }
                    TaskStatus::Done { .. } if record.killing => {
                        format!("pueue task {task_id} was torn down while no daemon was watching")
                    }
                    TaskStatus::Done { result, .. } => format!(
                        "pueue task {task_id} finished ({result:?}) while no daemon was watching; \
                         its exit code reached nobody"
                    ),
                    other => format!("pueue task {task_id} is {other:?}, not running"),
                },
            },
        };
        reconciled.dropped.push((record, reason));
    }
    reconciled
}

/// Unlinks every `jobserver-<pid>` in `dir` whose daemon is dead and that no
/// recovered record still points at. Returns what it removed.
fn sweep_fifos(dir: &Path, referenced: &BTreeSet<PathBuf>) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("cannot list {}", dir.display()))?;
    for entry in entries {
        let path = entry
            .with_context(|| format!("cannot list {}", dir.display()))?
            .path();
        let Some(pid) = fifo_pid(&path) else {
            continue;
        };
        if referenced.contains(&path) || pid_alive(pid) {
            continue;
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("cannot remove the stale fifo {}", path.display()))?;
        removed.push(path);
    }
    Ok(removed)
}

fn fifo_pid(path: &Path) -> Option<u32> {
    path.file_name()?
        .to_str()?
        .strip_prefix("jobserver-")?
        .parse()
        .ok()
}

/// `EPERM` counts as alive. Our own pid does not: a fifo bearing it predates
/// us, since pids are reused and ours is not created yet.
fn pid_alive(pid: u32) -> bool {
    // `kill(0, …)` would ask about our process group rather than a process.
    let Some(pid) = libc::pid_t::try_from(pid).ok().filter(|p| *p > 0) else {
        return false;
    };
    if pid as u32 == std::process::id() {
        return false;
    }
    // SAFETY: signal 0 delivers nothing; the call only checks the pid.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{done, running, task, tasks};
    use chrono::Local;
    use pueue_lib::task::TaskResult;

    fn record(id: u64, pueue_task_id: Option<usize>) -> Record {
        Record {
            id,
            label: "sleep 5".into(),
            argv: vec!["sleep".into(), "5".into()],
            class: Class::None,
            cores_held: 0,
            pueue_task_id,
            started_at_unix_ms: 0,
            submitted_at_unix_ms: None,
            fifo: None,
            killing: false,
        }
    }

    fn sleep5(id: usize, status: TaskStatus) -> Task {
        task(id, "sleep 5", status)
    }

    #[test]
    fn only_records_whose_task_is_running_are_adopted() {
        let state = tasks(vec![
            sleep5(1, running()),
            sleep5(2, done(TaskResult::Success)),
        ]);
        let records = vec![
            record(10, Some(1)),
            record(11, Some(2)),
            record(12, Some(3)),
            record(13, None),
        ];

        let reconciled = reconcile(records, &state);

        assert_eq!(
            reconciled.adopted.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![10]
        );
        let dropped: Vec<(u64, &str)> = reconciled
            .dropped
            .iter()
            .map(|(r, reason)| (r.id, reason.as_str()))
            .collect();
        assert_eq!(dropped.len(), 3, "dropped {dropped:?}");
        assert!(dropped[0].1.contains("finished"), "{:?}", dropped[0]);
        assert!(dropped[1].1.contains("gone"), "{:?}", dropped[1]);
        assert!(dropped[2].1.contains("never admitted"), "{:?}", dropped[2]);
    }

    #[test]
    fn a_recorded_id_that_names_another_task_is_not_adopted() {
        let mut relabelled = sleep5(1, running());
        relabelled.label = Some("cargo build".into());
        let mut regrouped = sleep5(2, running());
        regrouped.group = "default".into();
        let state = tasks(vec![relabelled, regrouped, sleep5(3, running())]);
        let records = vec![
            record(10, Some(1)),
            record(11, Some(2)),
            record(12, Some(3)),
        ];

        let reconciled = reconcile(records, &state);

        assert_eq!(
            reconciled.adopted.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![12]
        );
        let dropped: Vec<(u64, &str)> = reconciled
            .dropped
            .iter()
            .map(|(r, reason)| (r.id, reason.as_str()))
            .collect();
        assert_eq!(dropped.len(), 2, "dropped {dropped:?}");
        assert_eq!(dropped[0].0, 10);
        assert!(
            dropped[0].1.contains("not the recorded task"),
            "{:?}",
            dropped[0]
        );
        assert_eq!(dropped[1].0, 11);
        assert!(
            dropped[1].1.contains("not the recorded task"),
            "{:?}",
            dropped[1]
        );
    }

    #[test]
    fn a_teardown_in_flight_is_resumed_not_adopted() {
        let state = tasks(vec![
            sleep5(1, running()),
            sleep5(2, done(TaskResult::Success)),
        ]);
        let mut ignoring = record(10, Some(1));
        ignoring.killing = true;
        let mut finished = record(11, Some(2));
        finished.killing = true;

        let reconciled = reconcile(vec![ignoring, finished], &state);

        assert!(reconciled.adopted.is_empty(), "adopted a teardown");
        assert_eq!(
            reconciled.killing.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![10]
        );
        assert_eq!(reconciled.dropped.len(), 1);
        assert_eq!(reconciled.dropped[0].0.id, 11);
        assert!(
            reconciled.dropped[0].1.contains("torn down"),
            "{:?}",
            reconciled.dropped[0].1
        );
    }

    /// Spec table row "bzbd dies", between `pueue.add` and recording its id.
    #[test]
    fn a_submission_in_flight_when_the_daemon_died_is_matched_to_its_task() {
        let sent = Local::now();
        let build = |id: usize| {
            let mut task = sleep5(id, running());
            task.label = Some("cargo build".into());
            task.created_at = sent;
            task
        };
        let state = tasks(vec![build(4), build(5)]);
        let submitted = |id: u64| {
            let mut record = record(id, None);
            record.label = "cargo build".into();
            record.submitted_at_unix_ms = Some(sent.timestamp_millis() as u64);
            record
        };
        let mut claimed = record(11, Some(5));
        claimed.label = "cargo build".into();
        // 10 gets the task 11 does not claim; nothing is left for 12.
        let records = vec![submitted(10), claimed, submitted(12), record(13, None)];

        let reconciled = reconcile(records, &state);

        let adopted: Vec<(u64, Option<usize>)> = reconciled
            .adopted
            .iter()
            .map(|r| (r.id, r.pueue_task_id))
            .collect();
        assert_eq!(adopted, vec![(10, Some(4)), (11, Some(5))]);
        let dropped: Vec<(u64, &str)> = reconciled
            .dropped
            .iter()
            .map(|(r, reason)| (r.id, reason.as_str()))
            .collect();
        assert_eq!(dropped.len(), 2, "dropped {dropped:?}");
        assert_eq!(dropped[0].0, 12);
        assert!(dropped[0].1.contains("submission"), "{:?}", dropped[0]);
        assert_eq!(dropped[1].0, 13);
        assert!(dropped[1].1.contains("never admitted"), "{:?}", dropped[1]);
    }

    #[test]
    fn only_jobserver_files_carry_a_pid() {
        assert_eq!(fifo_pid(Path::new("/state/jobserver-4242")), Some(4242));
        assert_eq!(fifo_pid(Path::new("/state/jobserver-x")), None);
        assert_eq!(fifo_pid(Path::new("/state/bzbd.sock")), None);
    }

    #[test]
    fn liveness_counts_every_process_but_this_one() {
        assert!(pid_alive(unsafe { libc::getppid() } as u32));
        assert!(!pid_alive(std::process::id()));
        assert!(!pid_alive(i32::MAX as u32));
        assert!(!pid_alive(0));
    }
}
