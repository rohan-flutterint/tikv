// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

use std::sync::{Arc, RwLock};

use collections::HashMap;
use engine_traits::KvEngine;
use fail::fail_point;
use kvproto::metapb::{Peer, Region};
use raft::StateRole;
use raftstore::{Error as RaftStoreError, coprocessor::*, store::RegionSnapshot};
use tikv::storage::Statistics;
use tikv_util::{error, memory::MemoryQuota, warn, worker::Scheduler};

use crate::{
    Error as CdcError,
    endpoint::{Deregister, Task},
    old_value::{self, OldValueCache},
};

/// An Observer for CDC.
///
/// It observes raftstore internal events, such as:
///   1. Raft role change events,
///   2. Apply command events.
#[derive(Clone)]
pub struct CdcObserver {
    sched: Scheduler<Task>,
    memory_quota: Arc<MemoryQuota>,
    // A shared registry for managing observed regions.
    // TODO: it may become a bottleneck, find a better way to manage the registry.
    observe_regions: Arc<RwLock<HashMap<u64, ObserveId>>>,
}

impl CdcObserver {
    /// Create a new `CdcObserver`.
    ///
    /// Events are strong ordered, so `sched` must be implemented as
    /// a FIFO queue.
    pub fn new(sched: Scheduler<Task>, memory_quota: Arc<MemoryQuota>) -> CdcObserver {
        CdcObserver {
            sched,
            memory_quota,
            observe_regions: Arc::default(),
        }
    }

    pub fn register_to(&self, coprocessor_host: &mut CoprocessorHost<impl KvEngine>) {
        // use 0 as the priority of the cmd observer. CDC should have a higher priority
        // than the `resolved-ts`'s cmd observer
        coprocessor_host
            .registry
            .register_cmd_observer(0, BoxCmdObserver::new(self.clone()));
        coprocessor_host
            .registry
            .register_role_observer(100, BoxRoleObserver::new(self.clone()));
        coprocessor_host
            .registry
            .register_region_change_observer(100, BoxRegionChangeObserver::new(self.clone()));
    }

    /// Subscribe a region, the observer will sink events of the region into
    /// its scheduler.
    ///
    /// Return previous ObserveId if there is one.
    pub fn subscribe_region(&self, region_id: u64, observe_id: ObserveId) -> Option<ObserveId> {
        self.observe_regions
            .write()
            .unwrap()
            .insert(region_id, observe_id)
    }

    /// Stops observe the region.
    ///
    /// Return ObserverID if unsubscribe successfully.
    pub fn unsubscribe_region(&self, region_id: u64, observe_id: ObserveId) -> Option<ObserveId> {
        let mut regions = self.observe_regions.write().unwrap();
        // To avoid ABA problem, we must check the unique ObserveId.
        if let Some(oid) = regions.get(&region_id) {
            if *oid == observe_id {
                return regions.remove(&region_id);
            }
        }
        None
    }

    /// Check whether the region is subscribed or not.
    pub fn is_subscribed(&self, region_id: u64) -> Option<ObserveId> {
        self.observe_regions
            .read()
            .unwrap()
            .get(&region_id)
            .cloned()
    }
}

impl Coprocessor for CdcObserver {}

impl<E: KvEngine> CmdObserver<E> for CdcObserver {
    // `CdcObserver::on_flush_applied_cmd_batch` should only invoke if `cmd_batches`
    // is not empty
    fn on_flush_applied_cmd_batch(
        &self,
        max_level: ObserveLevel,
        cmd_batches: &mut Vec<CmdBatch>,
        engine: &E,
    ) {
        assert!(!cmd_batches.is_empty());
        fail_point!("before_cdc_flush_apply");

        if max_level < ObserveLevel::All {
            return;
        }
        let cmd_batches: Vec<_> = cmd_batches
            .iter()
            .filter(|cb| cb.level == ObserveLevel::All && !cb.is_empty())
            .cloned()
            .collect();
        if cmd_batches.is_empty() {
            return;
        }
        let mut region = Region::default();
        region.mut_peers().push(Peer::default());
        // Create a snapshot here for preventing the old value was GC-ed.
        let snapshot = RegionSnapshot::from_snapshot(Arc::new(engine.snapshot()), Arc::new(region));
        let get_old_value = move |key,
                                  query_ts,
                                  old_value_cache: &mut OldValueCache,
                                  statistics: &mut Statistics| {
            old_value::get_old_value(&snapshot, key, query_ts, old_value_cache, statistics)
        };

        let size = cmd_batches.iter().map(|b| b.size()).sum();
        self.memory_quota.alloc_force(size);
        if let Err(e) = self.sched.schedule(Task::MultiBatch {
            multi: cmd_batches,
            old_value_cb: Box::new(get_old_value),
        }) {
            warn!("cdc schedule task failed"; "error" => ?e);
        }
    }

    fn on_applied_current_term(&self, _: StateRole, _: &Region) {}
}

impl RoleObserver for CdcObserver {
    fn on_role_change(&self, ctx: &mut ObserverContext<'_>, role_change: &RoleChange) {
        if role_change.state != StateRole::Leader {
            let region_id = ctx.region().get_id();
            if let Some(observe_id) = self.is_subscribed(region_id) {
                let leader_id = if role_change.leader_id != raft::INVALID_ID {
                    Some(role_change.leader_id)
                } else if role_change.prev_lead_transferee == role_change.vote {
                    Some(role_change.prev_lead_transferee)
                } else {
                    None
                };
                let leader = leader_id
                    .and_then(|x| ctx.region().get_peers().iter().find(|p| p.id == x))
                    .cloned();

                // Unregister all downstreams.
                let store_err = RaftStoreError::NotLeader(region_id, leader);
                let deregister = Deregister::Delegate {
                    region_id,
                    observe_id,
                    err: CdcError::request(store_err.into()),
                };
                if let Err(e) = self.sched.schedule(Task::Deregister(deregister)) {
                    error!("cdc schedule cdc task failed"; "error" => ?e);
                }
            }
        }
    }
}

impl RegionChangeObserver for CdcObserver {
    fn on_region_changed(
        &self,
        ctx: &mut ObserverContext<'_>,
        event: RegionChangeEvent,
        _: StateRole,
    ) {
        match event {
            RegionChangeEvent::Destroy
            | RegionChangeEvent::Update(
                RegionChangeReason::Split | RegionChangeReason::CommitMerge,
            ) => {
                let region_id = ctx.region().get_id();
                if let Some(observe_id) = self.is_subscribed(region_id) {
                    // Unregister all downstreams.
                    let store_err = RaftStoreError::RegionNotFound(region_id);
                    let deregister = Deregister::Delegate {
                        region_id,
                        observe_id,
                        err: CdcError::request(store_err.into()),
                    };
                    if let Err(e) = self.sched.schedule(Task::Deregister(deregister)) {
                        error!("cdc schedule cdc task failed"; "error" => ?e);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use engine_rocks::RocksEngine;
    use kvproto::metapb::Region;
    use raftstore::coprocessor::RoleChange;
    use tikv::storage::kv::TestEngineBuilder;
    use tikv_util::{store::new_peer, worker::dummy_scheduler};
    use txn_types::{TxnExtra, TxnExtraScheduler};

    use super::*;
    use crate::CdcTxnExtraScheduler;

    #[test]
    fn test_register_and_deregister() {
        let (scheduler, mut rx) = tikv_util::worker::dummy_scheduler();
        let memory_quota = Arc::new(MemoryQuota::new(usize::MAX));
        let observer = CdcObserver::new(scheduler, memory_quota.clone());
        let observe_info = CmdObserveInfo::from_handle(
            ObserveHandle::new(),
            ObserveHandle::new(),
            ObserveHandle::new(),
        );
        let engine = TestEngineBuilder::new().build().unwrap().get_rocksdb();

        let mut cb = CmdBatch::new(&observe_info, 0);
        cb.push(&observe_info, 0, Cmd::default());
        let size = cb.size();
        <CdcObserver as CmdObserver<RocksEngine>>::on_flush_applied_cmd_batch(
            &observer,
            cb.level,
            &mut vec![cb],
            &engine,
        );
        assert_eq!(memory_quota.in_use(), size);
        match rx.recv_timeout(Duration::from_millis(10)).unwrap().unwrap() {
            Task::MultiBatch { multi, .. } => {
                assert_eq!(multi.len(), 1);
                assert_eq!(multi[0].len(), 1);
            }
            _ => panic!("unexpected task"),
        };

        // Stop observing cmd
        observe_info.cdc_id.stop_observing();
        observe_info.pitr_id.stop_observing();
        let mut cb = CmdBatch::new(&observe_info, 0);
        cb.push(&observe_info, 0, Cmd::default());
        <CdcObserver as CmdObserver<RocksEngine>>::on_flush_applied_cmd_batch(
            &observer,
            cb.level,
            &mut vec![cb],
            &engine,
        );
        match rx.recv_timeout(Duration::from_millis(10)) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            any => panic!("unexpected result: {:?}", any),
        };

        // Does not send unsubscribed region events.
        let mut region = Region::default();
        region.set_id(1);
        region.mut_peers().push(new_peer(2, 2));
        region.mut_peers().push(new_peer(3, 3));

        let mut ctx = ObserverContext::new(&region);
        observer.on_role_change(&mut ctx, &RoleChange::new_for_test(StateRole::Follower));
        rx.recv_timeout(Duration::from_millis(10)).unwrap_err();

        let oid = ObserveId::new();
        observer.subscribe_region(1, oid);
        let mut ctx = ObserverContext::new(&region);

        // NotLeader error should contains the new leader.
        observer.on_role_change(
            &mut ctx,
            &RoleChange {
                state: StateRole::Follower,
                leader_id: 2,
                prev_lead_transferee: raft::INVALID_ID,
                vote: raft::INVALID_ID,
                initialized: true,
                peer_id: raft::INVALID_ID,
            },
        );
        match rx.recv_timeout(Duration::from_millis(10)).unwrap().unwrap() {
            Task::Deregister(Deregister::Delegate {
                region_id,
                observe_id,
                err,
            }) => {
                assert_eq!(region_id, 1);
                assert_eq!(observe_id, oid);
                let store_err = RaftStoreError::NotLeader(region_id, Some(new_peer(2, 2)));
                match err {
                    CdcError::Request(err) => assert_eq!(*err, store_err.into()),
                    _ => panic!("unexpected err"),
                }
            }
            _ => panic!("unexpected task"),
        };

        // NotLeader error should includes leader transferee.
        observer.on_role_change(
            &mut ctx,
            &RoleChange {
                state: StateRole::Follower,
                leader_id: raft::INVALID_ID,
                prev_lead_transferee: 3,
                vote: 3,
                initialized: true,
                peer_id: raft::INVALID_ID,
            },
        );
        match rx.recv_timeout(Duration::from_millis(10)).unwrap().unwrap() {
            Task::Deregister(Deregister::Delegate {
                region_id,
                observe_id,
                err,
            }) => {
                assert_eq!(region_id, 1);
                assert_eq!(observe_id, oid);
                let store_err = RaftStoreError::NotLeader(region_id, Some(new_peer(3, 3)));
                match err {
                    CdcError::Request(err) => assert_eq!(*err, store_err.into()),
                    _ => panic!("unexpected err"),
                }
            }
            _ => panic!("unexpected task"),
        };

        // No event if it changes to leader.
        observer.on_role_change(&mut ctx, &RoleChange::new_for_test(StateRole::Leader));
        rx.recv_timeout(Duration::from_millis(10)).unwrap_err();

        // unsubscribed fail if observer id is different.
        assert_eq!(observer.unsubscribe_region(1, ObserveId::new()), None);

        // No event if it is unsubscribed.
        let oid_ = observer.unsubscribe_region(1, oid).unwrap();
        assert_eq!(oid_, oid);
        observer.on_role_change(&mut ctx, &RoleChange::new_for_test(StateRole::Follower));
        rx.recv_timeout(Duration::from_millis(10)).unwrap_err();

        // No event if it is unsubscribed.
        region.set_id(999);
        let mut ctx = ObserverContext::new(&region);
        observer.on_role_change(&mut ctx, &RoleChange::new_for_test(StateRole::Follower));
        rx.recv_timeout(Duration::from_millis(10)).unwrap_err();
    }

    #[test]
    fn test_txn_extra_dropped_since_exceed_memory_quota() {
        let memory_quota = Arc::new(MemoryQuota::new(10));
        let (task_sched, mut task_rx) = dummy_scheduler();
        let observer = CdcObserver::new(task_sched.clone(), memory_quota.clone());
        let txn_extra_scheduler =
            CdcTxnExtraScheduler::new(task_sched.clone(), memory_quota.clone());

        let observe_info = CmdObserveInfo::from_handle(
            ObserveHandle::new(),
            ObserveHandle::new(),
            ObserveHandle::new(),
        );
        let mut cb = CmdBatch::new(&observe_info, 0);
        cb.push(&observe_info, 0, Cmd::default());

        let engine = TestEngineBuilder::new().build().unwrap().get_rocksdb();
        <CdcObserver as CmdObserver<RocksEngine>>::on_flush_applied_cmd_batch(
            &observer,
            cb.level,
            &mut vec![cb],
            &engine,
        );

        txn_extra_scheduler.schedule(TxnExtra {
            old_values: Default::default(),
            one_pc: false,
            allowed_in_flashback: false,
        });

        match task_rx
            .recv_timeout(Duration::from_millis(10))
            .unwrap()
            .unwrap()
        {
            Task::MultiBatch { multi, .. } => {
                assert_eq!(multi.len(), 1);
                assert_eq!(multi[0].len(), 1);
            }
            _ => panic!("unexpected task"),
        };

        let err = task_rx.recv_timeout(Duration::from_millis(10)).unwrap_err();
        assert_eq!(err, std::sync::mpsc::RecvTimeoutError::Timeout);
    }
}
