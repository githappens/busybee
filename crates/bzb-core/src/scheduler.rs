//! Admission state machine (pure; no IO, no time source).
//!
//! See `docs/design/bzbd.md` § "Admission policy". Restated here so the daemon
//! can integrate without re-deriving the rules:
//!
//! Queue is FIFO and only the head is considered. A lease at the head is
//! admitted when:
//!
//! 1. `admitted_count < max_concurrent` (default 4). Needed because
//!    make/ninja/cargo each run one job *without* a token (the implicit
//!    token), so unbounded admission would add one uncounted job per task.
//! 2. Class-specific:
//!    - [`Class::Jobserver`]: admitted as soon as (1) holds; takes no tokens
//!      up front (`drain_target` 0).
//!    - [`Class::Static`]: `drain_target = clamp(cores_wanted, 1, fair)` where
//!      `fair = ceil(pool_size / (admitted_count + 1))` — fair share at the
//!      moment of admission. `cores_wanted` defaults to `fair`.
//!    - [`Class::None`]: static with `cores_wanted = pool_size`, and
//!      additionally only when nothing at all is admitted (exclusive: today's
//!      whole-machine behaviour; admitted jobserver leases block it too).
//!
//! Head-of-line blocking is intentional: a `none` lease waits for the pool to
//! be fully free, and everything behind it waits too. Priorities, preemption
//! and reordering are out of scope.
//!
//! The machine is driven by [`Event`]s and answers with [`Action`]s; the
//! daemon performs the IO (draining fifo tokens, submitting to pueued,
//! notifying clients) and reports back with `Started`/`Finished`.

use std::collections::{BTreeMap, VecDeque};

/// Identifies one lease for its whole lifetime. Allocated by the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseId(pub u64);

/// How a task shares the token pool.
///
/// Mirrors the classification table in the spec; will be re-exported from
/// `classify` once that module lands (#3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Speaks the jobserver protocol: takes no tokens up front and
    /// self-balances at compile-job granularity.
    Jobserver,
    /// Cannot speak jobserver: holds a fixed number of tokens for its lifetime.
    Static,
    /// Unrecognised or explicitly exclusive: wants the whole machine.
    None,
}

/// A queued lease request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub id: LeaseId,
    pub class: Class,
    /// Static/none target; defaults to the fair share when absent.
    pub cores_wanted: Option<u32>,
    /// Human-readable label, surfaced by `busybee status`.
    pub label: String,
}

/// Admission parameters, from the config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// Tokens in the pool; default = logical cores.
    pub pool_size: u32,
    /// Maximum leases admitted at once (default 4).
    pub max_concurrent: u32,
}

/// Something that happened outside the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A client asked for a lease; it joins the tail of the queue.
    Submit(Request),
    /// The client went away while queued, or its running task must be torn down.
    Cancel(LeaseId),
    /// The daemon finished the drain and launched the task.
    Started { id: LeaseId, cores_held: u32 },
    /// The task exited or was killed.
    Finished(LeaseId),
    /// The daemon could not collect a single token in time. Should not happen
    /// (the implicit token guarantees one); keeps the machine honest.
    DrainFailed(LeaseId),
}

/// Something the daemon must do. Emitted in a deterministic order:
/// [`Action::Drop`]s, then [`Action::Admit`]s in queue order, then
/// [`Action::Notify`]s in queue order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Drain `drain_target` tokens (0 for [`Class::Jobserver`]) and start the
    /// task, then report back with [`Event::Started`].
    Admit {
        id: LeaseId,
        class: Class,
        drain_target: u32,
    },
    /// The lease's queue position changed; tell the client `ahead` tasks
    /// are in front of it.
    Notify { id: LeaseId, ahead: usize },
    /// The lease is gone: tear down anything started for it and return its
    /// tokens.
    Drop(LeaseId),
}

/// An admitted lease, as exposed by [`Scheduler::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedLease {
    pub id: LeaseId,
    pub class: Class,
    /// Tokens the daemon actually collected, 0 until [`Event::Started`]
    /// arrives (and permanently 0 for [`Class::Jobserver`]).
    pub cores_held: u32,
    pub label: String,
}

/// Read-only view for `busybee status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Waiting leases, head first.
    pub queued: Vec<Request>,
    /// Admitted leases, ordered by [`LeaseId`].
    pub admitted: Vec<AdmittedLease>,
    /// `pool_size − Σ cores_held`, saturating at 0. The daemon overlays the
    /// real `FIONREAD` value; this is only an estimate.
    pub free_estimate: u32,
}

struct Admitted {
    class: Class,
    cores_held: u32,
    label: String,
}

/// FIFO admission state machine. Drive it with [`Scheduler::handle`].
pub struct Scheduler {
    params: Params,
    queue: VecDeque<Request>,
    admitted: BTreeMap<LeaseId, Admitted>,
    /// Last `ahead` value reported per queued lease, so [`Action::Notify`] is
    /// only emitted when the position actually changed.
    notified: BTreeMap<LeaseId, usize>,
}

impl Scheduler {
    pub fn new(params: Params) -> Self {
        Self {
            params,
            queue: VecDeque::new(),
            admitted: BTreeMap::new(),
            notified: BTreeMap::new(),
        }
    }

    /// Feed one event, get the actions the daemon must perform. Events naming
    /// a lease the machine does not know about are ignored (the daemon and the
    /// machine can race on a lease that just ended).
    pub fn handle(&mut self, ev: Event) -> Vec<Action> {
        let mut actions = Vec::new();
        match ev {
            Event::Submit(r) => self.queue.push_back(r),
            Event::Started { id, cores_held } => {
                if let Some(lease) = self.admitted.get_mut(&id) {
                    lease.cores_held = cores_held;
                }
            }
            Event::Finished(id) => {
                self.admitted.remove(&id);
            }
            // Queued: nothing to tear down. Admitted: the daemon must kill the
            // task and return its tokens.
            Event::Cancel(id) => {
                if self.admitted.remove(&id).is_some() {
                    actions.push(Action::Drop(id));
                } else if let Some(pos) = self.queue.iter().position(|r| r.id == id) {
                    self.queue.remove(pos);
                }
            }
            Event::DrainFailed(id) => {
                if self.admitted.remove(&id).is_some() {
                    actions.push(Action::Drop(id));
                }
            }
        }
        actions.extend(self.admit_from_head());
        actions.extend(self.notify_queue());
        actions
    }

    pub fn snapshot(&self) -> Snapshot {
        let held: u32 = self.admitted.values().map(|l| l.cores_held).sum();
        Snapshot {
            queued: self.queue.iter().cloned().collect(),
            admitted: self
                .admitted
                .iter()
                .map(|(id, l)| AdmittedLease {
                    id: *id,
                    class: l.class,
                    cores_held: l.cores_held,
                    label: l.label.clone(),
                })
                .collect(),
            free_estimate: self.params.pool_size.saturating_sub(held),
        }
    }

    /// Config reload. Already-held tokens are never revoked: a shrunk
    /// `pool_size` only affects future admissions and clamps
    /// [`Snapshot::free_estimate`] at 0.
    pub fn set_params(&mut self, p: Params) {
        self.params = p;
    }

    /// Admit as long as the head qualifies. Only the head is ever considered.
    fn admit_from_head(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        while let Some(head) = self.queue.front() {
            let Some(drain_target) = self.drain_target(head) else {
                break;
            };
            let r = self.queue.pop_front().expect("front() just returned Some");
            actions.push(Action::Admit {
                id: r.id,
                class: r.class,
                drain_target,
            });
            self.admitted.insert(
                r.id,
                Admitted {
                    class: r.class,
                    cores_held: 0,
                    label: r.label,
                },
            );
        }
        actions
    }

    /// Tokens the daemon should drain for `r`, or `None` if it cannot be
    /// admitted yet.
    fn drain_target(&self, r: &Request) -> Option<u32> {
        if self.admitted.len() as u32 >= self.params.max_concurrent {
            return None;
        }
        // An admitted `none` lease owns the machine until it ends.
        if self.admitted.values().any(|l| l.class == Class::None) {
            return None;
        }
        match r.class {
            Class::Jobserver => Some(0),
            Class::Static => Some(self.fair_share(r.cores_wanted)),
            // Exclusive. Note this is stricter than `Σ cores_held == 0`: a
            // lease admitted but not yet `Started` holds no tokens *yet* but is
            // about to, and admitted jobserver leases never hold any.
            Class::None if self.admitted.is_empty() => {
                Some(self.fair_share(Some(self.params.pool_size)))
            }
            Class::None => None,
        }
    }

    /// `clamp(cores_wanted, 1, ceil(pool_size / (admitted_count + 1)))`,
    /// defaulting `cores_wanted` to the fair share itself.
    fn fair_share(&self, cores_wanted: Option<u32>) -> u32 {
        let fair = self
            .params
            .pool_size
            .div_ceil(self.admitted.len() as u32 + 1)
            .max(1);
        cores_wanted.unwrap_or(fair).clamp(1, fair)
    }

    /// One [`Action::Notify`] per queued lease whose position changed.
    fn notify_queue(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut notified = BTreeMap::new();
        for (ahead, r) in self.queue.iter().enumerate() {
            if self.notified.get(&r.id) != Some(&ahead) {
                actions.push(Action::Notify { id: r.id, ahead });
            }
            notified.insert(r.id, ahead);
        }
        self.notified = notified;
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pool_size: u32, max_concurrent: u32) -> Params {
        Params {
            pool_size,
            max_concurrent,
        }
    }

    fn req(id: u64, class: Class, cores_wanted: Option<u32>) -> Request {
        Request {
            id: LeaseId(id),
            class,
            cores_wanted,
            label: format!("task {id}"),
        }
    }

    /// Submit, then report the drain finished with `cores_held` tokens.
    fn submit_and_start(s: &mut Scheduler, r: Request, cores_held: u32) -> Vec<Action> {
        let id = r.id;
        let actions = s.handle(Event::Submit(r));
        s.handle(Event::Started { id, cores_held });
        actions
    }

    fn admits(actions: &[Action]) -> Vec<&Action> {
        actions
            .iter()
            .filter(|a| matches!(a, Action::Admit { .. }))
            .collect()
    }

    #[test]
    fn jobserver_leases_are_admitted_immediately_up_to_max_concurrent() {
        let mut s = Scheduler::new(params(8, 4));
        for id in 1..=4 {
            let actions = submit_and_start(&mut s, req(id, Class::Jobserver, None), 0);
            assert_eq!(
                actions,
                vec![Action::Admit {
                    id: LeaseId(id),
                    class: Class::Jobserver,
                    drain_target: 0
                }],
                "lease {id} should be admitted at once"
            );
        }
        // Fifth waits: max_concurrent is 4.
        let actions = s.handle(Event::Submit(req(5, Class::Jobserver, None)));
        assert_eq!(
            actions,
            vec![Action::Notify {
                id: LeaseId(5),
                ahead: 0
            }]
        );
    }

    #[test]
    fn static_alone_drains_the_whole_pool() {
        let mut s = Scheduler::new(params(8, 4));
        let actions = s.handle(Event::Submit(req(1, Class::Static, None)));
        assert_eq!(
            actions,
            vec![Action::Admit {
                id: LeaseId(1),
                class: Class::Static,
                drain_target: 8
            }]
        );
    }

    #[test]
    fn static_with_two_admitted_gets_a_third_of_the_pool() {
        let mut s = Scheduler::new(params(8, 4));
        submit_and_start(&mut s, req(1, Class::Jobserver, None), 0);
        submit_and_start(&mut s, req(2, Class::Jobserver, None), 0);
        let actions = s.handle(Event::Submit(req(3, Class::Static, None)));
        // fair = ceil(8 / 3) = 3
        assert_eq!(
            actions,
            vec![Action::Admit {
                id: LeaseId(3),
                class: Class::Static,
                drain_target: 3
            }]
        );
    }

    #[test]
    fn static_cores_wanted_below_fair_share_is_honoured() {
        let mut s = Scheduler::new(params(8, 4));
        let actions = s.handle(Event::Submit(req(1, Class::Static, Some(2))));
        assert_eq!(
            actions,
            vec![Action::Admit {
                id: LeaseId(1),
                class: Class::Static,
                drain_target: 2
            }]
        );
    }

    #[test]
    fn static_cores_wanted_above_fair_share_is_clamped() {
        let mut s = Scheduler::new(params(8, 4));
        submit_and_start(&mut s, req(1, Class::Jobserver, None), 0);
        let actions = s.handle(Event::Submit(req(2, Class::Static, Some(100))));
        // fair = ceil(8 / 2) = 4
        assert_eq!(
            actions,
            vec![Action::Admit {
                id: LeaseId(2),
                class: Class::Static,
                drain_target: 4
            }]
        );
    }

    #[test]
    fn static_cores_wanted_zero_is_clamped_up_to_one() {
        let mut s = Scheduler::new(params(8, 4));
        let actions = s.handle(Event::Submit(req(1, Class::Static, Some(0))));
        assert_eq!(
            actions,
            vec![Action::Admit {
                id: LeaseId(1),
                class: Class::Static,
                drain_target: 1
            }]
        );
    }

    #[test]
    fn none_waits_while_any_lease_is_admitted() {
        let mut s = Scheduler::new(params(8, 4));
        // A jobserver lease holds no tokens, but still blocks an exclusive lease.
        submit_and_start(&mut s, req(1, Class::Jobserver, None), 0);
        let actions = s.handle(Event::Submit(req(2, Class::None, None)));
        assert_eq!(
            actions,
            vec![Action::Notify {
                id: LeaseId(2),
                ahead: 0
            }]
        );
    }

    #[test]
    fn none_is_admitted_when_the_last_lease_finishes() {
        let mut s = Scheduler::new(params(8, 4));
        submit_and_start(&mut s, req(1, Class::Jobserver, None), 0);
        submit_and_start(&mut s, req(2, Class::Static, Some(4)), 4);
        s.handle(Event::Submit(req(3, Class::None, None)));

        assert_eq!(s.handle(Event::Finished(LeaseId(1))), vec![]);
        assert_eq!(
            s.handle(Event::Finished(LeaseId(2))),
            vec![Action::Admit {
                id: LeaseId(3),
                class: Class::None,
                drain_target: 8
            }]
        );
    }

    #[test]
    fn none_blocks_everything_behind_it() {
        let mut s = Scheduler::new(params(8, 4));
        submit_and_start(&mut s, req(1, Class::Jobserver, None), 0);
        s.handle(Event::Submit(req(2, Class::None, None)));
        // Head-of-line blocking: the jobserver lease behind it waits too.
        let actions = s.handle(Event::Submit(req(3, Class::Jobserver, None)));
        assert_eq!(
            actions,
            vec![Action::Notify {
                id: LeaseId(3),
                ahead: 1
            }]
        );

        // Even once the pool frees, only the exclusive head is admitted.
        let actions = s.handle(Event::Finished(LeaseId(1)));
        assert_eq!(
            admits(&actions),
            vec![&Action::Admit {
                id: LeaseId(2),
                class: Class::None,
                drain_target: 8
            }]
        );
    }

    #[test]
    fn cancel_of_a_queued_lease_notifies_those_behind_it() {
        let mut s = Scheduler::new(params(8, 1));
        submit_and_start(&mut s, req(1, Class::Jobserver, None), 0);
        s.handle(Event::Submit(req(2, Class::Jobserver, None)));
        s.handle(Event::Submit(req(3, Class::Jobserver, None)));
        s.handle(Event::Submit(req(4, Class::Jobserver, None)));

        let actions = s.handle(Event::Cancel(LeaseId(2)));
        assert_eq!(
            actions,
            vec![
                Action::Notify {
                    id: LeaseId(3),
                    ahead: 0
                },
                Action::Notify {
                    id: LeaseId(4),
                    ahead: 1
                },
            ]
        );
    }

    #[test]
    fn cancel_of_an_admitted_lease_drops_it_and_frees_the_slot() {
        let mut s = Scheduler::new(params(8, 1));
        submit_and_start(&mut s, req(1, Class::Jobserver, None), 0);
        s.handle(Event::Submit(req(2, Class::Jobserver, None)));

        let actions = s.handle(Event::Cancel(LeaseId(1)));
        assert_eq!(
            actions,
            vec![
                Action::Drop(LeaseId(1)),
                Action::Admit {
                    id: LeaseId(2),
                    class: Class::Jobserver,
                    drain_target: 0
                },
            ]
        );
        assert!(s.snapshot().queued.is_empty());
    }

    #[test]
    fn drain_failed_drops_the_lease_and_admits_the_next_head() {
        let mut s = Scheduler::new(params(8, 1));
        s.handle(Event::Submit(req(1, Class::Static, None)));
        s.handle(Event::Submit(req(2, Class::Jobserver, None)));

        let actions = s.handle(Event::DrainFailed(LeaseId(1)));
        assert_eq!(
            actions,
            vec![
                Action::Drop(LeaseId(1)),
                Action::Admit {
                    id: LeaseId(2),
                    class: Class::Jobserver,
                    drain_target: 0
                },
            ]
        );
    }

    #[test]
    fn an_admitting_lease_counts_towards_max_concurrent_before_it_starts() {
        let mut s = Scheduler::new(params(8, 1));
        // Admitted but not yet Started.
        s.handle(Event::Submit(req(1, Class::Static, None)));
        let actions = s.handle(Event::Submit(req(2, Class::Jobserver, None)));
        assert_eq!(
            actions,
            vec![Action::Notify {
                id: LeaseId(2),
                ahead: 0
            }]
        );
    }

    #[test]
    fn events_for_unknown_leases_are_ignored() {
        let mut s = Scheduler::new(params(8, 4));
        assert_eq!(s.handle(Event::Finished(LeaseId(9))), vec![]);
        assert_eq!(s.handle(Event::Cancel(LeaseId(9))), vec![]);
        assert_eq!(s.handle(Event::DrainFailed(LeaseId(9))), vec![]);
        assert_eq!(
            s.handle(Event::Started {
                id: LeaseId(9),
                cores_held: 3
            }),
            vec![]
        );
        assert_eq!(s.snapshot().free_estimate, 8);
    }

    #[test]
    fn snapshot_reports_queue_order_admitted_leases_and_free_estimate() {
        let mut s = Scheduler::new(params(8, 2));
        submit_and_start(&mut s, req(1, Class::Jobserver, None), 0);
        submit_and_start(&mut s, req(2, Class::Static, Some(3)), 3);
        s.handle(Event::Submit(req(3, Class::Static, None)));
        s.handle(Event::Submit(req(4, Class::None, None)));

        let snap = s.snapshot();
        assert_eq!(
            snap.queued.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![LeaseId(3), LeaseId(4)]
        );
        assert_eq!(
            snap.admitted,
            vec![
                AdmittedLease {
                    id: LeaseId(1),
                    class: Class::Jobserver,
                    cores_held: 0,
                    label: "task 1".into(),
                },
                AdmittedLease {
                    id: LeaseId(2),
                    class: Class::Static,
                    cores_held: 3,
                    label: "task 2".into(),
                },
            ]
        );
        assert_eq!(snap.free_estimate, 5);
    }

    #[test]
    fn set_params_with_a_smaller_pool_keeps_already_held_tokens() {
        let mut s = Scheduler::new(params(8, 4));
        submit_and_start(&mut s, req(1, Class::Static, Some(8)), 8);

        s.set_params(params(4, 4));

        let snap = s.snapshot();
        assert_eq!(
            snap.admitted[0].cores_held, 8,
            "held tokens are never revoked"
        );
        assert_eq!(snap.free_estimate, 0, "free estimate saturates at 0");

        // The new pool size governs the next admission.
        s.handle(Event::Finished(LeaseId(1)));
        let actions = s.handle(Event::Submit(req(2, Class::Static, None)));
        assert_eq!(
            actions,
            vec![Action::Admit {
                id: LeaseId(2),
                class: Class::Static,
                drain_target: 4
            }]
        );
    }

    #[test]
    fn the_same_event_sequence_yields_the_same_actions() {
        let events = || {
            vec![
                Event::Submit(req(1, Class::Jobserver, None)),
                Event::Started {
                    id: LeaseId(1),
                    cores_held: 0,
                },
                Event::Submit(req(2, Class::Static, Some(3))),
                Event::Started {
                    id: LeaseId(2),
                    cores_held: 3,
                },
                Event::Submit(req(3, Class::None, None)),
                Event::Submit(req(4, Class::Jobserver, None)),
                Event::Cancel(LeaseId(3)),
                Event::Finished(LeaseId(1)),
                Event::DrainFailed(LeaseId(4)),
                Event::Finished(LeaseId(2)),
            ]
        };
        let run = || {
            let mut s = Scheduler::new(params(8, 2));
            let actions: Vec<Action> = events().into_iter().flat_map(|e| s.handle(e)).collect();
            (actions, s.snapshot())
        };
        assert_eq!(run(), run());
    }
}
