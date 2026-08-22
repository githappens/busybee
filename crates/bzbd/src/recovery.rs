//! What a starting daemon does about the one before it
//! (`docs/design/bzbd.md` §Failure and recovery, "bzbd dies").
//!
//! The tasks the previous daemon left running are pueued's children and are
//! still on the machine. Their leases are taken back from `leases.json`,
//! cross-checked against what pueued says is actually running, and this
//! happens before the socket exists: a lease admitted first would be sized as
//! though the machine were idle, and its persist would overwrite the only
//! record of what is really on it. The previous daemon's fifo is left to the
//! tasks that hold it open and unlinked once they are gone; a fresh one is
//! seeded with the tokens the adopted leases do not hold.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use bzb_core::{classify::Class, jobserver::Jobserver};
use pueue_lib::task::{Task, TaskStatus};
use serde::{Deserialize, Serialize};

use crate::submit::Pueue;

/// What one lease looks like in `leases.json`: enough to take it back after a
/// restart and to keep reporting it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: u64,
    pub label: String,
    /// The command as received, which is what `busybee status` classifies for
    /// its tool column. Absent from records written before this field.
    #[serde(default)]
    pub argv: Vec<String>,
    pub class: Class,
    pub cores_held: u32,
    pub pueue_task_id: Option<usize>,
    pub started_at_unix_ms: u64,
    /// The fifo the task was pointed at. It stays on disk while the task
    /// runs — a sub-make opens the path anew — and goes once it is done.
    /// Absent from records written before this field.
    #[serde(default)]
    pub fifo: Option<PathBuf>,
    /// A teardown in flight: the lease is over and its task was signalled,
    /// but pueued has not confirmed the task gone. It stays on record until
    /// then, because a task that ignores the signal is still on the machine,
    /// tokens and all, and a daemon restarted meanwhile must finish the job
    /// rather than admit the next lease beside it. Absent from records
    /// written before this field.
    #[serde(default)]
    pub killing: bool,
}

/// What the lease actor starts from.
pub struct Recovered {
    /// This daemon's pool, already short of what the adopted leases and the
    /// teardowns in flight hold.
    pub jobserver: Jobserver,
    /// The previous daemon's leases whose tasks are still running.
    pub adopted: Vec<Record>,
    /// The previous daemon's teardowns whose tasks are still running.
    pub killing: Vec<Record>,
    /// Tokens the records hold beyond what the pool has room for: the pool
    /// shrank under running tasks. The actor swallows that many of their
    /// releases, so the fifo never holds more than `pool_size`.
    pub debt: u32,
}

pub async fn recover(dir: &Path, leases_path: &Path, pool_size: u32) -> Result<Recovered> {
    let records = load(leases_path)?;
    // Only a record makes pueued worth asking: a daemon with nothing to check
    // must not start a pueued just to start itself.
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

    // Seeded with `pool_size − Σ cores_held`: created whole, then the held
    // part is taken straight back out.
    let jobserver =
        Jobserver::create(dir, pool_size).context("cannot create the jobserver fifo")?;
    let held: u32 = adopted.iter().chain(&killing).map(|r| r.cores_held).sum();
    let taken = jobserver
        .acquire(held.min(pool_size), Duration::ZERO)
        .context("cannot hold back the adopted leases' tokens")?;
    let debt = held - taken;
    if debt > 0 {
        // The pool shrank under running tasks. Nothing is revoked: the pool
        // starts empty and fills as they end, the same as a reload would do,
        // except that the first `debt` tokens they return are not put back.
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

/// The records in `leases.json`; none when the file was never written.
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
    /// Teardowns still to be confirmed, whose tasks pueued reports running.
    killing: Vec<Record>,
    /// What was not, and why.
    dropped: Vec<(Record, String)>,
}

/// Keeps the records whose task pueued reports running: a lease is adopted, a
/// teardown is resumed. Everything else is over: a task that finished had
/// nobody to report to, a task pueued no longer knows about is not on the
/// machine as far as anyone can tell, and a lease that was never admitted
/// lost its client with the daemon.
fn reconcile(records: Vec<Record>, tasks: &BTreeMap<usize, Task>) -> Reconciled {
    let mut reconciled = Reconciled {
        adopted: Vec::new(),
        killing: Vec::new(),
        dropped: Vec::new(),
    };
    for record in records {
        let reason = match record.pueue_task_id {
            None => "it was never admitted, and its client went with the daemon".to_string(),
            Some(task_id) => match tasks.get(&task_id).map(|t| &t.status) {
                Some(TaskStatus::Running { .. }) if record.killing => {
                    reconciled.killing.push(record);
                    continue;
                }
                Some(TaskStatus::Running { .. }) => {
                    reconciled.adopted.push(record);
                    continue;
                }
                Some(TaskStatus::Done { .. }) if record.killing => {
                    format!("pueue task {task_id} was torn down while no daemon was watching")
                }
                Some(TaskStatus::Done { result, .. }) => format!(
                    "pueue task {task_id} finished ({result:?}) while no daemon was watching; \
                     its exit code reached nobody"
                ),
                Some(other) => format!("pueue task {task_id} is {other:?}, not running"),
                None => format!("pueue task {task_id} is gone"),
            },
        };
        reconciled.dropped.push((record, reason));
    }
    reconciled
}

/// Unlinks every `jobserver-<pid>` in `dir` whose daemon is dead and that no
/// adopted lease still points a task at. Returns what it removed.
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

/// The pid in a `jobserver-<pid>` file name; `None` for anything else.
fn fifo_pid(path: &Path) -> Option<u32> {
    path.file_name()?
        .to_str()?
        .strip_prefix("jobserver-")?
        .parse()
        .ok()
}

/// Whether `pid` names a live process. One that exists but is somebody
/// else's answers `EPERM`, and still counts. Our own pid does not: a fifo
/// bearing it predates us — ours is not created yet — and pids are reused.
fn pid_alive(pid: u32) -> bool {
    let Some(pid) = libc::pid_t::try_from(pid).ok().filter(|p| *p > 0) else {
        // Not a pid, or pid 0: `kill(0, …)` would ask about our own process
        // group rather than a process.
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
    use chrono::Local;
    use pueue_lib::task::TaskResult;

    fn record(id: u64, pueue_task_id: Option<usize>) -> Record {
        Record {
            id,
            label: format!("task {id}"),
            argv: vec!["sleep".into(), "5".into()],
            class: Class::None,
            cores_held: 0,
            pueue_task_id,
            started_at_unix_ms: 0,
            fifo: None,
            killing: false,
        }
    }

    fn task(id: usize, status: TaskStatus) -> Task {
        let mut task = Task::new(
            "sleep 5".into(),
            PathBuf::from("/tmp"),
            Default::default(),
            "busybee".into(),
            status,
            Vec::new(),
            0,
            None,
        );
        task.id = id;
        task
    }

    fn tasks(tasks: Vec<Task>) -> BTreeMap<usize, Task> {
        tasks.into_iter().map(|t| (t.id, t)).collect()
    }

    fn running() -> TaskStatus {
        let now = Local::now();
        TaskStatus::Running {
            enqueued_at: now,
            start: now,
        }
    }

    fn done() -> TaskStatus {
        let now = Local::now();
        TaskStatus::Done {
            enqueued_at: now,
            start: now,
            end: now,
            result: TaskResult::Success,
        }
    }

    /// Only a task pueued still reports running is on the machine; every
    /// other record is over, each for a reason the log can say.
    #[test]
    fn only_records_whose_task_is_running_are_adopted() {
        let state = tasks(vec![task(1, running()), task(2, done())]);
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

    /// A teardown the previous daemon did not see through is not a lease to
    /// take back: the task is on its way out, not on the books. It is
    /// resumed while pueued still reports the task, and over once it does
    /// not.
    #[test]
    fn a_teardown_in_flight_is_resumed_not_adopted() {
        let state = tasks(vec![task(1, running()), task(2, done())]);
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

    #[test]
    fn only_jobserver_files_carry_a_pid() {
        assert_eq!(fifo_pid(Path::new("/state/jobserver-4242")), Some(4242));
        assert_eq!(fifo_pid(Path::new("/state/jobserver-x")), None);
        assert_eq!(fifo_pid(Path::new("/state/bzbd.sock")), None);
    }

    /// Our own pid is the one live pid a leftover fifo must not be kept for:
    /// the fifo predates this process and ours is about to be created at
    /// the same path.
    #[test]
    fn liveness_counts_every_process_but_this_one() {
        assert!(pid_alive(unsafe { libc::getppid() } as u32));
        assert!(!pid_alive(std::process::id()));
        assert!(!pid_alive(i32::MAX as u32));
        assert!(!pid_alive(0));
    }

    /// A `leases.json` written before `argv` and `fifo` existed still loads:
    /// the daemon that wrote it is the one being upgraded over.
    #[test]
    fn a_record_without_the_newer_fields_decodes() {
        let old = r#"[{"id":3,"label":"make","class":"none","cores_held":0,
                      "pueue_task_id":7,"started_at_unix_ms":1}]"#;
        let records: Vec<Record> = serde_json::from_str(old).expect("decode");
        assert_eq!(records[0].argv, Vec::<String>::new());
        assert_eq!(records[0].fifo, None);
        assert!(!records[0].killing);
    }
}
