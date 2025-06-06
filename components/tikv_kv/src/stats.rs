// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::cell::RefCell;

use engine_traits::{CF_DEFAULT, CF_LOCK, CF_WRITE, IterMetricsCollector};
use kvproto::kvrpcpb::{ScanDetail, ScanDetailV2, ScanInfo};
pub use raftstore::store::{FlowStatistics, FlowStatsReporter};

use super::metrics::{GcKeysCF, GcKeysDetail};

const STAT_PROCESSED_KEYS: &str = "processed_keys";
const STAT_GET: &str = "get";
const STAT_NEXT: &str = "next";
const STAT_PREV: &str = "prev";
const STAT_SEEK: &str = "seek";
const STAT_SEEK_FOR_PREV: &str = "seek_for_prev";
const STAT_OVER_SEEK_BOUND: &str = "over_seek_bound";
const STAT_NEXT_TOMBSTONE: &str = "next_tombstone";
const STAT_PREV_TOMBSTONE: &str = "prev_tombstone";
const STAT_SEEK_TOMBSTONE: &str = "seek_tombstone";
const STAT_SEEK_FOR_PREV_TOMBSTONE: &str = "seek_for_prev_tombstone";
/// Statistics of raw value tombstone by RawKV TTL expired or logical deleted.
const STAT_RAW_VALUE_TOMBSTONE: &str = "raw_value_tombstone";

thread_local! {
    pub static RAW_VALUE_TOMBSTONE : RefCell<usize> = const{ RefCell::new(0)};
}

pub enum StatsKind {
    Next,
    Prev,
    Seek,
    SeekForPrev,
}

pub struct StatsCollector<'a, T: IterMetricsCollector> {
    collector: T,

    stats: &'a mut CfStatistics,
    kind: StatsKind,

    internal_tombstone: usize,
    raw_value_tombstone: usize,
}

impl<'a, T: IterMetricsCollector> StatsCollector<'a, T> {
    pub fn new(collector: T, kind: StatsKind, stats: &'a mut CfStatistics) -> Self {
        let internal_tombstone = collector.internal_delete_skipped_count() as usize;
        StatsCollector {
            collector,
            stats,
            kind,
            internal_tombstone,
            raw_value_tombstone: RAW_VALUE_TOMBSTONE.with(|m| *m.borrow()),
        }
    }
}

impl<T: IterMetricsCollector> Drop for StatsCollector<'_, T> {
    fn drop(&mut self) {
        self.stats.raw_value_tombstone +=
            RAW_VALUE_TOMBSTONE.with(|m| *m.borrow()) - self.raw_value_tombstone;
        let internal_tombstone =
            self.collector.internal_delete_skipped_count() as usize - self.internal_tombstone;
        match self.kind {
            StatsKind::Next => {
                self.stats.next += 1;
                self.stats.next_tombstone += internal_tombstone;
            }
            StatsKind::Prev => {
                self.stats.prev += 1;
                self.stats.prev_tombstone += internal_tombstone;
            }
            StatsKind::Seek => {
                self.stats.seek += 1;
                self.stats.seek_tombstone += internal_tombstone;
            }
            StatsKind::SeekForPrev => {
                self.stats.seek_for_prev += 1;
                self.stats.seek_for_prev_tombstone += internal_tombstone;
            }
        }
    }
}

/// Statistics collects the ops taken when fetching data.
#[derive(Default, Clone, Debug)]
pub struct CfStatistics {
    // How many keys that's visible to user
    pub processed_keys: usize,

    pub get: usize,
    pub next: usize,
    pub prev: usize,
    pub seek: usize,
    pub seek_for_prev: usize,
    pub over_seek_bound: usize,

    pub flow_stats: FlowStatistics,

    pub next_tombstone: usize,
    pub prev_tombstone: usize,
    pub seek_tombstone: usize,
    pub seek_for_prev_tombstone: usize,
    pub raw_value_tombstone: usize,
}

const STATS_COUNT: usize = 12;

impl CfStatistics {
    #[inline]
    pub fn total_op_count(&self) -> usize {
        self.get + self.next + self.prev + self.seek + self.seek_for_prev
    }

    pub fn details(&self) -> [(&'static str, usize); STATS_COUNT] {
        [
            (STAT_PROCESSED_KEYS, self.processed_keys),
            (STAT_GET, self.get),
            (STAT_NEXT, self.next),
            (STAT_PREV, self.prev),
            (STAT_SEEK, self.seek),
            (STAT_SEEK_FOR_PREV, self.seek_for_prev),
            (STAT_OVER_SEEK_BOUND, self.over_seek_bound),
            (STAT_NEXT_TOMBSTONE, self.next_tombstone),
            (STAT_PREV_TOMBSTONE, self.prev_tombstone),
            (STAT_SEEK_TOMBSTONE, self.seek_tombstone),
            (STAT_SEEK_FOR_PREV_TOMBSTONE, self.seek_for_prev_tombstone),
            (STAT_RAW_VALUE_TOMBSTONE, self.raw_value_tombstone),
        ]
    }

    pub fn details_enum(&self) -> [(GcKeysDetail, usize); STATS_COUNT] {
        [
            (GcKeysDetail::processed_keys, self.processed_keys),
            (GcKeysDetail::get, self.get),
            (GcKeysDetail::next, self.next),
            (GcKeysDetail::prev, self.prev),
            (GcKeysDetail::seek, self.seek),
            (GcKeysDetail::seek_for_prev, self.seek_for_prev),
            (GcKeysDetail::over_seek_bound, self.over_seek_bound),
            (GcKeysDetail::next_tombstone, self.next_tombstone),
            (GcKeysDetail::prev_tombstone, self.prev_tombstone),
            (GcKeysDetail::seek_tombstone, self.seek_tombstone),
            (
                GcKeysDetail::seek_for_prev_tombstone,
                self.seek_for_prev_tombstone,
            ),
            (GcKeysDetail::raw_value_tombstone, self.raw_value_tombstone),
        ]
    }

    pub fn add(&mut self, other: &Self) {
        self.processed_keys = self.processed_keys.saturating_add(other.processed_keys);
        self.get = self.get.saturating_add(other.get);
        self.next = self.next.saturating_add(other.next);
        self.prev = self.prev.saturating_add(other.prev);
        self.seek = self.seek.saturating_add(other.seek);
        self.seek_for_prev = self.seek_for_prev.saturating_add(other.seek_for_prev);
        self.over_seek_bound = self.over_seek_bound.saturating_add(other.over_seek_bound);
        self.flow_stats.add(&other.flow_stats);
        self.next_tombstone = self.next_tombstone.saturating_add(other.next_tombstone);
        self.prev_tombstone = self.prev_tombstone.saturating_add(other.prev_tombstone);
        self.seek_tombstone = self.seek_tombstone.saturating_add(other.seek_tombstone);
        self.seek_for_prev_tombstone = self
            .seek_for_prev_tombstone
            .saturating_add(other.seek_for_prev_tombstone);
        self.raw_value_tombstone = self
            .raw_value_tombstone
            .saturating_add(other.raw_value_tombstone);
    }

    /// Deprecated
    pub fn scan_info(&self) -> ScanInfo {
        let mut info = ScanInfo::default();
        info.set_processed(self.processed_keys as i64);
        info.set_total(self.total_op_count() as i64);
        info
    }
}

#[derive(Default, Debug)]
pub struct Statistics {
    pub lock: CfStatistics,
    pub write: CfStatistics,
    pub data: CfStatistics,

    // Number of bytes of user key-value pairs.
    //
    // A user key in mem-comparable format doesn't contain timestamp but some markers and
    // paddings, so its size is still a little bit greater than the one at client view.
    //
    // Note that a value comes from either write cf (due to it's a short value) or default cf, we
    // can't embed this `processed_size` field into `CfStatistics`.
    pub processed_size: usize,

    // When getting data from default cf, we can check write cf statistics to decide which method
    // should be used to get the data.
    load_data_hint: LoadDataHintStatistics,
}

#[derive(Default, Debug)]
struct LoadDataHintStatistics {
    // The value of `over_seek_bound` when the last time calling `load_data_hint()`.
    last_write_over_seek_bound: usize,
}

#[derive(Default, PartialEq, Debug, Clone)]
pub enum LoadDataHint {
    #[default]
    NearSeek,
    Seek,
}

impl Statistics {
    // Use write cf stats to decide load action for default cf
    pub fn load_data_hint(&mut self) -> LoadDataHint {
        let stats = &mut self.load_data_hint;

        let hint = if self.write.over_seek_bound != stats.last_write_over_seek_bound {
            // Over seek bound indicates the next valid key may be far away from current
            // position, so use seek directly
            LoadDataHint::Seek
        } else {
            // The next valid key may be around current position, so use near seek which
            // calls next() multiple times before calling seek()
            LoadDataHint::NearSeek
        };
        stats.last_write_over_seek_bound = self.write.over_seek_bound;
        hint
    }

    pub fn details(&self) -> [(&'static str, [(&'static str, usize); STATS_COUNT]); 3] {
        [
            (CF_DEFAULT, self.data.details()),
            (CF_LOCK, self.lock.details()),
            (CF_WRITE, self.write.details()),
        ]
    }

    pub fn details_enum(&self) -> [(GcKeysCF, [(GcKeysDetail, usize); STATS_COUNT]); 3] {
        [
            (GcKeysCF::default, self.data.details_enum()),
            (GcKeysCF::lock, self.lock.details_enum()),
            (GcKeysCF::write, self.write.details_enum()),
        ]
    }

    pub fn add(&mut self, other: &Self) {
        self.lock.add(&other.lock);
        self.write.add(&other.write);
        self.data.add(&other.data);
        self.processed_size += other.processed_size;
    }

    /// Deprecated
    pub fn scan_detail(&self) -> ScanDetail {
        let mut detail = ScanDetail::default();
        detail.set_data(self.data.scan_info());
        detail.set_lock(self.lock.scan_info());
        detail.set_write(self.write.scan_info());
        detail
    }

    pub fn mut_cf_statistics(&mut self, cf: &str) -> &mut CfStatistics {
        if cf.is_empty() {
            return &mut self.data;
        }
        match cf {
            CF_DEFAULT => &mut self.data,
            CF_LOCK => &mut self.lock,
            CF_WRITE => &mut self.write,
            _ => unreachable!(),
        }
    }

    pub fn cf_statistics(&self, cf: &str) -> &CfStatistics {
        if cf.is_empty() {
            return &self.data;
        }
        match cf {
            CF_DEFAULT => &self.data,
            CF_LOCK => &self.lock,
            CF_WRITE => &self.write,
            _ => unreachable!(),
        }
    }

    pub fn write_scan_detail(&self, detail_v2: &mut ScanDetailV2) {
        detail_v2.set_processed_versions(self.write.processed_keys as u64);
        detail_v2.set_total_versions(self.write.total_op_count() as u64);
        detail_v2.set_processed_versions_size(self.processed_size as u64);
    }
}

#[derive(Default, Debug)]
pub struct StatisticsSummary {
    pub stat: Statistics,
    pub count: u64,
}

impl StatisticsSummary {
    pub fn add_statistics(&mut self, v: &Statistics) {
        self.stat.add(v);
        self.count += 1;
    }
}

/// Latency indicators for multi-execution-stages.
///
/// The detailed meaning of the indicators is as follows:
///
/// ```text
/// ------> Begin ------> Scheduled ------> SnapshotReceived ------> Finished ------>
/// |----- schedule_wait_time -----|
///                                |-- snapshot_wait_time --|
/// |------------------- wait_wall_time --------------------|
///                                                         |-- process_wall_time --|
/// |------------------------------ kv_read_wall_time ------------------------------|
/// ```
#[derive(Debug, Default, Copy, Clone)]
pub struct StageLatencyStats {
    pub schedule_wait_time_ns: u64,
    pub snapshot_wait_time_ns: u64,
    pub wait_wall_time_ns: u64,
    pub process_wall_time_ns: u64,
}
