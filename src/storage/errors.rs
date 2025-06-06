// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

//! Types for storage related errors and associated helper methods.
use std::{
    convert::TryFrom,
    error::Error as StdError,
    fmt::{self, Debug, Display, Formatter},
    io::Error as IoError,
    sync::Arc,
};

use error_code::{self, ErrorCode, ErrorCodeExt};
use kvproto::{errorpb, kvrpcpb, kvrpcpb::ApiVersion};
use thiserror::Error;
use tikv_util::deadline::{DeadlineError, set_deadline_exceeded_busy_error};
use txn_types::{KvPair, TimeStamp};

use crate::storage::{
    CommandKind, Result,
    kv::{self, Error as KvError, ErrorInner as KvErrorInner},
    mvcc::{Error as MvccError, ErrorInner as MvccErrorInner},
    txn::{self, Error as TxnError, ErrorInner as TxnErrorInner},
    types,
};

#[derive(Debug, Error)]
/// Detailed errors for storage operations. This enum also unifies code for
/// basic error handling functionality in a single place instead of being spread
/// out.
pub enum ErrorInner {
    #[error("{0}")]
    Kv(#[from] kv::Error),

    #[error("{0}")]
    Txn(#[from] txn::Error),

    #[error("{0}")]
    Engine(#[from] engine_traits::Error),

    #[error("storage is closed.")]
    Closed,

    #[error("{0}")]
    Other(#[from] Box<dyn StdError + Send + Sync>),

    #[error("{0}")]
    Io(#[from] IoError),

    #[error("scheduler is too busy")]
    SchedTooBusy,

    #[error("gc worker is too busy")]
    GcWorkerTooBusy,

    #[error("max key size exceeded, size: {}, limit: {}", .size, .limit)]
    KeyTooLarge { size: usize, limit: usize },

    #[error("invalid cf name: {0}")]
    InvalidCf(String),

    #[error("cf is deprecated in API V2, cf name: {0}")]
    CfDeprecated(String),

    #[error("ttl is not enabled, but get put request with ttl")]
    TtlNotEnabled,

    #[error("Deadline is exceeded")]
    DeadlineExceeded,

    #[error("The length of ttls does not equal to the length of pairs")]
    TtlLenNotEqualsToPairs,

    #[error("Api version in request does not match with TiKV storage, cmd: {:?}, storage: {:?}, request: {:?}", .cmd, .storage_api_version, .req_api_version)]
    ApiVersionNotMatched {
        cmd: CommandKind,
        storage_api_version: ApiVersion,
        req_api_version: ApiVersion,
    },

    #[error("Key mode mismatched with the request mode, cmd: {:?}, storage: {:?}, key: {}", .cmd, .storage_api_version, .key)]
    InvalidKeyMode {
        cmd: CommandKind,
        storage_api_version: ApiVersion,
        key: String,
    },

    #[error("Key mode mismatched with the request mode, cmd: {:?}, storage: {:?}, range: {:?}", .cmd, .storage_api_version, .range)]
    InvalidKeyRangeMode {
        cmd: CommandKind,
        storage_api_version: ApiVersion,
        range: (Option<String>, Option<String>),
    },
}

impl ErrorInner {
    pub fn invalid_key_mode(cmd: CommandKind, storage_api_version: ApiVersion, key: &[u8]) -> Self {
        ErrorInner::InvalidKeyMode {
            cmd,
            storage_api_version,
            key: log_wrappers::hex_encode_upper(key),
        }
    }

    pub fn invalid_key_range_mode(
        cmd: CommandKind,
        storage_api_version: ApiVersion,
        range: (Option<&[u8]>, Option<&[u8]>),
    ) -> Self {
        ErrorInner::InvalidKeyRangeMode {
            cmd,
            storage_api_version,
            range: (
                range.0.map(log_wrappers::hex_encode_upper),
                range.1.map(log_wrappers::hex_encode_upper),
            ),
        }
    }
}

impl From<DeadlineError> for ErrorInner {
    fn from(_: DeadlineError) -> Self {
        ErrorInner::DeadlineExceeded
    }
}

/// Errors for storage module. Wrapper type of `ErrorInner`.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] pub Box<ErrorInner>);

impl From<ErrorInner> for Error {
    #[inline]
    fn from(e: ErrorInner) -> Self {
        Error(Box::new(e))
    }
}

impl<T: Into<ErrorInner>> From<T> for Error {
    #[inline]
    default fn from(err: T) -> Self {
        let err = err.into();
        err.into()
    }
}

impl ErrorCodeExt for Error {
    fn error_code(&self) -> ErrorCode {
        match self.0.as_ref() {
            ErrorInner::Kv(e) => e.error_code(),
            ErrorInner::Txn(e) => e.error_code(),
            ErrorInner::Engine(e) => e.error_code(),
            ErrorInner::Closed => error_code::storage::CLOSED,
            ErrorInner::Other(_) => error_code::storage::UNKNOWN,
            ErrorInner::Io(_) => error_code::storage::IO,
            ErrorInner::SchedTooBusy => error_code::storage::SCHED_TOO_BUSY,
            ErrorInner::GcWorkerTooBusy => error_code::storage::GC_WORKER_TOO_BUSY,
            ErrorInner::KeyTooLarge { .. } => error_code::storage::KEY_TOO_LARGE,
            ErrorInner::InvalidCf(_) => error_code::storage::INVALID_CF,
            ErrorInner::CfDeprecated(_) => error_code::storage::CF_DEPRECATED,
            ErrorInner::TtlNotEnabled => error_code::storage::TTL_NOT_ENABLED,
            ErrorInner::DeadlineExceeded => error_code::storage::DEADLINE_EXCEEDED,
            ErrorInner::TtlLenNotEqualsToPairs => error_code::storage::TTL_LEN_NOT_EQUALS_TO_PAIRS,
            ErrorInner::ApiVersionNotMatched { .. } => error_code::storage::API_VERSION_NOT_MATCHED,
            ErrorInner::InvalidKeyMode { .. } => error_code::storage::INVALID_KEY_MODE,
            ErrorInner::InvalidKeyRangeMode { .. } => error_code::storage::INVALID_KEY_MODE,
        }
    }
}

/// Tags of errors for storage module.
pub enum ErrorHeaderKind {
    NotLeader,
    RegionNotFound,
    KeyNotInRegion,
    EpochNotMatch,
    ServerIsBusy,
    StaleCommand,
    StoreNotMatch,
    RaftEntryTooLarge,
    ReadIndexNotReady,
    ProposalInMergeMode,
    DataNotReady,
    RegionNotInitialized,
    DiskFull,
    RecoveryInProgress,
    FlashbackInProgress,
    BucketsVersionNotMatch,
    Other,
}

impl ErrorHeaderKind {
    /// TODO: This function is only used for bridging existing & legacy metric
    /// tags. It should be removed once Coprocessor starts using new static
    /// metrics.
    pub fn get_str(&self) -> &'static str {
        match *self {
            ErrorHeaderKind::NotLeader => "not_leader",
            ErrorHeaderKind::RegionNotFound => "region_not_found",
            ErrorHeaderKind::KeyNotInRegion => "key_not_in_region",
            ErrorHeaderKind::EpochNotMatch => "epoch_not_match",
            ErrorHeaderKind::ServerIsBusy => "server_is_busy",
            ErrorHeaderKind::StaleCommand => "stale_command",
            ErrorHeaderKind::StoreNotMatch => "store_not_match",
            ErrorHeaderKind::RaftEntryTooLarge => "raft_entry_too_large",
            ErrorHeaderKind::ReadIndexNotReady => "read_index_not_ready",
            ErrorHeaderKind::ProposalInMergeMode => "proposal_in_merge_mode",
            ErrorHeaderKind::DataNotReady => "data_not_ready",
            ErrorHeaderKind::RegionNotInitialized => "region_not_initialized",
            ErrorHeaderKind::DiskFull => "disk_full",
            ErrorHeaderKind::RecoveryInProgress => "recovery_in_progress",
            ErrorHeaderKind::FlashbackInProgress => "flashback_in_progress",
            ErrorHeaderKind::BucketsVersionNotMatch => "buckets_version_not_match",
            ErrorHeaderKind::Other => "other",
        }
    }
}

impl Display for ErrorHeaderKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get_str())
    }
}

const SCHEDULER_IS_BUSY: &str = "scheduler is busy";
const GC_WORKER_IS_BUSY: &str = "gc worker is busy";

/// Get the `ErrorHeaderKind` enum that corresponds to the error in the protobuf
/// message. Returns `ErrorHeaderKind::Other` if no match found.
pub fn get_error_kind_from_header(header: &errorpb::Error) -> ErrorHeaderKind {
    if header.has_not_leader() {
        ErrorHeaderKind::NotLeader
    } else if header.has_region_not_found() {
        ErrorHeaderKind::RegionNotFound
    } else if header.has_key_not_in_region() {
        ErrorHeaderKind::KeyNotInRegion
    } else if header.has_epoch_not_match() {
        ErrorHeaderKind::EpochNotMatch
    } else if header.has_server_is_busy() {
        ErrorHeaderKind::ServerIsBusy
    } else if header.has_stale_command() {
        ErrorHeaderKind::StaleCommand
    } else if header.has_store_not_match() {
        ErrorHeaderKind::StoreNotMatch
    } else if header.has_raft_entry_too_large() {
        ErrorHeaderKind::RaftEntryTooLarge
    } else if header.has_read_index_not_ready() {
        ErrorHeaderKind::ReadIndexNotReady
    } else if header.has_proposal_in_merging_mode() {
        ErrorHeaderKind::ProposalInMergeMode
    } else if header.has_data_is_not_ready() {
        ErrorHeaderKind::DataNotReady
    } else if header.has_region_not_initialized() {
        ErrorHeaderKind::RegionNotInitialized
    } else if header.has_disk_full() {
        ErrorHeaderKind::DiskFull
    } else if header.has_recovery_in_progress() {
        ErrorHeaderKind::RecoveryInProgress
    } else if header.has_flashback_in_progress() {
        ErrorHeaderKind::FlashbackInProgress
    } else if header.has_bucket_version_not_match() {
        ErrorHeaderKind::BucketsVersionNotMatch
    } else {
        ErrorHeaderKind::Other
    }
}

/// Get the metric tag of the error in the protobuf message.
/// Returns "other" if no match found.
pub fn get_tag_from_header(header: &errorpb::Error) -> &'static str {
    get_error_kind_from_header(header).get_str()
}

pub fn extract_region_error_from_error(e: &Error) -> Option<errorpb::Error> {
    match e {
        // TODO: use `Error::cause` instead.
        Error(box ErrorInner::Kv(KvError(box KvErrorInner::Request(ref e))))
        | Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Engine(KvError(
            box KvErrorInner::Request(ref e),
        )))))
        | Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Mvcc(MvccError(
            box MvccErrorInner::Kv(KvError(box KvErrorInner::Request(ref e))),
        ))))) => Some(e.to_owned()),
        Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::MaxTimestampNotSynced {
            ..
        }))) => {
            let mut err = errorpb::Error::default();
            err.set_max_timestamp_not_synced(Default::default());
            Some(err)
        }
        Error(box ErrorInner::Txn(
            e @ TxnError(box TxnErrorInner::RawKvMaxTimestampNotSynced { .. }),
        )) => {
            let mut err = errorpb::Error::default();
            err.set_max_timestamp_not_synced(Default::default());
            err.set_message(format!("{}", e));
            Some(err)
        }
        Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::FlashbackNotPrepared(
            region_id,
        )))) => {
            let mut err = errorpb::Error::default();
            let mut flashback_not_prepared_err = errorpb::FlashbackNotPrepared::default();
            flashback_not_prepared_err.set_region_id(*region_id);
            err.set_flashback_not_prepared(flashback_not_prepared_err);
            Some(err)
        }
        Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::InvalidMaxTsUpdate(
            invalid_max_ts_update,
        )))) => {
            let mut err = errorpb::Error::default();
            err.set_message(invalid_max_ts_update.to_string());
            Some(err)
        }
        Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Mvcc(MvccError(
            box MvccErrorInner::InvalidMaxTsUpdate(invalid_max_ts_update),
        ))))) => {
            let mut err = errorpb::Error::default();
            err.set_message(invalid_max_ts_update.to_string());
            Some(err)
        }
        Error(box ErrorInner::SchedTooBusy) => {
            let mut err = errorpb::Error::default();
            let mut server_is_busy_err = errorpb::ServerIsBusy::default();
            server_is_busy_err.set_reason(SCHEDULER_IS_BUSY.to_owned());
            err.set_server_is_busy(server_is_busy_err);
            Some(err)
        }
        Error(box ErrorInner::GcWorkerTooBusy) => {
            let mut err = errorpb::Error::default();
            let mut server_is_busy_err = errorpb::ServerIsBusy::default();
            server_is_busy_err.set_reason(GC_WORKER_IS_BUSY.to_owned());
            err.set_server_is_busy(server_is_busy_err);
            Some(err)
        }
        Error(box ErrorInner::Closed) => {
            // TiKV is closing, return an RegionError to tell the client that this region is
            // unavailable temporarily, the client should retry the request in other TiKVs.
            let mut err = errorpb::Error::default();
            err.set_message("TiKV is Closing".to_string());
            Some(err)
        }
        Error(box ErrorInner::DeadlineExceeded) => {
            let mut err = errorpb::Error::default();
            err.set_message(e.to_string());
            set_deadline_exceeded_busy_error(&mut err);
            Some(err)
        }
        _ => None,
    }
}

pub fn extract_region_error<T>(res: &Result<T>) -> Option<errorpb::Error> {
    match res {
        Ok(_) => None,
        Err(e) => extract_region_error_from_error(e),
    }
}

pub fn extract_committed(err: &Error) -> Option<TimeStamp> {
    match *err {
        Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Mvcc(MvccError(
            box MvccErrorInner::Committed { commit_ts, .. },
        ))))) => Some(commit_ts),
        _ => None,
    }
}

fn get_or_insert_default_for_key_error_debug_info(
    err: &mut kvrpcpb::KeyError,
) -> &mut kvrpcpb::DebugInfo {
    let debug_info = &mut err.debug_info;
    if debug_info.is_none() {
        debug_info.set_default()
    } else {
        debug_info.as_mut().unwrap()
    }
}

fn add_debug_mvcc_for_key_error(
    err: &mut kvrpcpb::KeyError,
    key: &[u8],
    mvcc_info: Option<types::MvccInfo>,
) {
    if let Some(mut mvcc) = mvcc_info {
        let debug_info = get_or_insert_default_for_key_error_debug_info(err);
        // remove the values in default CF to reduce the size of the response.
        mvcc.values.clear();
        // set mvcc info to debug_info
        let mut mvcc_debug_info = kvrpcpb::MvccDebugInfo::default();
        mvcc_debug_info.set_key(key.to_owned());
        mvcc_debug_info.set_mvcc(mvcc.into_proto());
        debug_info.mvcc_info.push(mvcc_debug_info);
    }
}

pub fn extract_key_error(err: &Error) -> kvrpcpb::KeyError {
    let mut key_error = kvrpcpb::KeyError::default();
    match err {
        Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Mvcc(MvccError(
            box MvccErrorInner::KeyIsLocked(info),
        )))))
        | Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Engine(KvError(
            box KvErrorInner::KeyIsLocked(info),
        )))))
        | Error(box ErrorInner::Kv(KvError(box KvErrorInner::KeyIsLocked(info)))) => {
            key_error.set_locked(info.clone());
        }
        // failed in prewrite or pessimistic lock
        Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Mvcc(MvccError(
            box MvccErrorInner::WriteConflict {
                start_ts,
                conflict_start_ts,
                conflict_commit_ts,
                key,
                primary,
                reason,
            },
        ))))) => {
            let mut write_conflict = kvrpcpb::WriteConflict::default();
            write_conflict.set_start_ts(start_ts.into_inner());
            write_conflict.set_conflict_ts(conflict_start_ts.into_inner());
            write_conflict.set_conflict_commit_ts(conflict_commit_ts.into_inner());
            write_conflict.set_key(key.to_owned());
            write_conflict.set_primary(primary.to_owned());
            write_conflict.set_reason(reason.to_owned());
            key_error.set_conflict(write_conflict);
            // for compatibility with older versions.
            key_error.set_retryable(format!("{:?}", err));
        }
        Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Mvcc(MvccError(
            box MvccErrorInner::AlreadyExist { key, .. },
        ))))) => {
            let mut exist = kvrpcpb::AlreadyExist::default();
            exist.set_key(key.clone());
            key_error.set_already_exist(exist);
        }
        // failed in commit
        Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Mvcc(MvccError(
            box MvccErrorInner::TxnLockNotFound {
                start_ts,
                commit_ts,
                key,
                mvcc_info,
            },
        ))))) => {
            // use an error without mvcc_info to construct error the error message
            let err_without_mvcc = &Error::from(TxnError::from(MvccError::from(
                MvccErrorInner::TxnLockNotFound {
                    start_ts: *start_ts,
                    commit_ts: *commit_ts,
                    key: key.clone(),
                    mvcc_info: None,
                },
            )));

            warn!("txn conflicts"; "err" => ?err_without_mvcc);
            key_error.set_retryable(format!("{:?}", err_without_mvcc));
            let mut txn_lock_not_found = kvrpcpb::TxnLockNotFound::default();
            txn_lock_not_found.set_key(key.clone());
            key_error.set_txn_lock_not_found(txn_lock_not_found);
            add_debug_mvcc_for_key_error(&mut key_error, key, mvcc_info.clone());
        }
        Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Mvcc(MvccError(
            box MvccErrorInner::TxnNotFound { start_ts, key },
        ))))) => {
            let mut txn_not_found = kvrpcpb::TxnNotFound::default();
            txn_not_found.set_start_ts(start_ts.into_inner());
            txn_not_found.set_primary_key(key.to_owned());
            key_error.set_txn_not_found(txn_not_found);
        }
        Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Mvcc(MvccError(
            box MvccErrorInner::Deadlock {
                lock_ts,
                lock_key,
                deadlock_key_hash,
                wait_chain,
                ..
            },
        ))))) => {
            warn!("txn deadlocks"; "err" => ?err);
            let mut deadlock = kvrpcpb::Deadlock::default();
            deadlock.set_lock_ts(lock_ts.into_inner());
            deadlock.set_lock_key(lock_key.to_owned());
            deadlock.set_deadlock_key_hash(*deadlock_key_hash);
            deadlock.set_wait_chain(wait_chain.clone().into());
            key_error.set_deadlock(deadlock);
        }
        Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Mvcc(MvccError(
            box MvccErrorInner::CommitTsExpired {
                start_ts,
                commit_ts,
                key,
                min_commit_ts,
                mvcc_info,
            },
        ))))) => {
            let mut commit_ts_expired = kvrpcpb::CommitTsExpired::default();
            commit_ts_expired.set_start_ts(start_ts.into_inner());
            commit_ts_expired.set_attempted_commit_ts(commit_ts.into_inner());
            commit_ts_expired.set_key(key.to_owned());
            commit_ts_expired.set_min_commit_ts(min_commit_ts.into_inner());
            key_error.set_commit_ts_expired(commit_ts_expired);
            add_debug_mvcc_for_key_error(&mut key_error, key, mvcc_info.clone());
        }
        Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Mvcc(MvccError(
            box MvccErrorInner::CommitTsTooLarge { min_commit_ts, .. },
        ))))) => {
            let mut commit_ts_too_large = kvrpcpb::CommitTsTooLarge::default();
            commit_ts_too_large.set_commit_ts(min_commit_ts.into_inner());
            key_error.set_commit_ts_too_large(commit_ts_too_large);
        }
        Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Mvcc(MvccError(
            box MvccErrorInner::AssertionFailed {
                start_ts,
                key,
                assertion,
                existing_start_ts,
                existing_commit_ts,
            },
        ))))) => {
            let mut assertion_failed = kvrpcpb::AssertionFailed::default();
            assertion_failed.set_start_ts(start_ts.into_inner());
            assertion_failed.set_key(key.to_owned());
            assertion_failed.set_assertion(*assertion);
            assertion_failed.set_existing_start_ts(existing_start_ts.into_inner());
            assertion_failed.set_existing_commit_ts(existing_commit_ts.into_inner());
            key_error.set_assertion_failed(assertion_failed);
        }
        Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Mvcc(MvccError(
            box MvccErrorInner::PrimaryMismatch(lock_info),
        ))))) => {
            let mut primary_mismatch = kvrpcpb::PrimaryMismatch::default();
            primary_mismatch.set_lock_info(lock_info.clone());
            key_error.set_primary_mismatch(primary_mismatch);
        }
        _ => {
            error!(?*err; "txn aborts");
            key_error.set_abort(format!("{:?}", err));
        }
    }
    key_error
}

pub fn extract_kv_pairs(res: Result<Vec<Result<KvPair>>>) -> Vec<kvrpcpb::KvPair> {
    match res {
        Ok(res) => map_kv_pairs(res),
        Err(e) => {
            let mut pair = kvrpcpb::KvPair::default();
            pair.set_error(extract_key_error(&e));
            vec![pair]
        }
    }
}

pub fn map_kv_pairs(r: Vec<Result<KvPair>>) -> Vec<kvrpcpb::KvPair> {
    r.into_iter()
        .map(|r| match r {
            Ok((key, value)) => {
                let mut pair = kvrpcpb::KvPair::default();
                pair.set_key(key);
                pair.set_value(value);
                pair
            }
            Err(e) => {
                let mut pair = kvrpcpb::KvPair::default();
                pair.set_error(extract_key_error(&e));
                pair
            }
        })
        .collect()
}

pub fn extract_key_errors(res: Result<Vec<Result<()>>>) -> Vec<kvrpcpb::KeyError> {
    match res {
        Ok(res) => res
            .into_iter()
            .filter_map(|x| match x {
                Err(e) => Some(extract_key_error(&e)),
                Ok(_) => None,
            })
            .collect(),
        Err(e) => vec![extract_key_error(&e)],
    }
}

/// The shared version of [`Error`]. In some cases, it's necessary to pass a
/// single error to more than one requests, since the inner error doesn't
/// support cloning.
#[derive(Debug, Clone, Error)]
#[error(transparent)]
pub struct SharedError(pub Arc<Error>);

impl SharedError {
    pub fn inner(&self) -> &ErrorInner {
        &self.0.0
    }
}

impl From<ErrorInner> for SharedError {
    fn from(e: ErrorInner) -> Self {
        Self(Arc::new(Error::from(e)))
    }
}

impl From<Error> for SharedError {
    fn from(e: Error) -> Self {
        Self(Arc::new(e))
    }
}

/// Tries to convert the shared error to owned one. It can success only when
/// it's the only reference to the error.
impl TryFrom<SharedError> for Error {
    type Error = ();

    fn try_from(e: SharedError) -> std::result::Result<Self, Self::Error> {
        Arc::try_unwrap(e.0).map_err(|_| ())
    }
}

#[cfg(test)]
mod test {
    use kvproto::kvrpcpb::WriteConflictReason;
    use txn_types::{Lock, LockType, Write, WriteType};

    use super::*;
    use crate::storage::types::MvccInfo;

    #[test]
    fn test_extract_key_error_write_conflict() {
        let start_ts = 110.into();
        let conflict_start_ts = 108.into();
        let conflict_commit_ts = 109.into();
        let key = b"key".to_vec();
        let primary = b"primary".to_vec();
        let case = Error::from(TxnError::from(MvccError::from(
            MvccErrorInner::WriteConflict {
                start_ts,
                conflict_start_ts,
                conflict_commit_ts,
                key: key.clone(),
                primary: primary.clone(),
                reason: WriteConflictReason::LazyUniquenessCheck,
            },
        )));
        let mut expect = kvrpcpb::KeyError::default();
        let mut write_conflict = kvrpcpb::WriteConflict::default();
        write_conflict.set_start_ts(start_ts.into_inner());
        write_conflict.set_conflict_ts(conflict_start_ts.into_inner());
        write_conflict.set_conflict_commit_ts(conflict_commit_ts.into_inner());
        write_conflict.set_key(key);
        write_conflict.set_primary(primary);
        write_conflict.set_reason(WriteConflictReason::LazyUniquenessCheck);
        expect.set_conflict(write_conflict);
        expect.set_retryable(format!("{:?}", case));

        let got = extract_key_error(&case);
        assert_eq!(got, expect);
    }

    fn mock_mvcc_info() -> MvccInfo {
        MvccInfo {
            lock: Some(Lock::new(
                LockType::Lock,
                b"k".to_vec(),
                10.into(),
                100,
                None,
                10.into(),
                1,
                10.into(),
                false,
            )),
            writes: vec![(
                TimeStamp::new(8),
                Write::new(WriteType::Lock, 7.into(), None),
            )],
            values: vec![(TimeStamp::new(7), b"v".to_vec())],
        }
    }

    fn expected_debug_info_from_mvcc(key: Vec<u8>, mvcc: MvccInfo) -> kvrpcpb::DebugInfo {
        let mut expect_pb_mvcc_info = mvcc.clone().into_proto();
        // should clear the values in default CF to reduce the size of the response.
        expect_pb_mvcc_info.values.clear();
        kvrpcpb::DebugInfo {
            mvcc_info: vec![kvrpcpb::MvccDebugInfo {
                key,
                mvcc: Some(expect_pb_mvcc_info).into(),
                ..Default::default()
            }]
            .into(),
            ..Default::default()
        }
    }

    #[test]
    fn test_extract_key_error_txn_lock_not_found() {
        fn mock_txn_lock_not_found_err(has_mvcc: bool) -> kvrpcpb::KeyError {
            extract_key_error(&Error::from(TxnError::from(MvccError::from(
                MvccErrorInner::TxnLockNotFound {
                    start_ts: TimeStamp::new(123),
                    commit_ts: TimeStamp::new(456),
                    key: b"key".to_vec(),
                    mvcc_info: has_mvcc.then(|| mock_mvcc_info()),
                },
            ))))
        }

        let key = b"key".to_vec();
        let mut expect = kvrpcpb::KeyError::default();
        let mut txn_lock_not_found = kvrpcpb::TxnLockNotFound::default();
        txn_lock_not_found.set_key(key.clone());
        expect.set_txn_lock_not_found(txn_lock_not_found);
        let expected_retryable_msg = format!(
            "{:?}",
            Error::from(TxnError::from(MvccError::from(
                MvccErrorInner::TxnLockNotFound {
                    start_ts: TimeStamp::new(123),
                    commit_ts: TimeStamp::new(456),
                    key: key.clone(),
                    mvcc_info: None,
                }
            ))),
        );
        expect.set_retryable(expected_retryable_msg);

        // without mvcc
        expect.clear_debug_info();
        assert_eq!(mock_txn_lock_not_found_err(false), expect);

        // with mvcc
        let mvcc_info = Some(mock_mvcc_info());
        expect.set_debug_info(expected_debug_info_from_mvcc(
            key.clone(),
            mvcc_info.clone().unwrap(),
        ));
        assert_eq!(mock_txn_lock_not_found_err(true), expect);
    }

    #[test]
    fn test_extract_key_error_commit_ts_expired() {
        fn mock_commit_ts_expired_err(has_mvcc: bool) -> kvrpcpb::KeyError {
            extract_key_error(&Error::from(TxnError::from(MvccError::from(
                MvccErrorInner::CommitTsExpired {
                    start_ts: TimeStamp::new(123),
                    commit_ts: TimeStamp::new(456),
                    key: b"key".to_vec(),
                    min_commit_ts: TimeStamp::new(789),
                    mvcc_info: has_mvcc.then(|| mock_mvcc_info()),
                },
            ))))
        }

        let key = b"key".to_vec();
        let mut expect = kvrpcpb::KeyError::default();
        let mut commit_ts_expired = kvrpcpb::CommitTsExpired::default();
        commit_ts_expired.set_key(key.clone());
        commit_ts_expired.set_start_ts(123);
        commit_ts_expired.set_attempted_commit_ts(456);
        commit_ts_expired.set_min_commit_ts(789);
        expect.set_commit_ts_expired(commit_ts_expired);

        // without mvcc
        expect.clear_debug_info();
        assert_eq!(mock_commit_ts_expired_err(false), expect);

        // with mvcc
        let mvcc = Some(mock_mvcc_info());
        expect.set_debug_info(expected_debug_info_from_mvcc(
            key.clone(),
            mvcc.clone().unwrap(),
        ));
        assert_eq!(mock_commit_ts_expired_err(true), expect);
    }
}
