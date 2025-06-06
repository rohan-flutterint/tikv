// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

//! Functionality related to compaction

use std::collections::BTreeMap;

use crate::{CfNamesExt, errors::Result};

#[derive(Clone, Debug)]
pub struct ManualCompactionOptions {
    pub exclusive_manual: bool,
    pub max_subcompactions: u32,
    pub bottommost_level_force: bool,
}

impl ManualCompactionOptions {
    pub fn new(
        exclusive_manual: bool,
        max_subcompactions: u32,
        bottommost_level_force: bool,
    ) -> Self {
        Self {
            exclusive_manual,
            max_subcompactions,
            bottommost_level_force,
        }
    }
}

pub trait CompactExt: CfNamesExt {
    type CompactedEvent: CompactedEvent;

    /// Checks whether any column family sets `disable_auto_compactions` to
    /// `True` or not.
    fn auto_compactions_is_disabled(&self) -> Result<bool>;

    fn compact_range(
        &self,
        start_key: Option<&[u8]>,
        end_key: Option<&[u8]>,
        compaction_option: ManualCompactionOptions,
    ) -> Result<()> {
        for cf in self.cf_names() {
            self.compact_range_cf(cf, start_key, end_key, compaction_option.clone())?;
        }
        Ok(())
    }

    /// Compacts the column families in the specified range by manual or not.
    fn compact_range_cf(
        &self,
        cf: &str,
        start_key: Option<&[u8]>,
        end_key: Option<&[u8]>,
        compaction_option: ManualCompactionOptions,
    ) -> Result<()>;

    /// Compacts files in the range and above the output level.
    /// Compacts all files if the range is not specified.
    /// Compacts all files to the bottommost level if the output level is not
    /// specified.
    fn compact_files_in_range(
        &self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
        output_level: Option<i32>,
    ) -> Result<()> {
        for cf in self.cf_names() {
            self.compact_files_in_range_cf(cf, start, end, output_level)?;
        }
        Ok(())
    }

    /// Compacts files in the range and above the output level of the given
    /// column family. Compacts all files to the bottommost level if the
    /// output level is not specified.
    fn compact_files_in_range_cf(
        &self,
        cf: &str,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
        output_level: Option<i32>,
    ) -> Result<()>;

    fn compact_files_cf(
        &self,
        cf: &str,
        files: Vec<String>,
        output_level: Option<i32>,
        max_subcompactions: u32,
        exclude_l0: bool,
    ) -> Result<()>;

    // Check all data is in the range [start, end).
    fn check_in_range(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<()>;
}

pub trait CompactedEvent: Send {
    fn total_bytes_declined(&self) -> u64;

    fn is_size_declining_trivial(&self, split_check_diff: u64) -> bool;

    fn output_level_label(&self) -> String;

    /// This takes self by value so that engine_rocks can move keys out of the
    /// CompactedEvent
    fn calc_ranges_declined_bytes(
        self,
        ranges: &BTreeMap<Vec<u8>, u64>,
        bytes_threshold: u64,
    ) -> Vec<(u64, u64)>;

    fn cf(&self) -> &str;
}
