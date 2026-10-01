//! Admission state machine (pure; no IO, no time source): FIFO, head only,
//! driven by [`Event`]s and answering with [`Action`]s. See
//! `docs/design/bzbd.md` §Admission policy.
//!
//! The machine tracks tokens but never moves them: an ending lease's tokens
//! must be back in the fifo before the daemon performs the actions returned
//! for that event, because the next [`Action::Admit`] is sized as if they
//! were already free.

use std::collections::{BTreeMap, VecDeque};

/// Identifies one lease for its whole lifetime. Allocated by the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseId(pub u64);

use crate::classify::Class;

/// A queued lease request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub id: LeaseId,
    pub class: Class,
    /// Static/none target; defaults to the fair share when absent.
    pub cores_wanted: Option<u32>,
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
    /// The daemon finished the drain and launched the task. Must be reported
    /// even when the lease was already dropped mid-drain, so the machine can
    /// ask for the now-live task to be torn down again.
    Started { id: LeaseId, cores_held: u32 },
    /// The task exited or was killed. The daemon has already returned the
    /// lease's tokens to the fifo, so no [`Action::Drop`] is emitted for it.
    Finished(LeaseId),
    /// The daemon could not launch the task at all (fifo unreadable, pueued
    /// submission failed). A short drain is *not* this: the task starts with
    /// what it collected and the daemon reports [`Event::Started`].
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
        /// The `{cores}` value: the fair share for [`Class::Jobserver`], which
        /// drains nothing (per admission, since one batch can admit leases
        /// with different shares); `None` for static/none, which are told
        /// what the drain collected (`drain_target` is only its upper bound).
        cores: Option<u32>,
    },
    /// The lease's queue position changed; tell the client `ahead` tasks
    /// are in front of it.
    Notify { id: LeaseId, ahead: usize },
    /// The lease is gone: tear down anything started for it and return its
    /// tokens. Always emitted before the [`Action::Admit`]s in the same batch,
    /// which are sized as if those tokens were already free.
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Head first.
    pub queued: Vec<Request>,
    /// Ordered by [`LeaseId`].
    pub admitted: Vec<AdmittedLease>,
}

struct Admitted {
    class: Class,
    cores_held: u32,
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

    /// Terminal events for unknown leases are ignored (the daemon can race a
    /// lease that just ended). [`Event::Started`] for one is answered with
    /// another [`Action::Drop`]: the task went live after its teardown.
    pub fn handle(&mut self, ev: Event) -> Vec<Action> {
        let mut actions = Vec::new();
        match ev {
            Event::Submit(r) => self.queue.push_back(r),
            Event::Started { id, cores_held } => match self.admitted.get_mut(&id) {
                Some(lease) => lease.cores_held = cores_held,
                None => actions.push(Action::Drop(id)),
            },
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

    /// Accounts for a lease a previous daemon left running (spec §Failure and
    /// recovery). Policy is not consulted; its slot and tokens count against
    /// everything admitted from here on.
    pub fn adopt(&mut self, r: Request, cores_held: u32) {
        self.admitted.insert(
            r.id,
            Admitted {
                class: r.class,
                cores_held,
            },
        );
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            queued: self.queue.iter().cloned().collect(),
            admitted: self
                .admitted
                .iter()
                .map(|(id, l)| AdmittedLease {
                    id: *id,
                    class: l.class,
                    cores_held: l.cores_held,
                })
                .collect(),
        }
    }

    /// Config reload. Held tokens are never revoked. Returns actions because
    /// raised limits can make the head eligible with no other event coming.
    pub fn set_params(&mut self, p: Params) -> Vec<Action> {
        self.params = p;
        let mut actions = self.admit_from_head();
        actions.extend(self.notify_queue());
        actions
    }

    /// Admit as long as the head qualifies.
    fn admit_from_head(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        while let Some(head) = self.queue.front() {
            let Some(drain_target) = self.drain_target(head) else {
                break;
            };
            let cores = match head.class {
                Class::Jobserver => Some(self.fair_share(None)),
                Class::Static | Class::None => None,
            };
            let r = self.queue.pop_front().expect("front() just returned Some");
            actions.push(Action::Admit {
                id: r.id,
                class: r.class,
                drain_target,
                cores,
            });
            self.admitted.insert(
                r.id,
                Admitted {
                    class: r.class,
                    cores_held: 0,
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
            // Stricter than `Σ cores_held == 0`: admitted-but-not-started and
            // jobserver leases hold no tokens yet still block exclusivity.
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
        }
    }

    fn submit_and_start(s: &mut Scheduler, r: Request, cores_held: u32) -> Vec<Action> {
        let id = r.id;
        let actions = s.handle(Event::Submit(r));
        s.handle(Event::Started { id, cores_held });
        actions
    }

    #[test]
    fn jobserver_leases_are_admitted_immediately_up_to_max_concurrent() {
        let mut s = Scheduler::new(params(8, 4));
        for id in 1..=4 {
            let actions = submit_and_start(&mut s, req(id, Class::Jobserver, None), 0);
            // The share each one gets is
            // `jobserver_admission_carries_a_fair_share_although_it_drains_nothing`.
            assert!(
                matches!(
                    actions[..],
                    [Action::Admit {
                        id: admitted,
                        class: Class::Jobserver,
                        drain_target: 0,
                        ..
                    }] if admitted == LeaseId(id)
                ),
                "lease {id} should be admitted at once, got {actions:?}"
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
    fn jobserver_admission_carries_a_fair_share_although_it_drains_nothing() {
        let mut s = Scheduler::new(params(8, 4));
        // fair = ceil(8 / 1), ceil(8 / 2), ceil(8 / 3): the share shrinks as
        // more leases are admitted, even though none of them holds a token.
        for (id, cores) in [(1, Some(8)), (2, Some(4)), (3, Some(3))] {
            let actions = submit_and_start(&mut s, req(id, Class::Jobserver, None), 0);
            assert_eq!(
                actions,
                vec![Action::Admit {
                    id: LeaseId(id),
                    class: Class::Jobserver,
                    drain_target: 0,
                    cores,
                }]
            );
        }
    }

    #[test]
    fn a_batch_of_admissions_gives_each_lease_its_own_share() {
        let mut s = Scheduler::new(params(8, 1));
        submit_and_start(&mut s, req(1, Class::Jobserver, None), 0);
        s.handle(Event::Submit(req(2, Class::Jobserver, None)));
        s.handle(Event::Submit(req(3, Class::Jobserver, None)));

        // One reload admits both queued leases; their shares differ because
        // each is computed against the admitted count at its own admission.
        let actions = s.set_params(params(8, 3));
        assert_eq!(
            actions,
            vec![
                Action::Admit {
                    id: LeaseId(2),
                    class: Class::Jobserver,
                    drain_target: 0,
                    cores: Some(4),
                },
                Action::Admit {
                    id: LeaseId(3),
                    class: Class::Jobserver,
                    drain_target: 0,
                    cores: Some(3),
                },
            ]
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
                drain_target: 8,
                cores: None,
            }]
        );
    }

    #[test]
    fn a_static_admission_leaves_the_core_count_to_the_drain_result() {
        let mut s = Scheduler::new(params(8, 2));
        // One lease already holds six of the eight tokens.
        submit_and_start(&mut s, req(1, Class::Static, Some(6)), 6);

        let actions = s.handle(Event::Submit(req(2, Class::Static, None)));
        // fair = ceil(8 / 2) = 4, but only two tokens are actually in the fifo.
        // The machine cannot know that, so it names the target and no `cores`:
        // telling the daemon to substitute `{cores}` = 4 would let the two
        // tasks demand ten cores from an eight-token pool.
        assert_eq!(
            actions,
            vec![Action::Admit {
                id: LeaseId(2),
                class: Class::Static,
                drain_target: 4,
                cores: None,
            }]
        );

        // The drain comes up short; `{cores}` is the two tokens it collected.
        s.handle(Event::Started {
            id: LeaseId(2),
            cores_held: 2,
        });
        assert_eq!(s.snapshot().admitted[1].cores_held, 2);
    }

    #[test]
    fn a_drain_that_collects_no_tokens_still_runs_on_the_implicit_core() {
        let mut s = Scheduler::new(params(8, 2));
        submit_and_start(&mut s, req(1, Class::Static, Some(8)), 8);
        s.handle(Event::Submit(req(2, Class::Static, None)));

        // The pool is empty, so the second drain reaches its deadline without
        // reading a token. That is ordinary token exhaustion, not a failure:
        // the task starts on the implicit core and keeps its slot.
        assert_eq!(
            s.handle(Event::Started {
                id: LeaseId(2),
                cores_held: 0
            }),
            vec![]
        );
        let snap = s.snapshot();
        assert_eq!(
            snap.admitted.iter().map(|l| l.id).collect::<Vec<_>>(),
            vec![LeaseId(1), LeaseId(2)]
        );
        // Still admitted, so it holds its slot and blocks an exclusive lease.
        assert_eq!(
            s.handle(Event::Submit(req(3, Class::None, None))),
            vec![Action::Notify {
                id: LeaseId(3),
                ahead: 0
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
                drain_target: 3,
                cores: None,
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
                drain_target: 2,
                cores: None,
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
                drain_target: 4,
                cores: None,
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
                drain_target: 1,
                cores: None,
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
                drain_target: 8,
                cores: None,
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

        // Even once the pool frees, only the exclusive head is admitted; the
        // jobserver lease behind it just moves up a place.
        let actions = s.handle(Event::Finished(LeaseId(1)));
        assert_eq!(
            actions,
            vec![
                Action::Admit {
                    id: LeaseId(2),
                    class: Class::None,
                    drain_target: 8,
                    cores: None,
                },
                Action::Notify {
                    id: LeaseId(3),
                    ahead: 0
                },
            ]
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
                    drain_target: 0,
                    cores: Some(8),
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
                    drain_target: 0,
                    cores: Some(8),
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
    fn terminal_events_for_unknown_leases_are_ignored() {
        let mut s = Scheduler::new(params(8, 4));
        assert_eq!(s.handle(Event::Finished(LeaseId(9))), vec![]);
        assert_eq!(s.handle(Event::Cancel(LeaseId(9))), vec![]);
        assert_eq!(s.handle(Event::DrainFailed(LeaseId(9))), vec![]);
        assert!(s.snapshot().admitted.is_empty());
    }

    #[test]
    fn late_started_for_an_untracked_lease_is_dropped_again() {
        let mut s = Scheduler::new(params(8, 1));
        s.handle(Event::Submit(req(1, Class::Static, Some(4))));
        s.handle(Event::Submit(req(2, Class::Jobserver, None)));

        // The client goes away mid-drain: the lease is torn down and the slot
        // handed to the one behind it.
        let actions = s.handle(Event::Cancel(LeaseId(1)));
        assert_eq!(
            actions,
            vec![
                Action::Drop(LeaseId(1)),
                Action::Admit {
                    id: LeaseId(2),
                    class: Class::Jobserver,
                    drain_target: 0,
                    cores: Some(8),
                },
            ]
        );

        // The drain had already finished and the task launched anyway: it is
        // live and holds four tokens the machine no longer tracks. Ignoring
        // this would leak both the process and its tokens, so ask for teardown
        // again rather than treating it as a harmless stale event.
        assert_eq!(
            s.handle(Event::Started {
                id: LeaseId(1),
                cores_held: 4
            }),
            vec![Action::Drop(LeaseId(1))]
        );
        // Still untracked: the daemon returns the tokens as part of the drop.
        assert!(s.snapshot().admitted.iter().all(|l| l.id != LeaseId(1)));
    }

    #[test]
    fn snapshot_reports_queue_order_and_admitted_leases() {
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
                },
                AdmittedLease {
                    id: LeaseId(2),
                    class: Class::Static,
                    cores_held: 3,
                },
            ]
        );
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

        // The new pool size governs the next admission.
        s.handle(Event::Finished(LeaseId(1)));
        let actions = s.handle(Event::Submit(req(2, Class::Static, None)));
        assert_eq!(
            actions,
            vec![Action::Admit {
                id: LeaseId(2),
                class: Class::Static,
                drain_target: 4,
                cores: None,
            }]
        );
    }

    #[test]
    fn set_params_raising_max_concurrent_admits_the_waiting_head() {
        let mut s = Scheduler::new(params(8, 1));
        submit_and_start(&mut s, req(1, Class::Jobserver, None), 0);
        s.handle(Event::Submit(req(2, Class::Jobserver, None)));
        s.handle(Event::Submit(req(3, Class::Jobserver, None)));

        // Nothing else happens on the machine: without re-evaluation the new
        // slot would stay unused until a running task ends.
        let actions = s.set_params(params(8, 2));
        assert_eq!(
            actions,
            vec![
                Action::Admit {
                    id: LeaseId(2),
                    class: Class::Jobserver,
                    drain_target: 0,
                    cores: Some(4),
                },
                Action::Notify {
                    id: LeaseId(3),
                    ahead: 0
                },
            ]
        );
    }

    #[test]
    fn set_params_that_admits_nothing_yields_no_actions() {
        let mut s = Scheduler::new(params(8, 1));
        submit_and_start(&mut s, req(1, Class::Jobserver, None), 0);
        s.handle(Event::Submit(req(2, Class::Jobserver, None)));

        assert_eq!(s.set_params(params(4, 1)), vec![]);
    }

    #[test]
    fn finished_releases_the_lease_without_a_drop() {
        let mut s = Scheduler::new(params(8, 1));
        submit_and_start(&mut s, req(1, Class::Static, Some(4)), 4);
        s.handle(Event::Submit(req(2, Class::None, None)));

        // The task already exited, so there is nothing for the daemon to tear
        // down; it returned the four tokens before feeding `Finished` in, so
        // the exclusive lease may drain the whole pool.
        let actions = s.handle(Event::Finished(LeaseId(1)));
        assert_eq!(
            actions,
            vec![Action::Admit {
                id: LeaseId(2),
                class: Class::None,
                drain_target: 8,
                cores: None,
            }]
        );
    }

    #[test]
    fn an_adopted_lease_is_admitted_without_policy_and_counts_against_the_rest() {
        let mut s = Scheduler::new(params(4, 1));
        s.adopt(req(7, Class::Static, None), 3);
        // No `Started` to wait for: the tokens are already held.
        assert_eq!(s.snapshot().admitted[0].cores_held, 3);
        // And a second one is taken on even though `max_concurrent` is 1.
        s.adopt(req(8, Class::None, None), 0);

        // Allocated after the adopted ids, so nothing collides.
        let actions = s.handle(Event::Submit(req(9, Class::Jobserver, None)));
        assert_eq!(
            actions,
            vec![Action::Notify {
                id: LeaseId(9),
                ahead: 0
            }]
        );

        s.handle(Event::Finished(LeaseId(8)));
        assert_eq!(
            s.handle(Event::Finished(LeaseId(7))),
            vec![Action::Admit {
                id: LeaseId(9),
                class: Class::Jobserver,
                drain_target: 0,
                cores: Some(4),
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
