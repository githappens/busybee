//! The lease actor: one task owning the scheduler, the token pool, the live
//! leases and the connection to pueued (see `docs/design/bzbd.md` §Lease model
//! and §Failure and recovery).

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use bzb_core::{
    classify::{classify, default_table, Class, Overrides, Plan, Table},
    config::{Config, StaticDefault},
    enqueue::{shell_escape_join, TaskSpec},
    errors::BusybeeError,
    exit_code::task_result_to_exit_code,
    group::BUSYBEE_GROUP,
    jobserver::Jobserver,
    nest::LEASE_ENV,
    protocol::{LeaseEvent, LeaseRequest, LeaseView, StatusReply},
    scheduler::{Action, Event, LeaseId, Params, Request as LeaseSpec, Scheduler},
};
use pueue_lib::{message::Signal, task::Task, task::TaskStatus};
use tokio::sync::{mpsc, oneshot};

use crate::{
    inject::inject,
    recovery::{Record, Recovered},
    submit::Pueue,
};

/// pueued poll cadence, fixed by the spec.
const POLL: Duration = Duration::from_secs(1);

/// Fifo-vs-books check: a backstop for tokens a tool lost or invented.
const ACCOUNTING: Duration = Duration::from_secs(10);

/// Exit code of a lease whose own client hung up.
const KILLED: i32 = 130;

pub(crate) type Events = mpsc::UnboundedSender<LeaseEvent>;

pub(crate) enum Command {
    Submit {
        request: Box<LeaseRequest>,
        events: Events,
        id: oneshot::Sender<LeaseId>,
    },
    Hangup(LeaseId),
    /// `false` says there is no such lease.
    Cancel {
        lease: LeaseId,
        known: oneshot::Sender<bool>,
    },
    Status(oneshot::Sender<Result<StatusReply>>),
    /// Acknowledged once in force, so a reload is only reported after it is.
    SetConfig {
        config: Box<Config>,
        done: oneshot::Sender<()>,
    },
    Shutdown(oneshot::Sender<()>),
}

#[derive(Clone)]
pub(crate) struct Handle(mpsc::Sender<Command>);

impl Handle {
    pub(crate) async fn submit(&self, request: LeaseRequest, events: Events) -> Result<LeaseId> {
        let request = Box::new(request);
        self.ask(
            |id| Command::Submit {
                request,
                events,
                id,
            },
            "a submission",
        )
        .await
    }

    pub(crate) async fn hangup(&self, lease: LeaseId) -> Result<()> {
        self.send(Command::Hangup(lease)).await
    }

    pub(crate) async fn cancel(&self, lease: LeaseId) -> Result<bool> {
        self.ask(|known| Command::Cancel { lease, known }, "a cancellation")
            .await
    }

    pub(crate) async fn status(&self) -> Result<StatusReply> {
        self.ask(Command::Status, "a status request").await?
    }

    pub(crate) async fn set_config(&self, config: Config) -> Result<()> {
        let config = Box::new(config);
        self.ask(
            |done| Command::SetConfig { config, done },
            "a set-config request",
        )
        .await
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.ask(Command::Shutdown, "the shutdown").await
    }

    async fn ask<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> Command,
        what: &str,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.send(command(tx)).await?;
        rx.await
            .with_context(|| format!("the lease actor dropped {what}"))
    }

    async fn send(&self, command: Command) -> Result<()> {
        self.0
            .send(command)
            .await
            .map_err(|_| anyhow::anyhow!("the lease actor is gone"))
    }
}

struct Lease {
    request: LeaseRequest,
    /// `None` for a lease adopted from a previous daemon.
    conn: Option<Events>,
    plan: Plan,
    pueue_task_id: Option<usize>,
    cores_held: u32,
    started_at: SystemTime,
    submitted_at: Option<SystemTime>,
    fifo: Option<PathBuf>,
}

impl Lease {
    fn label(&self) -> String {
        self.request
            .label
            .clone()
            .unwrap_or_else(|| shell_escape_join(&self.request.argv))
    }

    fn record(&self, id: LeaseId, killing: bool) -> Record {
        Record {
            id: id.0,
            label: self.label(),
            argv: self.request.argv.clone(),
            class: self.plan.class,
            cores_held: self.cores_held,
            pueue_task_id: self.pueue_task_id,
            started_at_unix_ms: unix_ms(self.started_at),
            submitted_at_unix_ms: self.submitted_at.map(unix_ms),
            fifo: self.fifo.clone(),
            killing,
        }
    }

    fn tell_finished(&self, id: LeaseId, exit_code: i32) {
        if let Some(conn) = &self.conn {
            let _ = conn.send(LeaseEvent::Finished {
                id: id.0,
                exit_code,
            });
        }
    }
}

pub(crate) struct Leases {
    scheduler: Scheduler,
    params: Params,
    jobserver: Jobserver,
    kill_grace: Duration,
    drain_deadline: Duration,
    table: Table,
    static_default: StaticDefault,
    leases: BTreeMap<LeaseId, Lease>,
    next_id: u64,
    pueue: Pueue,
    /// Teardowns by pueue task id, until pueued confirms the task gone.
    killing: BTreeMap<usize, Kill>,
    /// Submissions whose answer (and so task id) never arrived; pueued may
    /// have started them anyway. Kept in `leases.json` until a poll settles it.
    unreconciled: Vec<Record>,
    /// Previous daemons' fifos, unlinked once no task uses them.
    old_fifos: BTreeSet<PathBuf>,
    /// Admissions held back while an unaccounted task may still be running.
    deferred: VecDeque<Action>,
    /// Tokens owed to a shrunk pool; that many releases are withheld.
    debt: u32,
    leases_path: PathBuf,
}

struct Kill {
    /// When SIGKILL follows.
    deadline: Instant,
    escalated: bool,
    /// Holds the task's tokens until pueued confirms it gone. `None` for a
    /// task no lease accounted for (an unanswered submission's).
    record: Option<Record>,
}

impl Kill {
    fn pending(grace: Duration, record: Option<Record>) -> Self {
        Self {
            deadline: Instant::now() + grace,
            escalated: false,
            record,
        }
    }

    fn cores_held(&self) -> u32 {
        self.record.as_ref().map_or(0, |r| r.cores_held)
    }
}

impl Leases {
    pub(crate) fn new(
        config: &Config,
        recovered: Recovered,
        leases_path: PathBuf,
    ) -> (Self, Handle, mpsc::Receiver<Command>) {
        let Recovered {
            jobserver,
            adopted,
            killing,
            debt,
        } = recovered;
        let (tx, rx) = mpsc::channel(64);
        let params = config.params();
        let mut actor = Self {
            scheduler: Scheduler::new(params),
            params,
            jobserver,
            kill_grace: Duration::from_millis(config.kill_grace_ms),
            drain_deadline: Duration::from_millis(config.drain_deadline_ms),
            table: table(config),
            static_default: config.defaults.r#static,
            leases: BTreeMap::new(),
            next_id: 1,
            pueue: Pueue::default(),
            killing: BTreeMap::new(),
            unreconciled: Vec::new(),
            old_fifos: BTreeSet::new(),
            deferred: VecDeque::new(),
            debt,
            leases_path,
        };
        for record in killing {
            let Some(task_id) = record.pueue_task_id else {
                tracing::error!(lease = record.id, "a recorded teardown names no task");
                continue;
            };
            actor.next_id = actor.next_id.max(record.id + 1);
            actor.old_fifos.extend(record.fifo.clone());
            actor
                .killing
                .insert(task_id, Kill::pending(actor.kill_grace, Some(record)));
        }
        for record in adopted {
            let id = LeaseId(record.id);
            let argv = record.argv.clone();
            actor.next_id = actor.next_id.max(record.id + 1);
            actor.scheduler.adopt(
                LeaseSpec {
                    id,
                    class: record.class,
                    cores_wanted: None,
                },
                record.cores_held,
            );
            actor.old_fifos.extend(record.fifo.clone());
            actor.leases.insert(
                id,
                Lease {
                    // `detached`: no connection holds an orphan.
                    request: LeaseRequest {
                        argv: record.argv,
                        cwd: PathBuf::new(),
                        env: BTreeMap::new(),
                        label: Some(record.label),
                        class_override: None,
                        cores_wanted: None,
                        detached: true,
                    },
                    conn: None,
                    // The recorded class wins: a config edited since must not
                    // re-label a task in flight.
                    plan: Plan {
                        class: record.class,
                        ..classify(&argv, &Overrides::default(), &actor.table)
                    },
                    pueue_task_id: record.pueue_task_id,
                    cores_held: record.cores_held,
                    started_at: UNIX_EPOCH + Duration::from_millis(record.started_at_unix_ms),
                    submitted_at: record
                        .submitted_at_unix_ms
                        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms)),
                    fifo: record.fifo,
                },
            );
        }
        actor.persist();
        (actor, Handle(tx), rx)
    }

    pub(crate) async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        self.resume_teardowns().await;
        let mut ticker = tokio::time::interval(POLL);
        let mut accounting = tokio::time::interval(ACCOUNTING);
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.command(command).await,
                    None => break,
                },
                _ = ticker.tick() => {
                    self.poll().await;
                    self.collect_debt();
                }
                _ = accounting.tick() => self.account(),
            }
        }
    }

    /// The previous daemon booked each teardown before signalling, so the
    /// SIGTERM may never have gone out.
    async fn resume_teardowns(&mut self) {
        let resumed: Vec<usize> = self.killing.keys().copied().collect();
        for task_id in resumed {
            if let Err(err) = self.pueue.kill(task_id, Signal::SigTerm).await {
                tracing::error!(
                    task = task_id,
                    "cannot signal a recovered teardown: {err:#}"
                );
            }
        }
    }

    async fn command(&mut self, command: Command) {
        // A dropped reply receiver means the asker went away; nothing to do.
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
                let _ = known.send(live);
            }
            Command::SetConfig { config, done } => {
                // Driven: a raised pool can make the queue head admissible now.
                let actions = self.reconfigure(&config);
                self.drive(actions).await;
                let _ = done.send(());
            }
            Command::Status(reply) => {
                let _ = reply.send(self.status());
            }
            Command::Shutdown(done) => {
                self.persist();
                // The tasks keep the fifo; the next daemon unlinks it.
                self.jobserver.leave();
                let _ = done.send(());
            }
        }
    }

    /// Tokens out on a task's behalf. Leases, teardowns and unanswered
    /// submissions are disjoint: a lease leaves the map before its record
    /// joins either list.
    fn held(&self) -> u32 {
        self.leases.values().map(|l| l.cores_held).sum::<u32>()
            + self.killing.values().map(Kill::cores_held).sum::<u32>()
            + self.unreconciled.iter().map(|r| r.cores_held).sum::<u32>()
    }

    /// Whether tokens may legitimately be out without the books saying so.
    fn tokens_in_flight(&self) -> bool {
        self.holding()
            || self
                .leases
                .values()
                .any(|l| l.plan.class == Class::Jobserver && l.pueue_task_id.is_some())
    }

    fn free_and_held(&self) -> Option<(u32, u32)> {
        match self.jobserver.free() {
            Ok(free) => Some((free, self.held())),
            Err(err) => {
                tracing::error!("cannot count the pool's free tokens: {err}");
                None
            }
        }
    }

    /// Drains tokens above the pool; a shortfall is only reported, since
    /// whoever holds those tokens is the one to return them.
    fn account(&mut self) {
        let Some((free, held)) = self.free_and_held() else {
            return;
        };
        let pool = self.params.pool_size;
        if free + held > pool {
            self.take_excess(free, held);
        } else if free + held < pool && !self.tokens_in_flight() {
            tracing::warn!(
                free,
                held,
                pool,
                deficit = pool - free - held,
                "the pool is short of tokens; a task did not return them"
            );
        }
    }

    /// Drains the excess and pays the debt with it, or `release` would later
    /// withhold tokens that are already gone.
    fn take_excess(&mut self, free: u32, held: u32) {
        let pool = self.params.pool_size;
        match self.jobserver.drain_excess(pool.saturating_sub(held)) {
            Ok(0) => {}
            Ok(drained) => {
                self.debt -= drained.min(self.debt);
                tracing::warn!(
                    free,
                    held,
                    pool,
                    drained,
                    debt = self.debt,
                    "the pool held excess tokens: a tool wrote more than it read, or the pool shrank"
                );
            }
            Err(err) => tracing::error!("cannot drain the excess tokens: {err}"),
        }
    }

    /// A jobserver build returns tokens straight to the fifo, bypassing
    /// `release`, so debt is collected from there on every poll and before
    /// every admission.
    fn collect_debt(&mut self) {
        if self.debt == 0 {
            return;
        }
        let Some((free, held)) = self.free_and_held() else {
            return;
        };
        if free + held > self.params.pool_size {
            self.take_excess(free, held);
        }
    }

    fn reconfigure(&mut self, config: &Config) -> Vec<Action> {
        let params = config.params();
        self.resize_pool(params.pool_size);
        self.params = params;
        self.drain_deadline = Duration::from_millis(config.drain_deadline_ms);
        self.table = table(config);
        self.static_default = config.defaults.r#static;
        self.scheduler.set_params(params)
    }

    /// Grows by releasing the delta; shrinks by draining what is free and
    /// booking the rest as debt (spec §Configuration).
    fn resize_pool(&mut self, pool_size: u32) {
        let old = self.params.pool_size;
        if pool_size > old {
            self.release(pool_size - old);
        } else if pool_size < old {
            let held = self.held();
            let wanted = old - pool_size;
            match self.jobserver.drain_excess(pool_size.saturating_sub(held)) {
                Ok(drained) if drained == wanted => {}
                Ok(drained) => {
                    // Booked as debt, not just logged: otherwise the next
                    // admission runs on a pool wider than the file says.
                    self.debt += wanted - drained;
                    tracing::warn!(
                        old,
                        new = pool_size,
                        drained,
                        held,
                        debt = self.debt,
                        "the pool shrank by more than was free; the rest is owed as the leases holding it end"
                    );
                }
                Err(err) => {
                    self.debt += wanted;
                    tracing::error!(debt = self.debt, "cannot shrink the pool: {err}");
                }
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
        // FIFO: everything already in the map is ahead of this one.
        let ahead = self.leases.len();
        let overrides = Overrides {
            class: request.class_override,
            cores: request.cores_wanted,
        };
        let mut plan = classify(&request.argv, &overrides, &self.table);
        if plan.class == Class::Static && plan.cores_wanted.is_none() {
            plan.cores_wanted = self.static_default.cores_wanted();
        }
        let lease = Lease {
            request,
            conn: Some(events),
            plan,
            pueue_task_id: None,
            cores_held: 0,
            started_at: SystemTime::now(),
            submitted_at: None,
            fifo: Some(self.jobserver.path().to_path_buf()),
        };
        let spec = LeaseSpec {
            id,
            class: lease.plan.class,
            cores_wanted: lease.plan.cores_wanted,
        };
        self.leases.insert(id, lease);
        self.persist();
        if reply.send(id).is_err() {
            // The connection died before it learnt the id, so it cannot hang up.
            self.leases.remove(&id);
            self.persist();
            return;
        }
        // Before `Queued`: that is where a `--detach` client stops reading.
        for text in &self.leases[&id].plan.notices {
            self.send(id, LeaseEvent::Notice { text: text.clone() });
        }
        self.send(id, LeaseEvent::Queued { id: id.0, ahead });

        let actions = self.scheduler.handle(Event::Submit(spec));
        // Would repeat the `Queued` just sent.
        let actions = actions
            .into_iter()
            .filter(|a| !matches!(a, Action::Notify { id: notified, .. } if *notified == id))
            .collect();
        self.drive(actions).await;
    }

    async fn hangup(&mut self, id: LeaseId) {
        if self.leases.get(&id).is_some_and(|l| l.request.detached) {
            tracing::info!(lease = id.0, "the client detached; the lease stays");
            return;
        }
        self.end(id).await;
    }

    async fn end(&mut self, id: LeaseId) {
        let actions = self.scheduler.handle(Event::Cancel(id));
        self.drive(actions).await;
        // A queued lease gets no `Drop` action.
        if self.leases.remove(&id).is_some() {
            self.persist();
        }
    }

    async fn drive(&mut self, actions: Vec<Action>) {
        let mut pending: VecDeque<Action> = actions.into();
        while let Some(action) = pending.pop_front() {
            match action {
                // The scheduler sizes an admission as if the lease it replaces
                // were gone; wait until the poll confirms it is.
                Action::Admit { .. } if self.holding() => {
                    tracing::debug!("holding an admission back until the machine is accounted for");
                    self.deferred.push_back(action);
                }
                Action::Admit {
                    id,
                    drain_target,
                    cores,
                    ..
                } => pending.extend(self.admit(id, drain_target, cores).await),
                Action::Notify { id, ahead } => {
                    // Clients count running tasks as ahead too.
                    let ahead = ahead + self.scheduler.snapshot().admitted.len();
                    self.send(id, LeaseEvent::Queued { id: id.0, ahead });
                }
                Action::Drop(id) => self.drop_lease(id).await,
            }
        }
    }

    /// A task the scheduler no longer counts may still be on the machine.
    fn holding(&self) -> bool {
        !self.killing.is_empty() || !self.unreconciled.is_empty()
    }

    async fn admit(&mut self, id: LeaseId, drain_target: u32, cores: Option<u32>) -> Vec<Action> {
        if !self.leases.contains_key(&id) {
            tracing::error!(lease = id.0, "admitted a lease that no longer exists");
            return self.scheduler.handle(Event::DrainFailed(id));
        }

        // The build whose end drove this admission has just returned its
        // tokens, old pool and all; collect the debt before anything reads them.
        self.collect_debt();

        let diag_free = self.jobserver.free().ok();
        let diag_t0 = std::time::Instant::now();
        let drained = tokio::task::block_in_place(|| {
            self.jobserver.acquire(drain_target, self.drain_deadline)
        });
        tracing::info!(lease = id.0, drain_target, ?drained, ?diag_free, elapsed_ms = diag_t0.elapsed().as_millis() as u64, "DIAG drain");
        let got = match drained {
            Ok(got) => got,
            Err(err) => {
                tracing::error!(lease = id.0, "cannot drain the pool: {err}");
                return self.refuse(id, format!("bzbd could not drain the token pool: {err}"));
            }
        };
        if drain_target > 0 && got == 0 {
            self.send(
                id,
                LeaseEvent::Notice {
                    text: format!(
                        "no token was free within {} ms; starting on the implicit token only",
                        self.drain_deadline.as_millis()
                    ),
                },
            );
        }
        let lease = self.leases.get(&id).expect("checked above");
        // Jobserver: the fair share. Otherwise: the tokens held, at least one.
        let share = cores.unwrap_or(got.max(1));
        let mut injected = inject(
            &lease.plan,
            lease.request.argv.len(),
            lease.request.env.clone(),
            &self.jobserver.path().display().to_string(),
            share,
        );
        // Always overwrite: a stale inherited value must not survive.
        injected.env.insert(LEASE_ENV.to_string(), id.0.to_string());
        let spec = TaskSpec {
            command: shell_escape_join(&injected.argv),
            cwd: lease.request.cwd.clone(),
            env: injected.env,
            label: Some(lease.label()),
            // The group is at `parallel_tasks = 0`; bzbd decides what starts.
            start_immediately: true,
        };
        let class = lease.plan.class;

        // On record before the submission: pueued starts the task on arrival,
        // and a successor must find the grant and the submission time.
        let lease = self.leases.get_mut(&id).expect("checked above");
        lease.submitted_at = Some(SystemTime::now());
        lease.cores_held = got;
        if let Err(err) = self.try_persist() {
            tracing::error!(lease = id.0, "cannot record the grant: {err:#}");
            return self.refuse(
                id,
                format!("bzbd could not record the lease before starting it: {err:#}"),
            );
        }
        let task_id = match self.pueue.add(spec).await {
            Ok(task_id) => task_id,
            Err(err) => {
                tracing::error!(lease = id.0, "cannot submit to pueued: {err:#}");
                let actions =
                    self.drain_failed(id, format!("bzbd could not start the task: {err}"));
                self.hold_unanswered(id);
                return actions;
            }
        };

        self.leases
            .get_mut(&id)
            .expect("checked above")
            .pueue_task_id = Some(task_id);
        self.persist();

        self.send(
            id,
            LeaseEvent::Admitted {
                id: id.0,
                pueue_task_id: task_id,
                class: class.as_str().to_string(),
                cores: share,
                pool_size: self.params.pool_size,
                peers: self.scheduler.snapshot().admitted.len().saturating_sub(1),
            },
        );
        self.scheduler.handle(Event::Started {
            id,
            cores_held: got,
        })
    }

    /// Tells the scheduler and the client; the caller ends the lease, since
    /// the scheduler's `Drop` has no task to act on.
    fn drain_failed(&mut self, id: LeaseId, notice: String) -> Vec<Action> {
        let actions = self
            .scheduler
            .handle(Event::DrainFailed(id))
            .into_iter()
            .filter(|a| !matches!(a, Action::Drop(dropped) if *dropped == id))
            .collect();
        self.send(id, LeaseEvent::Notice { text: notice });
        actions
    }

    /// Ends an admitted lease that never reached pueued.
    fn refuse(&mut self, id: LeaseId, reason: String) -> Vec<Action> {
        let actions = self.drain_failed(id, reason);
        self.finish(id, 1);
        actions
    }

    /// Ends a lease whose submission went unanswered. It stays in
    /// `leases.json`, tokens and all, as a teardown with no task named until
    /// the next poll (or a restart) goes looking for the task.
    fn hold_unanswered(&mut self, id: LeaseId) {
        let Some(lease) = self.leases.remove(&id) else {
            return;
        };
        self.unreconciled.push(lease.record(id, true));
        self.persist();
        lease.tell_finished(id, 1);
    }

    async fn drop_lease(&mut self, id: LeaseId) {
        let Some(lease) = self.leases.remove(&id) else {
            tracing::warn!(lease = id.0, "asked to drop a lease that is not tracked");
            return;
        };
        self.deferred
            .retain(|a| !matches!(a, Action::Admit { id: held, .. } if *held == id));
        match lease.pueue_task_id {
            Some(task_id) => self.kill_task(task_id, Some(lease.record(id, true))).await,
            None => self.persist(),
        }
        lease.tell_finished(id, KILLED);
    }

    /// SIGTERM now, SIGKILL on the poll after the grace; tokens go back once
    /// pueued confirms the task gone.
    async fn kill_task(&mut self, task_id: usize, record: Option<Record>) {
        self.book_teardown(task_id, record);
        if let Err(err) = self.pueue.kill(task_id, Signal::SigTerm).await {
            tracing::error!(task = task_id, "cannot stop the task: {err:#}");
        }
    }

    /// On disk before the signal, so a daemon killed in between resumes the
    /// teardown instead of adopting the task as a lease.
    fn book_teardown(&mut self, task_id: usize, record: Option<Record>) {
        self.killing
            .insert(task_id, Kill::pending(self.kill_grace, record));
        self.persist();
    }

    /// Returns tokens to the pool, after paying its debt.
    fn release(&mut self, tokens: u32) {
        let owed = tokens.min(self.debt);
        if owed > 0 {
            self.debt -= owed;
            tracing::warn!(
                withheld = owed,
                remaining = self.debt,
                "withholding released tokens the pool has no room for"
            );
        }
        let tokens = tokens - owed;
        if tokens == 0 {
            return;
        }
        if let Err(err) = self.jobserver.release(tokens) {
            tracing::error!(tokens, "cannot return the tokens to the pool: {err}");
        }
    }

    async fn poll(&mut self) {
        let watching = self.leases.values().any(|l| l.pueue_task_id.is_some());
        if !watching && !self.holding() && self.deferred.is_empty() {
            return;
        }
        let state = match self.pueue.status().await {
            Ok(state) => state,
            Err(err) => {
                tracing::error!("cannot poll pueued: {err:#}");
                self.lose_running_leases(&err).await;
                return;
            }
        };
        let status = |task_id: usize| state.tasks.get(&task_id).map(|t| &t.status);

        let mut settled = false;
        for (task_id, mut kill) in std::mem::take(&mut self.killing) {
            match status(task_id) {
                None | Some(TaskStatus::Done { .. }) => {
                    self.release(kill.cores_held());
                    settled = true;
                    continue;
                }
                _ if kill.escalated => {}
                _ if Instant::now() >= kill.deadline => {
                    tracing::warn!(task = task_id, "still running after SIGTERM; killing");
                    match self.pueue.kill(task_id, Signal::SigKill).await {
                        // Only a delivered SIGKILL counts, or the retry is lost.
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
        if settled {
            self.persist();
        }

        // An unanswered submission whose task pueued did start is killed like
        // any orphan; one that never landed just gives its tokens back.
        if !self.unreconciled.is_empty() {
            let tracked: BTreeSet<usize> = self
                .leases
                .values()
                .filter_map(|l| l.pueue_task_id)
                .chain(self.killing.keys().copied())
                .collect();
            let mut records = std::mem::take(&mut self.unreconciled);
            claim_orphans(&mut records, &state.tasks, tracked);
            for record in records {
                match record.pueue_task_id {
                    Some(task_id) => {
                        tracing::error!(
                            task = task_id,
                            "pueued started a task whose submission failed; stopping it"
                        );
                        self.kill_task(task_id, Some(record)).await;
                    }
                    None => {
                        tracing::info!(
                            label = record.label,
                            "the failed submission never reached pueued"
                        );
                        self.release(record.cores_held);
                        self.persist();
                    }
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
            self.end_task(id, completion.exit_code, completion.notice)
                .await;
        }

        // Only at the end of the tick: a task submitted now is missing from
        // `state`, and the scan above would take it for one that vanished.
        if !self.holding() && !self.deferred.is_empty() {
            let waiting = std::mem::take(&mut self.deferred);
            self.drive(waiting.into()).await;
        }
        self.sweep_old_fifos();
    }

    /// Not while a teardown is unsettled: its task may use one of them.
    fn sweep_old_fifos(&mut self) {
        if self.old_fifos.is_empty() || self.holding() {
            return;
        }
        let in_use: BTreeSet<&PathBuf> = self
            .leases
            .values()
            .filter_map(|l| l.fifo.as_ref())
            .collect();
        let (keep, done): (BTreeSet<PathBuf>, BTreeSet<PathBuf>) =
            std::mem::take(&mut self.old_fifos)
                .into_iter()
                .partition(|path| in_use.contains(path));
        self.old_fifos = keep;
        for path in done {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    tracing::info!(fifo = %path.display(), "removed the previous daemon's fifo")
                }
                Err(err) => {
                    tracing::error!(fifo = %path.display(), "cannot remove the previous daemon's fifo: {err}")
                }
            }
        }
    }

    /// Spec §Failure and recovery, "pueued dies": nothing it ran can be
    /// accounted for, so its leases and teardowns end and their tokens go back.
    async fn lose_running_leases(&mut self, err: &BusybeeError) {
        if self.holding() {
            tracing::error!(
                teardowns = self.killing.len(),
                submissions = self.unreconciled.len(),
                "giving up on what pueued was asked to do: it is gone"
            );
            for (_, kill) in std::mem::take(&mut self.killing) {
                self.release(kill.cores_held());
            }
            for record in std::mem::take(&mut self.unreconciled) {
                self.release(record.cores_held);
            }
            self.persist();
        }
        let running: Vec<LeaseId> = self
            .leases
            .iter()
            .filter(|(_, lease)| lease.pueue_task_id.is_some())
            .map(|(id, _)| *id)
            .collect();
        for id in running {
            let notice = format!("pueued went away ({err}); task state unknown");
            self.end_task(id, 1, Some(notice)).await;
        }
        let waiting = std::mem::take(&mut self.deferred);
        self.drive(waiting.into()).await;
    }

    async fn end_task(&mut self, id: LeaseId, exit_code: i32, notice: Option<String>) {
        if let Some(text) = notice {
            self.send(id, LeaseEvent::Notice { text });
        }
        self.finish(id, exit_code);
        let actions = self.scheduler.handle(Event::Finished(id));
        self.drive(actions).await;
    }

    /// Off the books before the client hears, so its next status agrees.
    fn finish(&mut self, id: LeaseId, exit_code: i32) {
        let Some(lease) = self.leases.remove(&id) else {
            return;
        };
        self.release(lease.cores_held);
        self.persist();
        lease.tell_finished(id, exit_code);
    }

    fn send(&self, id: LeaseId, event: LeaseEvent) {
        let Some(conn) = self.leases.get(&id).and_then(|l| l.conn.as_ref()) else {
            return;
        };
        if conn.send(event).is_err() {
            tracing::debug!(lease = id.0, "the client is no longer reading its events");
        }
    }

    fn status(&self) -> Result<StatusReply> {
        // The real count: jobserver tasks move tokens the scheduler never sees.
        let free = self
            .jobserver
            .free()
            .context("cannot count the pool's free tokens")?;
        let snapshot = self.scheduler.snapshot();
        let leases = snapshot
            .admitted
            .iter()
            .map(|l| (l.id, l.class, l.cores_held))
            .chain(snapshot.queued.iter().map(|r| (r.id, r.class, 0)))
            .enumerate()
            .map(|(position, (id, class, cores))| {
                let lease = self.leases.get(&id);
                // Admitted but held back (no task yet) reports as queued.
                let pueue_task_id = lease.and_then(|l| l.pueue_task_id);
                let running = pueue_task_id.is_some();
                let state = match lease {
                    Some(lease) if running && lease.conn.is_none() => "orphaned",
                    _ if running => "running",
                    _ => "queued",
                };
                LeaseView {
                    id: id.0,
                    label: lease.map(Lease::label).unwrap_or_default(),
                    tool: lease.map(|l| l.plan.tool.clone()).unwrap_or_default(),
                    class: class.as_str().to_string(),
                    // Jobserver leases hold no fixed tokens; per-process
                    // attribution is not yet implemented, so usage is unknown.
                    cores: (class != Class::Jobserver).then_some(cores),
                    state: state.to_string(),
                    elapsed_ms: lease.map_or(0, |l| elapsed_ms(l.started_at)),
                    ahead: (!running).then_some(position),
                    pueue_task_id,
                }
            })
            .collect();
        Ok(StatusReply {
            pool_size: self.params.pool_size,
            free,
            held: self.held(),
            leases,
        })
    }

    fn persist(&self) {
        if let Err(err) = self.try_persist() {
            tracing::error!("cannot record the leases: {err:#}");
        }
    }

    fn try_persist(&self) -> Result<()> {
        let records: Vec<Record> = self
            .leases
            .iter()
            .map(|(id, lease)| lease.record(*id, false))
            .chain(self.killing.values().filter_map(|k| k.record.clone()))
            .chain(self.unreconciled.iter().cloned())
            .collect();
        write_json(&self.leases_path, &records)
    }
}

/// Through a temporary file: a half-written `leases.json` is worse than an old one.
fn write_json(path: &Path, records: &[Record]) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    let encoded = serde_json::to_vec(records).context("cannot encode the leases")?;
    std::fs::write(&temporary, encoded)
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("cannot replace {}", path.display()))
}

/// Names the task each unanswered submission may have started. Each match is
/// claimed before the next record looks, so same-label submissions do not
/// share a task.
pub(crate) fn claim_orphans(
    records: &mut [Record],
    tasks: &BTreeMap<usize, Task>,
    mut claimed: BTreeSet<usize>,
) {
    for record in records.iter_mut().filter(|r| r.pueue_task_id.is_none()) {
        let Some(since) = record.submitted_at_unix_ms else {
            continue;
        };
        if let Some(task_id) = orphan(tasks, &record.label, since, &claimed) {
            claimed.insert(task_id);
            record.pueue_task_id = Some(task_id);
        }
    }
}

/// A live task in the `busybee` group with this label, created no earlier
/// than `since_ms`, that nothing else claims.
fn orphan(
    tasks: &BTreeMap<usize, Task>,
    label: &str,
    since_ms: u64,
    tracked: &BTreeSet<usize>,
) -> Option<usize> {
    tasks
        .values()
        .find(|task| {
            task.group == BUSYBEE_GROUP
                && task.label.as_deref() == Some(label)
                && task.created_at.timestamp_millis() >= since_ms as i64
                && !tracked.contains(&task.id)
                && !matches!(task.status, TaskStatus::Done { .. })
        })
        .map(|task| task.id)
}

/// `None` while the task is still going.
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
        // `pueue clean`, or a restarted pueued: the exit code is gone.
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

/// Rebuilt on every reload, so a removed override disappears.
fn table(config: &Config) -> Table {
    let mut table = default_table();
    config.apply_overrides(&mut table);
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{done, hold_umask, running, task, tasks};
    use chrono::Local;
    use pueue_lib::task::TaskResult;

    #[test]
    fn a_running_task_has_not_completed() {
        assert_eq!(completion(1, Some(&running())), None);
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

    #[test]
    fn a_task_that_vanished_ends_the_lease_with_a_notice() {
        let completion = completion(7, None).expect("a vanished task ends its lease");
        assert_eq!(completion.exit_code, 1);
        assert!(
            completion.notice.expect("a notice").contains("7"),
            "the notice must name the task"
        );
    }

    fn ms(time: chrono::DateTime<Local>) -> u64 {
        time.timestamp_millis() as u64
    }

    #[test]
    fn an_unanswered_submission_finds_the_task_pueued_started() {
        let since = Local::now();
        let state = tasks(vec![task(4, "cargo build", running())]);
        assert_eq!(
            orphan(&state, "cargo build", ms(since), &BTreeSet::new()),
            Some(4)
        );
    }

    /// Two sessions building the same project is the ordinary case.
    #[test]
    fn a_task_a_lease_holds_is_not_an_orphan() {
        let since = Local::now();
        let state = tasks(vec![task(4, "cargo build", running())]);
        assert_eq!(
            orphan(&state, "cargo build", ms(since), &BTreeSet::from([4])),
            None
        );
    }

    #[test]
    fn a_task_older_than_the_submission_is_not_an_orphan() {
        let since = Local::now();
        let mut older = task(4, "cargo build", running());
        older.created_at = since - chrono::Duration::seconds(1);
        let state = tasks(vec![older]);
        assert_eq!(
            orphan(&state, "cargo build", ms(since), &BTreeSet::new()),
            None
        );
    }

    /// Adopting it would let `busybee cancel` signal somebody else's task.
    #[test]
    fn a_task_in_another_group_is_not_an_orphan() {
        let since = Local::now();
        let mut theirs = task(4, "cargo build", running());
        theirs.group = "default".into();
        let state = tasks(vec![theirs]);
        assert_eq!(
            orphan(&state, "cargo build", ms(since), &BTreeSet::new()),
            None
        );
    }

    #[test]
    fn each_unanswered_submission_claims_a_task_of_its_own() {
        let since = Local::now();
        let state = tasks(vec![
            task(3, "cargo build", running()),
            task(4, "cargo build", running()),
            task(5, "cargo build", running()),
        ]);
        let submitted = |id: u64| Record {
            label: "cargo build".into(),
            argv: vec!["cargo".into(), "build".into()],
            cores_held: 1,
            pueue_task_id: None,
            submitted_at_unix_ms: Some(ms(since)),
            killing: true,
            ..held(id, 0, 0)
        };
        let mut records = vec![submitted(10), submitted(11), submitted(12)];

        // 3 is already being torn down.
        claim_orphans(&mut records, &state, BTreeSet::from([3]));

        let claimed: Vec<Option<usize>> = records.iter().map(|r| r.pueue_task_id).collect();
        assert_eq!(claimed, vec![Some(4), Some(5), None]);
    }

    const POOL: u32 = 4;

    /// The caller holds the umask: the actor creates a fifo and `leases.json`.
    fn actor(
        directory: &Path,
        recovered: impl FnOnce(&Jobserver) -> (Vec<Record>, Vec<Record>, u32),
    ) -> Leases {
        let mut config = Config::defaults().expect("the defaults are a valid config");
        config.pool_size = POOL;
        config.max_concurrent = POOL;
        let jobserver = Jobserver::create(directory, POOL).expect("a fifo");
        let (adopted, killing, debt) = recovered(&jobserver);
        let (actor, _handle, _commands) = Leases::new(
            &config,
            Recovered {
                jobserver,
                adopted,
                killing,
                debt,
            },
            directory.join("leases.json"),
        );
        actor
    }

    fn nothing(_: &Jobserver) -> (Vec<Record>, Vec<Record>, u32) {
        (Vec::new(), Vec::new(), 0)
    }

    /// A static lease on `task` holding `cores_held` tokens.
    fn held(id: u64, task: usize, cores_held: u32) -> Record {
        Record {
            id,
            label: "make".into(),
            argv: vec!["make".into()],
            class: Class::Static,
            cores_held,
            pueue_task_id: Some(task),
            started_at_unix_ms: 0,
            submitted_at_unix_ms: None,
            fifo: None,
            killing: false,
        }
    }

    fn teardown(id: u64, task: usize, cores_held: u32) -> Record {
        Record {
            killing: true,
            ..held(id, task, cores_held)
        }
    }

    fn request(argv: &[&str]) -> LeaseRequest {
        LeaseRequest {
            argv: argv.iter().map(|a| (*a).to_string()).collect(),
            cwd: PathBuf::from("/tmp"),
            env: Default::default(),
            label: None,
            class_override: None,
            cores_wanted: None,
            detached: false,
        }
    }

    fn pending(record: Option<Record>) -> Kill {
        Kill::pending(
            Duration::from_millis(bzb_core::config::DEFAULT_KILL_GRACE_MS),
            record,
        )
    }

    fn free(actor: &Leases) -> u32 {
        actor.jobserver.free().expect("FIONREAD")
    }

    fn recorded(directory: &Path) -> Vec<Record> {
        let written = std::fs::read(directory.join("leases.json")).expect("read");
        serde_json::from_slice(&written).expect("decode")
    }

    #[tokio::test]
    async fn an_admission_held_back_is_reported_as_queued() {
        let _umask = hold_umask(0o022);
        let directory = tempfile::tempdir().expect("create a tempdir");
        let mut actor = actor(directory.path(), nothing);
        actor.killing.insert(9, pending(None));

        let (events, _stream) = mpsc::unbounded_channel();
        let (id, _asked) = oneshot::channel();
        actor.submit(request(&["cargo", "build"]), events, id).await;

        let status = actor.status().expect("the fifo is readable");
        assert_eq!(status.leases.len(), 1, "leases were {:?}", status.leases);
        assert_eq!(status.leases[0].state, "queued");
        assert_eq!(status.leases[0].pueue_task_id, None);
        assert_eq!(status.leases[0].ahead, Some(0));
    }

    #[tokio::test]
    async fn losing_pueued_returns_the_tokens_of_a_teardown_in_flight() {
        let _umask = hold_umask(0o022);
        let directory = tempfile::tempdir().expect("create a tempdir");
        let mut actor = actor(directory.path(), |jobserver| {
            assert_eq!(jobserver.acquire(3, Duration::ZERO).expect("acquire"), 3);
            nothing(jobserver)
        });
        actor.killing.insert(9, pending(Some(teardown(5, 9, 3))));
        assert_eq!(free(&actor), 1);

        actor
            .lose_running_leases(&BusybeeError::Other("gone".into()))
            .await;

        assert_eq!(free(&actor), 4, "the teardown's tokens never came back");
        assert!(actor.killing.is_empty());
        assert_eq!(
            std::fs::read_to_string(directory.path().join("leases.json")).expect("read"),
            "[]"
        );
    }

    #[test]
    fn a_teardown_in_flight_is_recorded_until_pueued_confirms_it_gone() {
        let _umask = hold_umask(0o022);
        let directory = tempfile::tempdir().expect("create a tempdir");
        let mut actor = actor(directory.path(), nothing);

        actor.book_teardown(9, Some(teardown(5, 9, 0)));

        assert!(actor.holding(), "the teardown holds nothing back");
        let records = recorded(directory.path());
        assert_eq!(records.len(), 1, "records were {records:?}");
        assert_eq!((records[0].id, records[0].pueue_task_id), (5, Some(9)));
        assert!(records[0].killing, "the teardown was recorded as a lease");
    }

    #[test]
    fn an_unanswered_submission_stays_on_record_until_the_poll_settles_it() {
        let _umask = hold_umask(0o022);
        let directory = tempfile::tempdir().expect("create a tempdir");
        let mut actor = actor(directory.path(), |jobserver| {
            assert_eq!(jobserver.acquire(2, Duration::ZERO).expect("acquire"), 2);
            nothing(jobserver)
        });
        let id = LeaseId(5);
        actor.leases.insert(
            id,
            Lease {
                request: request(&["make"]),
                conn: None,
                plan: Plan {
                    class: Class::Static,
                    ..classify(
                        &["make".to_string()],
                        &Overrides::default(),
                        &default_table(),
                    )
                },
                pueue_task_id: None,
                cores_held: 2,
                started_at: SystemTime::now(),
                submitted_at: Some(SystemTime::now()),
                fifo: None,
            },
        );

        actor.hold_unanswered(id);

        assert!(actor.leases.is_empty(), "the lease is over");
        assert!(actor.holding(), "the submission holds nothing back");
        assert_eq!(
            free(&actor),
            2,
            "the tokens came back while the task may hold them"
        );
        let records = recorded(directory.path());
        assert_eq!(records.len(), 1, "records were {records:?}");
        let record = &records[0];
        assert_eq!(
            (
                record.id,
                record.pueue_task_id,
                record.killing,
                record.cores_held
            ),
            (5, None, true, 2)
        );
        assert!(
            record.submitted_at_unix_ms.is_some(),
            "the restart matches the task by the submission time"
        );
    }

    #[test]
    fn a_recovered_teardown_holds_admissions_back() {
        let _umask = hold_umask(0o022);
        let directory = tempfile::tempdir().expect("create a tempdir");
        let actor = actor(directory.path(), |_| {
            (Vec::new(), vec![teardown(5, 9, 0)], 0)
        });

        assert!(actor.holding(), "the recovered teardown holds nothing back");
        assert!(actor.killing.contains_key(&9));
        assert_eq!(
            actor.next_id, 6,
            "ids must carry on from after the teardown's"
        );
    }

    #[test]
    fn a_recovered_pool_never_grows_past_its_size() {
        let _umask = hold_umask(0o022);
        let directory = tempfile::tempdir().expect("create a tempdir");
        let mut actor = actor(directory.path(), |jobserver| {
            // What `recover` does for a lease holding 6 of a pool of 4.
            assert_eq!(jobserver.acquire(4, Duration::ZERO).expect("acquire"), 4);
            (vec![held(5, 9, 6)], Vec::new(), 2)
        });
        assert_eq!(free(&actor), 0);

        actor.finish(LeaseId(5), 0);

        assert_eq!(free(&actor), 4, "the pool grew past its size");
        assert_eq!(actor.debt, 0);
    }

    // Multi-threaded: the drain in `admit` is a `block_in_place`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shrink_is_collected_before_the_admission_it_made_room_for() {
        let _umask = hold_umask(0o022);
        let directory = tempfile::tempdir().expect("create a tempdir");
        let mut actor = actor(directory.path(), nothing);
        // Holds the admission back so it can be driven by hand.
        actor.killing.insert(9, pending(None));
        let (events, _stream) = mpsc::unbounded_channel();
        let (id, asked) = oneshot::channel();
        actor.submit(request(&["make"]), events, id).await;
        asked.await.expect("the submission names its lease");
        assert!(
            matches!(actor.deferred.front(), Some(Action::Admit { .. })),
            "the admission was not held back: {:?}",
            actor.deferred
        );

        // Shrunk from four to two under a build that has since returned all four.
        actor.params.pool_size = 2;
        actor.debt = 2;
        assert_eq!(free(&actor), 4);
        // An unwritable record refuses the lease before it reaches pueued.
        std::fs::create_dir(directory.path().join("leases.json.tmp")).expect("plant the directory");

        actor.killing.clear();
        let waiting = std::mem::take(&mut actor.deferred);
        actor.drive(waiting.into()).await;

        assert_eq!(
            free(&actor),
            2,
            "the admission went out on a pool the shrink had not been collected from"
        );
        assert_eq!(actor.debt, 0);
    }
}
