//! The lease actor: one task owning the admission machine, the live leases and
//! the connection to pueued.
//!
//! Everything that changes a lease happens here, in one place and in one
//! thread, so the [`Scheduler`](bzb_core::scheduler::Scheduler)'s accounting
//! and the tasks pueued is actually running cannot drift apart. Connections
//! reach it through a [`Handle`]; it answers on a channel per lease.
//!
//! Lifecycle, from `docs/design/bzbd.md` §Lease model: `Queued` →
//! `Admitted { pueue_task_id, class, cores }` → `Finished { exit_code }`. A
//! lease ends when pueued reports the task `Done`, or when the requesting
//! client's connection drops — before admission it is simply dropped from the
//! queue, after admission its task is killed first.

use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use bzb_core::{
    classify::Class,
    enqueue::{shell_escape_join, TaskSpec},
    exit_code::task_result_to_exit_code,
    protocol::{LeaseEvent, LeaseRequest, LeaseView, StatusReply},
    scheduler::{Action, Event, LeaseId, Params, Request as LeaseSpec, Scheduler},
};
use pueue_lib::{message::Signal, task::TaskStatus};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

use crate::submit::Pueue;

/// How often pueued is asked what its tasks are doing. `docs/design/bzbd.md`
/// §Failure and recovery fixes the cadence at one second — the same one the
/// client polled at before the broker.
const POLL: Duration = Duration::from_secs(1);

/// How long a task gets to honour SIGTERM before SIGKILL follows. The
/// escalation is the client's from before the broker, on a timer rather than
/// on a second Ctrl-C.
const KILL_GRACE: Duration = Duration::from_secs(1);

/// The exit code a lease reports when its own client hung up: nobody is left
/// to read it, but the code has to say the task did not finish on its own.
const KILLED: i32 = 130;

/// Where a lease's events go: the connection that asked for it.
pub type Events = mpsc::UnboundedSender<LeaseEvent>;

/// What a connection asks the actor for.
pub enum Command {
    Submit {
        request: Box<LeaseRequest>,
        events: Events,
        id: oneshot::Sender<LeaseId>,
    },
    /// The connection went away; the lease goes with it.
    Hangup(LeaseId),
    Status(oneshot::Sender<StatusReply>),
}

/// Talks to the actor. Cloneable; one per connection.
#[derive(Clone)]
pub struct Handle(mpsc::Sender<Command>);

impl Handle {
    pub async fn submit(&self, request: LeaseRequest, events: Events) -> Result<LeaseId> {
        let (tx, rx) = oneshot::channel();
        self.send(Command::Submit {
            request: Box::new(request),
            events,
            id: tx,
        })
        .await?;
        rx.await.context("the lease actor dropped a submission")
    }

    pub async fn hangup(&self, lease: LeaseId) -> Result<()> {
        self.send(Command::Hangup(lease)).await
    }

    pub async fn status(&self) -> Result<StatusReply> {
        let (tx, rx) = oneshot::channel();
        self.send(Command::Status(tx)).await?;
        rx.await.context("the lease actor dropped a status request")
    }

    async fn send(&self, command: Command) -> Result<()> {
        self.0
            .send(command)
            .await
            .map_err(|_| anyhow::anyhow!("the lease actor is gone"))
    }
}

/// One live lease.
struct Lease {
    request: LeaseRequest,
    /// The connection that owns it: dropping this end is how the client says
    /// the lease is over.
    conn: Events,
    class: Class,
    pueue_task_id: Option<usize>,
    cores_held: u32,
    started_at: SystemTime,
}

impl Lease {
    /// What `busybee status` and pueue call the task.
    fn label(&self) -> String {
        self.request
            .label
            .clone()
            .unwrap_or_else(|| shell_escape_join(&self.request.argv))
    }
}

pub struct Leases {
    scheduler: Scheduler,
    params: Params,
    leases: BTreeMap<LeaseId, Lease>,
    next_id: u64,
    pueue: Pueue,
    /// pueue tasks that have been sent SIGTERM, and when SIGKILL follows.
    killing: BTreeMap<usize, Instant>,
    leases_path: PathBuf,
}

impl Leases {
    /// Builds the actor and the handle connections use to reach it.
    pub fn new(params: Params, leases_path: PathBuf) -> (Self, Handle, mpsc::Receiver<Command>) {
        let (tx, rx) = mpsc::channel(64);
        let actor = Self {
            scheduler: Scheduler::new(params),
            params,
            leases: BTreeMap::new(),
            next_id: 1,
            pueue: Pueue::default(),
            killing: BTreeMap::new(),
            leases_path,
        };
        (actor, Handle(tx), rx)
    }

    pub async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        let mut ticker = tokio::time::interval(POLL);
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.command(command).await,
                    // Every connection is gone and so is the server: nothing
                    // can reach us again.
                    None => break,
                },
                _ = ticker.tick() => self.poll().await,
            }
        }
    }

    async fn command(&mut self, command: Command) {
        match command {
            Command::Submit {
                request,
                events,
                id,
            } => self.submit(*request, events, id).await,
            Command::Hangup(lease) => self.hangup(lease).await,
            Command::Status(reply) => {
                // The receiver is gone only if the connection died while we
                // were composing the answer.
                let _ = reply.send(self.status());
            }
        }
    }

    async fn submit(
        &mut self,
        request: LeaseRequest,
        events: Events,
        reply: oneshot::Sender<LeaseId>,
    ) {
        let id = LeaseId(self.next_id);
        self.next_id += 1;
        // Everything else in the map was submitted before this one, and the
        // queue is FIFO: they are exactly what it is waiting behind.
        let ahead = self.leases.len();
        // Classification arrives with the injection work; until then every
        // task is exclusive, which is what busybee did before the broker.
        let class = Class::None;
        let lease = Lease {
            request,
            conn: events,
            class,
            pueue_task_id: None,
            cores_held: 0,
            started_at: SystemTime::now(),
        };
        let spec = LeaseSpec {
            id,
            class,
            cores_wanted: lease.request.cores_wanted,
            label: lease.label(),
        };
        self.leases.insert(id, lease);
        self.persist();
        if reply.send(id).is_err() {
            // The connection died between asking and hearing back; it can no
            // longer hang up on us, so end the lease here.
            self.leases.remove(&id);
            self.persist();
            return;
        }
        self.send(id, LeaseEvent::Queued { id: id.0, ahead });

        let actions = self.scheduler.handle(Event::Submit(spec));
        // The machine's own notification for this lease would repeat the
        // `Queued` just sent by hand.
        let actions = actions
            .into_iter()
            .filter(|a| !matches!(a, Action::Notify { id: notified, .. } if *notified == id))
            .collect();
        self.drive(actions).await;
    }

    /// The connection is gone: drop the lease, killing its task if it has one.
    async fn hangup(&mut self, id: LeaseId) {
        let actions = self.scheduler.handle(Event::Cancel(id));
        self.drive(actions).await;
        // A queued lease gets no `Drop` action — there is nothing to tear
        // down — so it is dropped here.
        if self.leases.remove(&id).is_some() {
            self.persist();
        }
    }

    /// Performs the actions the machine asked for, feeding it back what came
    /// of them until it has nothing more to say.
    async fn drive(&mut self, actions: Vec<Action>) {
        let mut pending: VecDeque<Action> = actions.into();
        while let Some(action) = pending.pop_front() {
            match action {
                Action::Admit { id, cores, .. } => pending.extend(self.admit(id, cores).await),
                Action::Notify { id, ahead } => {
                    // The machine counts queue positions; a client wants to
                    // know how many tasks are in front of it, running ones
                    // included.
                    let ahead = ahead + self.scheduler.snapshot().admitted.len();
                    self.send(id, LeaseEvent::Queued { id: id.0, ahead });
                }
                Action::Drop(id) => self.drop_lease(id).await,
            }
        }
    }

    /// Submits an admitted lease to pueued and tells its client it is running.
    async fn admit(&mut self, id: LeaseId, cores: Option<u32>) -> Vec<Action> {
        let Some(lease) = self.leases.get(&id) else {
            // The actor is the only thing that removes leases, and it does not
            // do so between an admission and this call.
            tracing::error!(lease = id.0, "admitted a lease that no longer exists");
            return self.scheduler.handle(Event::DrainFailed(id));
        };
        let spec = TaskSpec {
            command: shell_escape_join(&lease.request.argv),
            cwd: lease.request.cwd.clone(),
            env: lease.request.env.clone(),
            label: Some(lease.label()),
            // The group is at `parallel_tasks = 0`, so pueue's dispatcher will
            // never start it: admission is bzbd's decision and it has been made.
            start_immediately: true,
        };
        let class = lease.class;

        let task_id = match self.pueue.add(spec).await {
            Ok(task_id) => task_id,
            Err(err) => {
                tracing::error!(lease = id.0, "cannot submit to pueued: {err:#}");
                let actions = self.scheduler.handle(Event::DrainFailed(id));
                // No task was created, so the machine's teardown has nothing
                // to act on; the lease ends here instead, and never silently:
                // the client hears why its command is not going to run.
                let actions = actions
                    .into_iter()
                    .filter(|a| !matches!(a, Action::Drop(dropped) if *dropped == id))
                    .collect();
                self.send(
                    id,
                    LeaseEvent::Notice {
                        text: format!("bzbd could not start the task: {err}"),
                    },
                );
                self.finish(id, 1);
                return actions;
            }
        };

        let lease = self.leases.get_mut(&id).expect("checked above");
        lease.pueue_task_id = Some(task_id);
        // Still none: the fifo drain arrives with the injection work, and an
        // exclusive lease owns the machine without holding a token either way.
        let cores_held = lease.cores_held;
        self.persist();

        self.send(
            id,
            LeaseEvent::Admitted {
                id: id.0,
                pueue_task_id: task_id,
                class: class.as_str().to_string(),
                // A jobserver lease is told the fair share the machine sized
                // it at; anything else is told the tokens it actually holds,
                // with the implicit one as the minimum.
                cores: cores.unwrap_or(cores_held.max(1)),
                pool_size: self.params.pool_size,
                peers: self.scheduler.snapshot().admitted.len().saturating_sub(1),
            },
        );
        self.scheduler.handle(Event::Started { id, cores_held })
    }

    /// Tears a lease down: its task is killed and its client told the lease is
    /// over. Used for a client that hung up, and for a task that went live
    /// after its lease was already gone.
    async fn drop_lease(&mut self, id: LeaseId) {
        let Some(lease) = self.leases.remove(&id) else {
            tracing::warn!(lease = id.0, "asked to drop a lease that is not tracked");
            return;
        };
        self.persist();
        if let Some(task_id) = lease.pueue_task_id {
            if let Err(err) = self.pueue.kill(task_id, Signal::SigTerm).await {
                tracing::error!(task = task_id, "cannot stop the task: {err:#}");
            }
            // SIGKILL follows on the poll after the grace period, for a task
            // that ignores the first signal.
            self.killing.insert(task_id, Instant::now() + KILL_GRACE);
        }
        let _ = lease.conn.send(LeaseEvent::Finished {
            id: id.0,
            exit_code: KILLED,
        });
    }

    /// One tick: ask pueued about every task we are waiting on, and act on the
    /// ones that ended.
    async fn poll(&mut self) {
        let watching = self.leases.values().any(|l| l.pueue_task_id.is_some());
        if !watching && self.killing.is_empty() {
            return;
        }
        let state = match self.pueue.status().await {
            Ok(state) => state,
            Err(err) => {
                // Loud, and retried on the next tick: a poll that cannot run
                // leaves leases running, not finished.
                tracing::error!("cannot poll pueued: {err:#}");
                return;
            }
        };
        let status = |task_id: usize| state.tasks.get(&task_id).map(|t| &t.status);

        for (task_id, deadline) in std::mem::take(&mut self.killing) {
            match status(task_id) {
                // Gone or done: the escalation is over either way.
                None | Some(TaskStatus::Done { .. }) => {}
                _ if Instant::now() >= deadline => {
                    tracing::warn!(task = task_id, "still running after SIGTERM; killing");
                    if let Err(err) = self.pueue.kill(task_id, Signal::SigKill).await {
                        tracing::error!(task = task_id, "cannot kill the task: {err:#}");
                    }
                }
                _ => {
                    self.killing.insert(task_id, deadline);
                }
            }
        }

        let ended: Vec<(LeaseId, Completion)> = self
            .leases
            .iter()
            .filter_map(|(id, lease)| {
                let task_id = lease.pueue_task_id?;
                Some((*id, completion(task_id, status(task_id))?))
            })
            .collect();
        for (id, completion) in ended {
            if let Some(text) = completion.notice {
                self.send(id, LeaseEvent::Notice { text });
            }
            self.finish(id, completion.exit_code);
            let actions = self.scheduler.handle(Event::Finished(id));
            self.drive(actions).await;
        }
    }

    /// Ends a lease that is over: it leaves the books before its client hears
    /// about it, so a client that asks for the status it was just told about
    /// cannot see the lease it already knows is finished.
    fn finish(&mut self, id: LeaseId, exit_code: i32) {
        let Some(lease) = self.leases.remove(&id) else {
            return;
        };
        self.persist();
        let _ = lease.conn.send(LeaseEvent::Finished {
            id: id.0,
            exit_code,
        });
    }

    fn send(&self, id: LeaseId, event: LeaseEvent) {
        let Some(lease) = self.leases.get(&id) else {
            return;
        };
        if lease.conn.send(event).is_err() {
            // The connection task is on its way out and will hang up; nothing
            // to do here but note it.
            tracing::debug!(lease = id.0, "the client is no longer reading its events");
        }
    }

    fn status(&self) -> StatusReply {
        let snapshot = self.scheduler.snapshot();
        let admitted_count = snapshot.admitted.len();
        let view = |id: LeaseId, class: Class, cores: u32, state: &str, ahead: Option<usize>| {
            let lease = self.leases.get(&id);
            LeaseView {
                id: id.0,
                label: lease.map(Lease::label).unwrap_or_default(),
                class: class.as_str().to_string(),
                cores,
                state: state.to_string(),
                elapsed_ms: lease.map_or(0, |l| elapsed_ms(l.started_at)),
                ahead,
                pueue_task_id: lease.and_then(|l| l.pueue_task_id),
            }
        };
        let leases = snapshot
            .admitted
            .iter()
            .map(|l| view(l.id, l.class, l.cores_held, "running", None))
            .chain(
                snapshot
                    .queued
                    .iter()
                    .enumerate()
                    .map(|(i, r)| view(r.id, r.class, 0, "queued", Some(admitted_count + i))),
            )
            .collect();
        StatusReply {
            pool_size: self.params.pool_size,
            free: snapshot.free_estimate,
            held: snapshot.admitted.iter().map(|l| l.cores_held).sum(),
            leases,
        }
    }

    /// Rewrites `leases.json`, which is what a restarted bzbd reads to find the
    /// tasks it left running.
    fn persist(&self) {
        let records: Vec<Record> = self
            .leases
            .iter()
            .map(|(id, lease)| Record {
                id: id.0,
                label: lease.label(),
                class: lease.class.as_str(),
                cores_held: lease.cores_held,
                pueue_task_id: lease.pueue_task_id,
                started_at_unix_ms: unix_ms(lease.started_at),
            })
            .collect();
        if let Err(err) = write_json(&self.leases_path, &records) {
            // Not fatal to the leases themselves, but a restart would forget
            // them, so it is never swallowed.
            tracing::error!("cannot record the leases: {err:#}");
        }
    }
}

/// What one lease looks like in `leases.json`.
#[derive(Serialize)]
struct Record {
    id: u64,
    label: String,
    class: &'static str,
    cores_held: u32,
    pueue_task_id: Option<usize>,
    started_at_unix_ms: u64,
}

/// Writes through a temporary file: a half-written `leases.json` is worse than
/// an old one, because the restart that reads it is exactly when it matters.
fn write_json(path: &Path, records: &[Record]) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    let encoded = serde_json::to_vec(records).context("cannot encode the leases")?;
    std::fs::write(&temporary, encoded)
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("cannot replace {}", path.display()))
}

/// What a poll of pueue's task list says about a lease's task; `None` while it
/// is still going.
#[derive(Debug, PartialEq, Eq)]
struct Completion {
    exit_code: i32,
    notice: Option<String>,
}

fn completion(task_id: usize, status: Option<&TaskStatus>) -> Option<Completion> {
    match status {
        Some(TaskStatus::Done { result, .. }) => Some(Completion {
            exit_code: task_result_to_exit_code(result),
            notice: None,
        }),
        Some(_) => None,
        // `pueue clean`, or a pueued that restarted: whatever the task did,
        // nobody can tell us any more. The lease ends non-zero rather than
        // waiting for a report that will never come, and says why.
        None => Some(Completion {
            exit_code: 1,
            notice: Some(format!(
                "pueue task {task_id} disappeared before it finished; \
                 its exit code is lost"
            )),
        }),
    }
}

fn unix_ms(time: SystemTime) -> u64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_millis() as u64,
        Err(err) => {
            tracing::warn!("the clock is before the epoch: {err}");
            0
        }
    }
}

fn elapsed_ms(since: SystemTime) -> u64 {
    match since.elapsed() {
        Ok(elapsed) => elapsed.as_millis() as u64,
        Err(err) => {
            tracing::warn!("the clock went backwards under a lease: {err}");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use pueue_lib::task::TaskResult;

    fn done(result: TaskResult) -> TaskStatus {
        let now = Local::now();
        TaskStatus::Done {
            enqueued_at: now,
            start: now,
            end: now,
            result,
        }
    }

    #[test]
    fn a_running_task_has_not_completed() {
        let now = Local::now();
        let running = TaskStatus::Running {
            enqueued_at: now,
            start: now,
        };
        assert_eq!(completion(1, Some(&running)), None);
    }

    #[test]
    fn a_finished_task_carries_its_exit_code() {
        assert_eq!(
            completion(1, Some(&done(TaskResult::Failed(7)))),
            Some(Completion {
                exit_code: 7,
                notice: None,
            })
        );
    }

    /// `pueue clean` takes a task's record with it. Waiting for a status that
    /// will never arrive would hang the client forever, so the lease ends —
    /// non-zero, because the exit code went with the record.
    #[test]
    fn a_task_that_vanished_ends_the_lease_with_a_notice() {
        let completion = completion(7, None).expect("a vanished task ends its lease");
        assert_eq!(completion.exit_code, 1);
        assert!(
            completion.notice.expect("a notice").contains("7"),
            "the notice must name the task"
        );
    }
}
