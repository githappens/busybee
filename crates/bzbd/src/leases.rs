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
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use bzb_core::{
    classify::{classify, default_table, Class, Overrides},
    enqueue::{shell_escape_join, TaskSpec},
    errors::BusybeeError,
    exit_code::task_result_to_exit_code,
    protocol::{LeaseEvent, LeaseRequest, LeaseView, StatusReply},
    scheduler::{Action, Event, LeaseId, Params, Request as LeaseSpec, Scheduler},
};
use chrono::{DateTime, Local};
use pueue_lib::{message::Signal, task::Task, task::TaskStatus};
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

/// How many polls in a row may fail before pueued counts as gone. A failed
/// request drops the connection and the next poll reconnects — spawning pueued
/// if it has to — so a run of failures means it is not coming back.
const LOST_AFTER_FAILED_POLLS: u32 = 5;

/// Where a lease's events go: the connection that asked for it.
pub type Events = mpsc::UnboundedSender<LeaseEvent>;

/// What a connection asks the actor for.
pub enum Command {
    Submit {
        request: Box<LeaseRequest>,
        events: Events,
        id: oneshot::Sender<LeaseId>,
    },
    /// The connection went away; the lease goes with it, unless it is
    /// detached.
    Hangup(LeaseId),
    /// `busybee cancel <id>`: end the lease whoever asked for it. `false` says
    /// there is no such lease.
    Cancel {
        lease: LeaseId,
        known: oneshot::Sender<bool>,
    },
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

    /// Ends a lease on request. `false` means no lease of that id is live.
    pub async fn cancel(&self, lease: LeaseId) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(Command::Cancel { lease, known: tx }).await?;
        rx.await.context("the lease actor dropped a cancellation")
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

    /// The tool column of `busybee status`: the basename `classify` finds under
    /// the wrappers. Reporting only — admission still gives every lease
    /// `Class::None` until the injection work lands.
    fn tool(&self) -> String {
        classify(&self.request.argv, &Overrides::default(), &default_table()).tool
    }
}

pub struct Leases {
    scheduler: Scheduler,
    params: Params,
    leases: BTreeMap<LeaseId, Lease>,
    next_id: u64,
    pueue: Pueue,
    /// pueue tasks being torn down, until pueued confirms they are gone.
    killing: BTreeMap<usize, Kill>,
    /// Submissions pueued may or may not have started; see [`Unreconciled`].
    unreconciled: Vec<Unreconciled>,
    /// Consecutive failed polls. Enough of them and pueued counts as gone.
    failed_polls: u32,
    /// Admissions held back while a task bzbd cannot account for may still be
    /// on the machine — one being killed, or one submitted without an answer.
    /// The admission machine has already forgotten its lease, so starting the
    /// next one now would run two exclusive tasks at once.
    deferred: VecDeque<Action>,
    leases_path: PathBuf,
}

/// A teardown in flight.
struct Kill {
    /// When SIGKILL follows, for a task that ignores SIGTERM.
    deadline: Instant,
    /// Whether it already has: the escalation happens once delivered, and then
    /// the wait is for pueued to report the task gone.
    escalated: bool,
}

/// A submission whose answer never arrived. The task id comes back in that
/// answer, so without it bzbd cannot tell a submission that never landed from
/// one pueued has already started — and it starts them on arrival. The next
/// poll settles it, and admissions wait until it has.
struct Unreconciled {
    /// The label the task would carry.
    label: String,
    /// When the submission went out: a task created before it is somebody
    /// else's.
    since: DateTime<Local>,
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
            unreconciled: Vec::new(),
            failed_polls: 0,
            deferred: VecDeque::new(),
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
            Command::Cancel { lease, known } => {
                let live = self.leases.contains_key(&lease);
                if live {
                    self.end(lease).await;
                }
                // The receiver is gone only if the connection died while we
                // were cancelling; the lease is over either way.
                let _ = known.send(live);
            }
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
        let unhonoured_override =
            request.class_override.is_some() || request.cores_wanted.is_some();
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
        // An override the daemon cannot act on yet is said out loud: accepting
        // one and running the task exclusive anyway would tell the caller their
        // scheduling choice took effect when it did not.
        if unhonoured_override {
            self.send(
                id,
                LeaseEvent::Notice {
                    text: "--class/--cores are not in effect yet (the injection work is #8); \
                           this task runs exclusive"
                        .into(),
                },
            );
        }

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
    /// A detached lease stays: `--detach` asked for a task that outlives the
    /// client, and `busybee cancel <id>` is what ends it.
    async fn hangup(&mut self, id: LeaseId) {
        if self.leases.get(&id).is_some_and(|l| l.request.detached) {
            tracing::info!(lease = id.0, "the client detached; the lease stays");
            return;
        }
        self.end(id).await;
    }

    /// Ends a lease, killing its task if it has one.
    async fn end(&mut self, id: LeaseId) {
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
                // The machine sizes an admission as if the lease it replaces
                // were already gone. While a task bzbd cannot account for may
                // still be running that is not yet true, so the admission waits
                // for the poll that settles it; teardowns and queue positions
                // carry on meanwhile.
                Action::Admit { .. } if self.holding() => {
                    tracing::debug!("holding an admission back until the machine is accounted for");
                    self.deferred.push_back(action);
                }
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

    /// Whether an admission has to wait, because a task the admission machine
    /// has stopped counting may still be on the machine.
    fn holding(&self) -> bool {
        !self.killing.is_empty() || !self.unreconciled.is_empty()
    }

    /// Submits an admitted lease to pueued and tells its client it is running.
    async fn admit(&mut self, id: LeaseId, cores: Option<u32>) -> Vec<Action> {
        let Some(lease) = self.leases.get(&id) else {
            // The actor is the only thing that removes leases, and it does not
            // do so between an admission and this call.
            tracing::error!(lease = id.0, "admitted a lease that no longer exists");
            return self.scheduler.handle(Event::DrainFailed(id));
        };
        let label = lease.label();
        let spec = TaskSpec {
            command: shell_escape_join(&lease.request.argv),
            cwd: lease.request.cwd.clone(),
            env: lease.request.env.clone(),
            label: Some(label.clone()),
            // The group is at `parallel_tasks = 0`, so pueue's dispatcher will
            // never start it: admission is bzbd's decision and it has been made.
            start_immediately: true,
        };
        let class = lease.class;

        let sent_at = Local::now();
        let task_id = match self.pueue.add(spec).await {
            Ok(task_id) => task_id,
            Err(err) => {
                tracing::error!(lease = id.0, "cannot submit to pueued: {err:#}");
                // pueued may have started it anyway — the id is in the answer
                // that did not arrive — so the next poll goes looking before
                // anything else is admitted.
                self.unreconciled.push(Unreconciled {
                    label,
                    since: sent_at,
                });
                let actions = self.scheduler.handle(Event::DrainFailed(id));
                // The machine's teardown has no task id to act on; the lease
                // ends here instead, and never silently: the client hears why
                // its command is not going to run.
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
        // Its admission may not have happened yet; it must not happen now.
        self.deferred
            .retain(|a| !matches!(a, Action::Admit { id: held, .. } if *held == id));
        if let Some(task_id) = lease.pueue_task_id {
            self.kill_task(task_id).await;
        }
        let _ = lease.conn.send(LeaseEvent::Finished {
            id: id.0,
            exit_code: KILLED,
        });
    }

    /// Signals a task and waits for pueued to confirm it is gone; SIGKILL
    /// follows on the poll after the grace period, for a task that ignores the
    /// first signal.
    async fn kill_task(&mut self, task_id: usize) {
        if let Err(err) = self.pueue.kill(task_id, Signal::SigTerm).await {
            tracing::error!(task = task_id, "cannot stop the task: {err:#}");
        }
        self.killing.insert(
            task_id,
            Kill {
                deadline: Instant::now() + KILL_GRACE,
                escalated: false,
            },
        );
    }

    /// One tick: ask pueued about every task we are waiting on, and act on the
    /// ones that ended.
    async fn poll(&mut self) {
        let watching = self.leases.values().any(|l| l.pueue_task_id.is_some());
        if !watching && !self.holding() && self.deferred.is_empty() {
            return;
        }
        let state = match self.pueue.status().await {
            Ok(state) => {
                self.failed_polls = 0;
                state
            }
            Err(err) => {
                // Loud, and retried on the next tick: a poll that cannot run
                // leaves leases running, not finished.
                self.failed_polls += 1;
                tracing::error!(failures = self.failed_polls, "cannot poll pueued: {err:#}");
                if self.failed_polls >= LOST_AFTER_FAILED_POLLS {
                    self.failed_polls = 0;
                    self.lose_running_leases(&err).await;
                }
                return;
            }
        };
        let status = |task_id: usize| state.tasks.get(&task_id).map(|t| &t.status);

        for (task_id, mut kill) in std::mem::take(&mut self.killing) {
            match status(task_id) {
                // Gone or done: the teardown is over either way, and whatever
                // was waiting on it may go ahead.
                None | Some(TaskStatus::Done { .. }) => continue,
                // Signalled but still there: SIGKILL is not sent twice, so all
                // that is left is to wait for pueued to report it gone.
                _ if kill.escalated => {}
                _ if Instant::now() >= kill.deadline => {
                    tracing::warn!(task = task_id, "still running after SIGTERM; killing");
                    match self.pueue.kill(task_id, Signal::SigKill).await {
                        // Only a delivered SIGKILL counts as the escalation:
                        // one recorded but never sent would leave the task
                        // running and everything queued behind it stuck.
                        Ok(()) => kill.escalated = true,
                        Err(err) => {
                            tracing::error!(task = task_id, "cannot kill the task: {err:#}");
                        }
                    }
                }
                _ => {}
            }
            self.killing.insert(task_id, kill);
        }

        // A submission whose answer was lost: if pueued did start the task, it
        // is running with nothing to account for it, so it is killed like any
        // other orphan. Either way the admission it held up may go ahead.
        if !self.unreconciled.is_empty() {
            let tracked: BTreeSet<usize> = self
                .leases
                .values()
                .filter_map(|l| l.pueue_task_id)
                .collect();
            for pending in std::mem::take(&mut self.unreconciled) {
                match orphan(&state.tasks, &pending, &tracked) {
                    Some(task_id) => {
                        tracing::error!(
                            task = task_id,
                            "pueued started a task whose submission failed; stopping it"
                        );
                        self.kill_task(task_id).await;
                    }
                    None => tracing::info!(
                        label = pending.label,
                        "the failed submission never reached pueued"
                    ),
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

        // Every task bzbd could not account for is now accounted for, so the
        // admissions that waited on them may go ahead. Only now, at the end of
        // the tick: the task they submit is not in the `state` read above, and
        // the scan for ended leases would take it for one that vanished.
        if !self.holding() && !self.deferred.is_empty() {
            let waiting = std::mem::take(&mut self.deferred);
            self.drive(waiting.into()).await;
        }
    }

    /// pueued has stopped answering for long enough to count as gone
    /// (`docs/design/bzbd.md` §Failure and recovery). Nothing it was running
    /// can be accounted for any more, so the leases that depend on it are
    /// marked lost and their clients told why; bzbd keeps serving, and the next
    /// submission spawns pueued again.
    async fn lose_running_leases(&mut self, err: &BusybeeError) {
        // Confirmations that will never arrive. Waiting for them would hold
        // every remaining admission behind a task nobody can ask about.
        if self.holding() {
            tracing::error!(
                teardowns = self.killing.len(),
                submissions = self.unreconciled.len(),
                "giving up on what pueued was asked to do: it is gone"
            );
            self.killing.clear();
            self.unreconciled.clear();
        }
        let running: Vec<LeaseId> = self
            .leases
            .iter()
            .filter(|(_, lease)| lease.pueue_task_id.is_some())
            .map(|(id, _)| *id)
            .collect();
        for id in running {
            self.send(
                id,
                LeaseEvent::Notice {
                    text: format!(
                        "bzbd lost contact with pueued: {err}; this task is no longer tracked"
                    ),
                },
            );
            self.finish(id, 1);
            let actions = self.scheduler.handle(Event::Finished(id));
            self.drive(actions).await;
        }
        let waiting = std::mem::take(&mut self.deferred);
        self.drive(waiting.into()).await;
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
        let leases = snapshot
            .admitted
            .iter()
            .map(|l| (l.id, l.class, l.cores_held))
            .chain(snapshot.queued.iter().map(|r| (r.id, r.class, 0)))
            .enumerate()
            .map(|(position, (id, class, cores))| {
                let lease = self.leases.get(&id);
                // An admitted lease with no task of its own has not started:
                // it is held back while an earlier task is torn down, and
                // reporting it as running would show a command nobody is
                // executing. What is ahead of it is what precedes it here.
                let pueue_task_id = lease.and_then(|l| l.pueue_task_id);
                let running = pueue_task_id.is_some();
                LeaseView {
                    id: id.0,
                    label: lease.map(Lease::label).unwrap_or_default(),
                    tool: lease.map(Lease::tool).unwrap_or_default(),
                    class: class.as_str().to_string(),
                    cores,
                    state: if running { "running" } else { "queued" }.to_string(),
                    elapsed_ms: lease.map_or(0, |l| elapsed_ms(l.started_at)),
                    ahead: (!running).then_some(position),
                    pueue_task_id,
                }
            })
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

/// The task an unanswered submission may have started: it carries the label
/// bzbd asked for, pueued created it no earlier than the submission, it is
/// still alive, and no lease claims it. Anything else is somebody's task and
/// is left alone.
fn orphan(
    tasks: &BTreeMap<usize, Task>,
    pending: &Unreconciled,
    tracked: &BTreeSet<usize>,
) -> Option<usize> {
    tasks
        .values()
        .find(|task| {
            task.label.as_deref() == Some(pending.label.as_str())
                && task.created_at >= pending.since
                && !tracked.contains(&task.id)
                && !matches!(task.status, TaskStatus::Done { .. })
        })
        .map(|task| task.id)
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
    fn task(id: usize, label: &str, created_at: DateTime<Local>, status: TaskStatus) -> Task {
        let mut task = Task::new(
            "sleep 1".into(),
            PathBuf::from("/tmp"),
            Default::default(),
            "busybee".into(),
            status,
            Vec::new(),
            0,
            Some(label.into()),
        );
        task.id = id;
        task.created_at = created_at;
        task
    }

    fn running() -> TaskStatus {
        let now = Local::now();
        TaskStatus::Running {
            enqueued_at: now,
            start: now,
        }
    }

    fn tasks(tasks: Vec<Task>) -> BTreeMap<usize, Task> {
        tasks.into_iter().map(|t| (t.id, t)).collect()
    }

    /// A submission whose answer was lost may still have started a task, and
    /// `start_immediately` means it is already running. Nothing accounts for
    /// it, so the poll has to find it.
    #[test]
    fn an_unanswered_submission_finds_the_task_pueued_started() {
        let since = Local::now();
        let state = tasks(vec![task(4, "cargo build", since, running())]);
        let pending = Unreconciled {
            label: "cargo build".into(),
            since,
        };
        assert_eq!(orphan(&state, &pending, &BTreeSet::new()), Some(4));
    }

    /// The task a live lease is watching is accounted for, even when its label
    /// is the same — two sessions building the same project is the ordinary
    /// case, and killing the other one would be the bug this guards against.
    #[test]
    fn a_task_a_lease_holds_is_not_an_orphan() {
        let since = Local::now();
        let state = tasks(vec![task(4, "cargo build", since, running())]);
        let pending = Unreconciled {
            label: "cargo build".into(),
            since,
        };
        assert_eq!(orphan(&state, &pending, &BTreeSet::from([4])), None);
    }

    /// Nor is one that predates the submission: it cannot be the task the
    /// submission would have created.
    #[test]
    fn a_task_older_than_the_submission_is_not_an_orphan() {
        let since = Local::now();
        let state = tasks(vec![task(
            4,
            "cargo build",
            since - chrono::Duration::seconds(1),
            running(),
        )]);
        let pending = Unreconciled {
            label: "cargo build".into(),
            since,
        };
        assert_eq!(orphan(&state, &pending, &BTreeSet::new()), None);
    }

    /// An admission held back while an earlier task is torn down has no task
    /// of its own, and its client has not been told it is running. `busybee
    /// status` must not say otherwise.
    #[tokio::test]
    async fn an_admission_held_back_is_reported_as_queued() {
        let directory = tempfile::tempdir().expect("create a tempdir");
        let params = Params {
            pool_size: 8,
            max_concurrent: 4,
        };
        let (mut actor, _handle, _commands) =
            Leases::new(params, directory.path().join("leases.json"));
        // A teardown in flight: what holds the admission back.
        actor.killing.insert(
            9,
            Kill {
                deadline: Instant::now() + KILL_GRACE,
                escalated: false,
            },
        );

        let (events, _stream) = mpsc::unbounded_channel();
        let (id, _asked) = oneshot::channel();
        actor
            .submit(
                LeaseRequest {
                    argv: vec!["cargo".into(), "build".into()],
                    cwd: PathBuf::from("/tmp"),
                    env: Default::default(),
                    label: None,
                    class_override: None,
                    cores_wanted: None,
                    detached: false,
                },
                events,
                id,
            )
            .await;

        let status = actor.status();
        assert_eq!(status.leases.len(), 1, "leases were {:?}", status.leases);
        assert_eq!(status.leases[0].state, "queued");
        assert_eq!(status.leases[0].pueue_task_id, None);
        assert_eq!(status.leases[0].ahead, Some(0));
    }

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
