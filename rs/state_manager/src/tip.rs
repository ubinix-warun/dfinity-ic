use crate::{
    compute_bundled_manifest, release_lock_and_persist_metadata,
    state_sync::types::{
        FILE_GROUP_CHUNK_ID_OFFSET, MANIFEST_CHUNK_ID_OFFSET, MAX_SUPPORTED_STATE_SYNC_VERSION,
    },
    CheckpointError, PageMapType, SharedState, StateManagerMetrics,
    CRITICAL_ERROR_CHUNK_ID_USAGE_NEARING_LIMITS, NUMBER_OF_CHECKPOINT_THREADS,
};
use crossbeam_channel::{unbounded, Sender};
use ic_base_types::subnet_id_into_protobuf;
use ic_config::flag_status::FlagStatus;
use ic_logger::{error, fatal, info, ReplicaLogger};
use ic_protobuf::state::{
    stats::v1::Stats,
    system_metadata::v1::{SplitFrom, SystemMetadata},
};
use ic_replicated_state::page_map::{
    MergeCandidate, PersistDestination, PersistenceError, StorageMetrics, MAX_NUMBER_OF_FILES,
};
#[allow(unused)]
use ic_replicated_state::{
    canister_state::execution_state::SandboxMemory, page_map::PAGE_SIZE, CanisterState,
    NumWasmPages, PageMap, ReplicatedState,
};
use ic_state_layout::{
    error::LayoutError, CanisterStateBits, CheckpointLayout, ExecutionStateBits, ReadOnly,
    RwPolicy, StateLayout, TipHandler,
};
use ic_sys::fs::defrag_file_partially;
use ic_types::{malicious_flags::MaliciousFlags, CanisterId, Height};
use ic_utils::thread::parallel_map;
use ic_utils::thread::JoinOnDrop;
use prometheus::HistogramTimer;
use rand::prelude::SliceRandom;
use rand::{seq::IteratorRandom, Rng, SeedableRng};
use rand_chacha::ChaChaRng;
use std::collections::BTreeSet;
use std::os::unix::prelude::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const DEFRAG_SIZE: u64 = 1 << 29; // 500 MB
const DEFRAG_SAMPLE: usize = 100;

/// Tip directory can be in following states:
///    Empty: no data available. The only possible request is ResetTipAndMerge to populate it
///    ReadyForPageDeltas(height): ready to write page deltas. We keep track of height to make sure
///    it never decreases.
///    Serialized(height): all the data for the height `height` is flushed to tip, we can rename it
///    to checkpoint.
/// Height(0) is special, it has no corresponding checkpoint to write on top of. That's why the
/// state of a freshly created TipRequest with empty tip directory is ReadyForPageDeltas(0).
#[derive(Debug, Eq, PartialEq)]
enum TipState {
    Empty,
    ReadyForPageDeltas(Height),
    Serialized(Height),
}

/// A single pagemap to truncate and/or flush.
pub struct PageMapToFlush {
    pub page_map_type: PageMapType,
    pub truncate: bool,
    pub page_map: Option<PageMap>,
}

/// Request for the Tip directory handling thread.
pub(crate) enum TipRequest {
    /// Create checkpoint from the current tip for the given height.
    /// Return the created checkpoint or error into the sender.
    /// State: Serialized(height) -> Empty
    TipToCheckpoint {
        height: Height,
        sender: Sender<Result<CheckpointLayout<ReadOnly>, LayoutError>>,
    },
    /// Filter canisters in tip. Remove ones not present in the set.
    /// State: !Empty
    FilterTipCanisters {
        height: Height,
        ids: BTreeSet<CanisterId>,
    },
    /// Flush PageMaps's unflushed delta on disc.
    /// State: ReadyForPageDeltas(h) -> ReadyForPageDeltas(height), height >= h
    FlushPageMapDelta {
        height: Height,
        pagemaps: Vec<PageMapToFlush>,
    },
    /// Reset tip folder to the checkpoint with given height.
    /// Merge overlays in tip folder if necessary.
    /// State: * -> ReadyForPageDeltas(checkpoint_layout.height())
    ResetTipAndMerge {
        checkpoint_layout: CheckpointLayout<ReadOnly>,
        pagemaptypes_with_num_pages: Vec<(PageMapType, usize)>,
    },
    /// Run one round of tip defragmentation.
    /// State: ReadyForPageDeltas(h) -> ReadyForPageDeltas(height), height >= h
    DefragTip {
        height: Height,
        page_map_types: Vec<PageMapType>,
    },
    /// State: ReadyForPageDeltas(h) -> Serialized(height), height >= h
    SerializeToTip {
        height: Height,
        replicated_state: Box<ReplicatedState>,
    },
    /// Compute manifest, store result into states and persist metadata as result.
    /// State: *
    ComputeManifest {
        checkpoint_layout: CheckpointLayout<ReadOnly>,
        manifest_delta: Option<crate::manifest::ManifestDelta>,
        states: Arc<parking_lot::RwLock<SharedState>>,
        persist_metadata_guard: Arc<Mutex<()>>,
    },
    /// Wait for the message to be executed and notify back via sender.
    /// State: *
    Wait {
        sender: Sender<()>,
    },
    Noop,
}

fn request_timer(metrics: &StateManagerMetrics, name: &str) -> HistogramTimer {
    metrics
        .checkpoint_metrics
        .tip_handler_request_duration
        .with_label_values(&[name])
        .start_timer()
}

/// Helper struct for some relevant paths. Also see page_map_paths().
struct PageMapPaths {
    /// Path of the base file
    base_file_path: PathBuf,
    /// All existing overlay files
    existing_overlays: Vec<PathBuf>,
    /// If we write a new overlay file, we use this path
    next_overlay_path: PathBuf,
}

/// Helper function to collect all relevant paths for a PageMap
fn page_map_paths(
    tip_handler: &mut TipHandler,
    height: Height,
    page_map_type: &PageMapType,
) -> Result<PageMapPaths, LayoutError> {
    let layout = &tip_handler.tip(height)?;
    let base_file_path = page_map_type.path(layout)?;
    let existing_overlays = page_map_type.overlays(layout)?;
    let next_overlay_path = page_map_type.overlay(layout, height)?;

    Ok(PageMapPaths {
        base_file_path,
        existing_overlays,
        next_overlay_path,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DowngradeState {
    /// We don't know whether we need to downgrade.
    Unknown,
    /// We have discovered we need to downgrade and did the first step: merge all the overlays into
    /// base files in Tip.
    DowngradedTip,
    /// We have created a first checkpoint without overlays.
    DowngradedCheckpoint,
    /// We don't need to downgrade. Either we have LSMT on or we have successfully created a
    /// first downgraded checkpoint and recomputed full Manifest.
    NotNeeded,
}

pub(crate) fn spawn_tip_thread(
    log: ReplicaLogger,
    mut tip_handler: TipHandler,
    state_layout: StateLayout,
    lsmt_storage: FlagStatus,
    metrics: StateManagerMetrics,
    malicious_flags: MaliciousFlags,
) -> (JoinOnDrop<()>, Sender<TipRequest>) {
    let (tip_sender, tip_receiver) = unbounded();
    let mut thread_pool = scoped_threadpool::Pool::new(NUMBER_OF_CHECKPOINT_THREADS);
    let mut tip_state = TipState::ReadyForPageDeltas(Height::from(0));
    // On top of tip state transitions, we enforce that each checkpoint gets manifest before we
    // create next one. Height(0) doesn't need manifest, so original state is true.
    let mut have_latest_manifest = true;
    let mut downgrade_state = if lsmt_storage == FlagStatus::Enabled {
        DowngradeState::NotNeeded
    } else {
        DowngradeState::Unknown
    };
    let tip_handle = JoinOnDrop::new(
        std::thread::Builder::new()
            .name("TipThread".to_string())
            .spawn(move || {
                while let Ok(req) = tip_receiver.recv() {
                    match req {
                        TipRequest::FilterTipCanisters { height, ids } => {
                            debug_assert_ne!(tip_state, TipState::Empty);

                            let _timer = request_timer(&metrics, "filter_tip_canisters");
                            tip_handler
                                .filter_tip_canisters(height, &ids)
                                .unwrap_or_else(|err| {
                                    fatal!(
                                        log,
                                        "Failed to filter tip canisters for height @{}: {}",
                                        height,
                                        err
                                    )
                                });
                        }
                        TipRequest::TipToCheckpoint { height, sender } => {
                            debug_assert_eq!(tip_state, TipState::Serialized(height));
                            debug_assert!(have_latest_manifest);
                            have_latest_manifest = false;
                            let _timer =
                                request_timer(&metrics, "tip_to_checkpoint_send_checkpoint");
                            let tip = tip_handler.tip(height);
                            match tip {
                                Err(err) => {
                                    sender
                                        .send(Err(err))
                                        .expect("Failed to return TipToCheckpoint error");
                                    continue;
                                }
                                Ok(tip) => {
                                    let cp_or_err = state_layout.scratchpad_to_checkpoint(
                                        tip,
                                        height,
                                        Some(&mut thread_pool),
                                    );
                                    match cp_or_err {
                                        Err(err) => {
                                            sender
                                                .send(Err(err))
                                                .expect("Failed to return TipToCheckpoint error");
                                            continue;
                                        }
                                        Ok(cp) => {
                                            sender
                                                .send(Ok(cp.clone()))
                                                .expect("Failed to return TipToCheckpoint result");
                                        }
                                    }
                                }
                            }
                            if downgrade_state == DowngradeState::DowngradedTip {
                                downgrade_state = DowngradeState::DowngradedCheckpoint;
                            }
                        }

                        TipRequest::FlushPageMapDelta { height, pagemaps } => {
                            let _timer = request_timer(&metrics, "flush_unflushed_delta");
                            #[cfg(debug_assertions)]
                            match tip_state {
                                TipState::ReadyForPageDeltas(h) => debug_assert!(height >= h),
                                _ => panic!("Unexpected tip state: {:?}", tip_state),
                            }
                            tip_state = TipState::ReadyForPageDeltas(height);
                            parallel_map(
                                &mut thread_pool,
                                pagemaps.into_iter().map(
                                    |PageMapToFlush {
                                         page_map_type,
                                         truncate,
                                         page_map,
                                     }| {
                                        (
                                            truncate,
                                            page_map,
                                            page_map_paths(
                                                &mut tip_handler,
                                                height,
                                                &page_map_type,
                                            )
                                            .unwrap_or_else(|err| {
                                                fatal!(log, "Failed to flush page map: {}", err);
                                            }),
                                        )
                                    },
                                ),
                                |(
                                    truncate,
                                    page_map,
                                    PageMapPaths {
                                        base_file_path,
                                        existing_overlays,
                                        next_overlay_path,
                                    },
                                )| {
                                    if *truncate {
                                        delete_pagemap_files(
                                            &log,
                                            base_file_path,
                                            existing_overlays,
                                        );
                                    }
                                    if page_map.is_some()
                                        && !page_map.as_ref().unwrap().unflushed_delta_is_empty()
                                    {
                                        let dst = PersistDestination::new(
                                            base_file_path.clone(),
                                            next_overlay_path.clone(),
                                            lsmt_storage,
                                        );
                                        page_map
                                            .as_ref()
                                            .unwrap()
                                            .persist_unflushed_delta(dst, &metrics.storage_metrics)
                                            .unwrap_or_else(|err| {
                                                fatal!(
                                                    log,
                                                    "Failed to persist unflushed delta: {}",
                                                    err
                                                );
                                            });
                                    }
                                },
                            );
                        }
                        TipRequest::SerializeToTip {
                            height,
                            replicated_state,
                        } => {
                            let _timer = request_timer(&metrics, "serialize_to_tip");
                            #[cfg(debug_assertions)]
                            match tip_state {
                                TipState::ReadyForPageDeltas(h) => debug_assert!(height >= h),
                                _ => panic!("Unexpected tip state: {:?}", tip_state),
                            }
                            tip_state = TipState::Serialized(height);
                            serialize_to_tip(
                                &log,
                                &replicated_state,
                                &tip_handler.tip(height).unwrap_or_else(|err| {
                                    fatal!(
                                        log,
                                        "Failed to get tip @{} to serialize to: {}",
                                        height,
                                        err
                                    );
                                }),
                                &mut thread_pool,
                                &metrics.storage_metrics,
                                lsmt_storage,
                            )
                            .unwrap_or_else(|err| {
                                fatal!(log, "Failed to serialize to tip @{}: {}", height, err);
                            });
                        }
                        TipRequest::ResetTipAndMerge {
                            checkpoint_layout,
                            pagemaptypes_with_num_pages,
                        } => {
                            let _timer = request_timer(&metrics, "reset_tip_to");
                            let height = checkpoint_layout.height();
                            tip_state = TipState::ReadyForPageDeltas(height);
                            tip_handler
                                .reset_tip_to(
                                    &state_layout,
                                    &checkpoint_layout,
                                    lsmt_storage,
                                    Some(&mut thread_pool),
                                )
                                .unwrap_or_else(|err| {
                                    fatal!(
                                        log,
                                        "Failed to reset tip to height @{}: {}",
                                        height,
                                        err
                                    );
                                });
                            match lsmt_storage {
                                FlagStatus::Enabled => merge(
                                    &mut tip_handler,
                                    &pagemaptypes_with_num_pages,
                                    height,
                                    &mut thread_pool,
                                    &log,
                                    &metrics,
                                ),
                                FlagStatus::Disabled => {
                                    if downgrade_state == DowngradeState::Unknown {
                                        if merge_full(
                                            &mut tip_handler,
                                            &pagemaptypes_with_num_pages,
                                            height,
                                            &mut thread_pool,
                                            &log,
                                            &metrics,
                                        ) {
                                            downgrade_state = DowngradeState::DowngradedTip;
                                        } else {
                                            downgrade_state = DowngradeState::NotNeeded;
                                        }
                                    }
                                }
                            };
                        }
                        TipRequest::DefragTip {
                            height,
                            page_map_types,
                        } => {
                            debug_assert_ne!(tip_state, TipState::Empty);
                            tip_state = TipState::ReadyForPageDeltas(height);
                            let _timer = request_timer(&metrics, "defrag_tip");
                            defrag_tip(
                                &tip_handler.tip(height).unwrap_or_else(|err| {
                                    fatal!(log, "Failed to get tip @{} to defrag: {}", height, err);
                                }),
                                &page_map_types,
                                DEFRAG_SIZE,
                                DEFRAG_SAMPLE,
                                height.get(),
                            )
                            .unwrap_or_else(|err| {
                                fatal!(log, "Failed to defrag tip @{}: {}", height, err);
                            });
                        }

                        TipRequest::Wait { sender } => {
                            let _timer = request_timer(&metrics, "wait");
                            let _ = sender.send(());
                        }

                        TipRequest::ComputeManifest {
                            checkpoint_layout,
                            manifest_delta,
                            states,
                            persist_metadata_guard,
                        } => {
                            let _timer = request_timer(&metrics, "compute_manifest");
                            // If we are downgrading, do a full manifest computation
                            let manifest_delta =
                                if downgrade_state == DowngradeState::DowngradedCheckpoint {
                                    None
                                } else {
                                    manifest_delta
                                };
                            handle_compute_manifest_request(
                                &mut thread_pool,
                                &metrics,
                                &log,
                                &states,
                                &state_layout,
                                &checkpoint_layout,
                                manifest_delta,
                                &persist_metadata_guard,
                                &malicious_flags,
                            );
                            have_latest_manifest = true;
                            if downgrade_state == DowngradeState::DowngradedCheckpoint {
                                downgrade_state = DowngradeState::NotNeeded;
                            }
                        }
                        TipRequest::Noop => {}
                    }
                }
            })
            .expect("failed to spawn tip thread"),
    );
    (tip_handle, tip_sender)
}

#[derive(Clone, Debug)]
struct MergeCandidateAndMetrics {
    // Merge candidate if any merge is applicable.
    merge_candidate: Option<MergeCandidate>,

    // Number of files for the page map currently (before merge).
    num_files_before: usize,

    // Storage size on disk currently (before merge) and an estimate of it for after the merge.
    storage_size_bytes_before: usize,
    storage_size_bytes_after: usize,

    // Page map size in RAM.
    page_map_size_bytes: usize,

    // Estimate how much is necessary to flush to disk to apply the merge.
    write_size_bytes: usize,
}

impl MergeCandidateAndMetrics {
    fn new(
        merge_candidate: Option<MergeCandidate>,
        base_path: &Path,
        existing_overlays: &[PathBuf],
        page_map_num_pages: usize,
    ) -> Result<Self, PersistenceError> {
        let merge_all = merge_candidate
            .as_ref()
            .map_or(false, |m| m.is_full_merge());
        let existing_base = if base_path.exists() {
            Some(base_path.to_path_buf())
        } else {
            None
        };
        let existing_files = existing_base.iter().chain(existing_overlays.iter());
        let existing_files_lengths: Vec<u64> = existing_files
            .map(|f| std::fs::metadata(f).unwrap().len())
            .collect();
        let num_files_before = existing_files_lengths.len();

        let page_map_size_bytes = page_map_num_pages * PAGE_SIZE;
        let storage_size_bytes_before = existing_files_lengths.iter().sum::<u64>() as usize;
        // If we do partial merges, in the worst case scenario all the overlays contain
        // non-overlapping pages and we don't save space. We do these merges to maintain a pyramid
        // shape of file sizes and to cap number of files.
        let storage_size_bytes_after = if merge_all {
            page_map_size_bytes
        } else {
            storage_size_bytes_before
        };
        let merge_input_bytes = match merge_candidate.as_ref() {
            None => 0,
            Some(m) => m.input_size_bytes()?,
        };
        let write_size_bytes = if merge_all {
            page_map_size_bytes
        } else {
            std::cmp::min(page_map_size_bytes, merge_input_bytes as usize)
        };
        Ok(MergeCandidateAndMetrics {
            merge_candidate,
            num_files_before,
            storage_size_bytes_before,
            storage_size_bytes_after,
            page_map_size_bytes,
            write_size_bytes,
        })
    }
}

/// Merge excessive overlays.
///
/// We want to achieve two metrics:
///     1) Total overhead, that is total_size_on_disk / total_size_of_pages_in_pagemaps to be at
///        most 2.5
///     We don't cap overhead of an individual pagemap.
///     2) Number of files in each pagemap to be not too high.
/// Moreover, we don't want to throttle execution. For example, if we have exactly the same load
/// pattern in many canisters, we don't want to merge all the data for all the canisters in the
/// same checkpoint interval but rather to spread the load. The heaviest operation for the system
/// is flushing data to disk, so we try to cap it.
/// We need the merge to be deterministic, i.e. any two instances of the subnet must have exactly
/// the same layout for all pagemaps.
///
///
/// First, sort the vector of MergeCandidates by decreasing `num_files_before`. Schedule for
/// merging the head of the sorted vector, which contains either all MergeCandidates with more than
/// Storage::MAX_NUMBER_OF_FILES or top ones till accumulated write_size_bytes is more than one
/// quarter of `sum(storage_size_bytes_before)`. This way we iterate through all page maps with more
/// than MAX_NUMBER_OF_FILES at most in 4 checkpoint intervals. Since we produce at most 2 overlays
/// per checkpoint per `PageMap`, we have a cap of `MAX_NUMBER_OF_FILES` + 2 * 4 files per `PageMap`
/// at any checkpoint.
///
/// Then we need to take care of storage overhead. Storage overhead is the ratio
/// `sum(storage_size_bytes_after)` / `sum(page_map_size_bytes)`. In other words, we need
/// `sum(storage_size_bytes_after)` to become <= 2.5 * `sum(page_map_size_bytes)`.
/// Note, that after applying merge any `PageMap` reaches overhead of at most 2.0. This is
/// guaranteed by the `MergeCandidate::new` logic. So we need to schedule enough merge candidates
/// till our overhead goal is met.
/// We already have some merges scheduled that potentially reduce overhead. We calculate the
/// storage size that is going to be after we apply them. If it's more than
/// 2.5 * `sum(page_map_size_bytes)` we extend our list of scheduled merges starting with lowest
/// hanging fruits, that is MergeCandidates with highest
/// (`storage_size_bytes_before` - `storage_size_bytes_after`) / `write_size_bytes`.
///
/// Write size calculation.
/// The merges for number of files are at most 1/4 of allowed state size.
/// Merges for overhead have input with overhead >= 2.5 and output being == 1.0. Meaning by writing
/// 1 MiB to disk during merge, we replace what used to be >= 2.5 MiB on disk with 1 MiB.
/// In other words, 1 MiB of write during merge reduces storage by at least 1.5 MiB.
/// Let's say last checkpoint was 100 GiB of state size with the max overhead, meaning storage size
/// is 250 GiB. During the checkpoint interval we flushed 60 GiB of page delta, now the state size is
/// 310 GiB and the overhead is 3.1. We need to reduce the storage by 60 GiB. We need to write at
/// most 60 GiB / 1.5 = 40 GiB in order to achieve it, with some round off error for a canister
/// size.
/// For a more general estimate, the data written to disk during checkpoint interval is at most
/// max_dirty_pages, meaning we need to write max_dirty_pages / 1.5 + one canister size to reduce
/// the overhead under 2.5 again.
/// The total write is at most 1/4 state size + 2/3 * max_dirty_pages + the size of the last
/// `PageMap`. Note that if canisters are removed, upgraded, or otherwise delete data, this can
/// further increase the amount of data written in order to enforce the storage overhead.
fn merge(
    tip_handler: &mut TipHandler,
    pagemaptypes_with_num_pages: &[(PageMapType, usize)],
    height: Height,
    thread_pool: &mut scoped_threadpool::Pool,
    log: &ReplicaLogger,
    metrics: &StateManagerMetrics,
) {
    let merges_with_metrics = parallel_map(
        thread_pool,
        pagemaptypes_with_num_pages
            .iter()
            .map(|(page_map_type, num_pages)| {
                (
                    *num_pages,
                    page_map_paths(tip_handler, height, page_map_type).unwrap_or_else(|err| {
                        fatal!(log, "Failed to get page map paths: {}", err);
                    }),
                )
            }),
        |(
            num_pages,
            PageMapPaths {
                base_file_path,
                existing_overlays,
                next_overlay_path,
            },
        )| {
            let m = MergeCandidate::new(
                base_file_path,
                next_overlay_path,
                base_file_path,
                existing_overlays,
            )
            .unwrap_or_else(|err| {
                fatal!(log, "Failed to get MergeCandidate: {}", err);
            });
            MergeCandidateAndMetrics::new(m, base_file_path, existing_overlays, *num_pages)
                .unwrap_or_else(|err| {
                    fatal!(log, "Failed to get metrics for MergeCandidate: {}", err);
                })
        },
    );

    let pm_size: usize = merges_with_metrics
        .iter()
        .map(|m| m.page_map_size_bytes)
        .sum();
    let storage_size: usize = merges_with_metrics
        .iter()
        .map(|m| m.storage_size_bytes_before)
        .sum();
    // Max 2.5 overhead
    let max_storage = pm_size * 2 + pm_size / 2;

    // Discard ones where no merge is applicable.
    // These have number of files less or equal to Storage::MAX_NUMBER_OF_FILES and at most 2.0
    // overhead.
    let mut merges_with_metrics: Vec<MergeCandidateAndMetrics> = merges_with_metrics
        .into_iter()
        .filter(|m| m.merge_candidate.is_some())
        .collect();

    merges_with_metrics.sort_by_key(|m| -(m.num_files_before as i64));
    let storage_to_merge_for_filenum = pm_size / 4;
    let merges_by_filenum = merges_with_metrics
        .iter()
        .scan(0, |state, m| {
            if *state >= storage_to_merge_for_filenum || m.num_files_before < MAX_NUMBER_OF_FILES {
                None
            } else {
                *state += m.page_map_size_bytes;
                Some(())
            }
        })
        .count();

    // [0; merges_by_filenum) are already scheduled, some of the rest may be necessary to achieve
    // low enough overhead.
    let mut scheduled_merges = merges_with_metrics;
    let mut merges_with_metrics = scheduled_merges.split_off(merges_by_filenum);

    // Sort by ratio of saved bytes to write size.
    merges_with_metrics.sort_by_key(|m| {
        if m.write_size_bytes != 0 {
            // Fixed point to compute overhead ratio for sort.
            -1000i64 * (m.storage_size_bytes_before as i64 - m.storage_size_bytes_after as i64)
                / m.write_size_bytes as i64
        } else {
            0
        }
    });
    let storage_to_save = storage_size as i64 - max_storage as i64;
    // For a full merge the resulting base file can be larger than sum of the overlays,
    // so we need a signed accumulator.
    let mut storage_saved: i64 = scheduled_merges
        .iter()
        .map(|m| m.storage_size_bytes_before as i64 - m.storage_size_bytes_after as i64)
        .sum();
    for m in merges_with_metrics.into_iter() {
        if storage_saved >= storage_to_save {
            break;
        }

        storage_saved += m.storage_size_bytes_before as i64 - m.storage_size_bytes_after as i64;
        // Only full merges reduce overhead, and there should be enough of them to reach
        // `storage_to_save` before tapping into partial merges.
        debug_assert!(m.merge_candidate.as_ref().unwrap().is_full_merge());
        scheduled_merges.push(m);
    }
    info!(
        log,
        "Merging {} files out of {}; pm_size: {}; storage_size: {}; max_storage: {}, storage_saves: {}, merges_by_filenum: {}",
        scheduled_merges.len(),
        pagemaptypes_with_num_pages.len(),
        pm_size,
        storage_size,
        max_storage,
        storage_saved,
        merges_by_filenum,
    );

    parallel_map(thread_pool, scheduled_merges.iter(), |m| {
        m.merge_candidate
            .as_ref()
            .unwrap()
            .apply(&metrics.storage_metrics)
    });
}

/// Merge all the overlays (if any) into bases.
/// Return true if any merge was done.
fn merge_full(
    tip_handler: &mut TipHandler,
    pagemaptypes_with_num_pages: &[(PageMapType, usize)],
    height: Height,
    thread_pool: &mut scoped_threadpool::Pool,
    log: &ReplicaLogger,
    metrics: &StateManagerMetrics,
) -> bool {
    let rewritten = parallel_map(
        thread_pool,
        pagemaptypes_with_num_pages
            .iter()
            .map(|(page_map_type, _)| {
                page_map_paths(tip_handler, height, page_map_type).unwrap_or_else(|err| {
                    fatal!(log, "Failed to merge page map: {}", err);
                })
            }),
        |PageMapPaths {
             base_file_path,
             existing_overlays,
             next_overlay_path: _,
         }| {
            let merge_candidate =
                MergeCandidate::full_merge(base_file_path, base_file_path, existing_overlays);
            if let Some(m) = merge_candidate.as_ref() {
                m.apply(&metrics.storage_metrics).unwrap_or_else(|err| {
                    fatal!(log, "Failed to apply MergeCandidate for downgrade: {}", err);
                });
            }
            merge_candidate.is_some()
        },
    );

    return rewritten.iter().any(|b| *b);
}

fn serialize_to_tip(
    log: &ReplicaLogger,
    state: &ReplicatedState,
    tip: &CheckpointLayout<RwPolicy<TipHandler>>,
    thread_pool: &mut scoped_threadpool::Pool,
    metrics: &StorageMetrics,
    lsmt_storage: FlagStatus,
) -> Result<(), CheckpointError> {
    // Serialize ingress history separately. The `SystemMetadata` proto does not
    // encode it.
    //
    // This also makes it possible to validate post-split states simply by comparing
    // manifest file hashes (the ingress history is initially preserved unmodified
    // on both sides of the split, while the system metadata is not).
    let ingress_history = (&state.system_metadata().ingress_history).into();
    tip.ingress_history().serialize(ingress_history)?;

    let system_metadata: SystemMetadata = state.system_metadata().into();
    tip.system_metadata().serialize(system_metadata)?;

    // The split marker is also serialized separately from `SystemMetadata` because
    // preserving the latter unmodified during a split makes verification a matter
    // of comparing manifest file hashes.
    match state.system_metadata().split_from {
        Some(subnet_id) => {
            tip.split_marker().serialize(SplitFrom {
                subnet_id: Some(subnet_id_into_protobuf(subnet_id)),
            })?;
        }
        None => {
            tip.split_marker().try_remove_file()?;
        }
    }

    tip.subnet_queues()
        .serialize((state.subnet_queues()).into())?;

    tip.stats().serialize(Stats {
        query_stats: state.query_stats().as_query_stats(),
    })?;

    let results = parallel_map(thread_pool, state.canisters_iter(), |canister_state| {
        serialize_canister_to_tip(log, canister_state, tip, metrics, lsmt_storage)
    });

    for result in results.into_iter() {
        result?;
    }

    Ok(())
}

fn serialize_canister_to_tip(
    log: &ReplicaLogger,
    canister_state: &CanisterState,
    tip: &CheckpointLayout<RwPolicy<TipHandler>>,
    metrics: &StorageMetrics,
    lsmt_storage: FlagStatus,
) -> Result<(), CheckpointError> {
    let canister_layout = tip.canister(&canister_state.canister_id())?;
    canister_layout
        .queues()
        .serialize(canister_state.system_state.queues().into())?;

    let execution_state_bits = match &canister_state.execution_state {
        Some(execution_state) => {
            let wasm_binary = &execution_state.wasm_binary.binary;
            match wasm_binary.file() {
                Some(path) => {
                    let wasm = canister_layout.wasm();
                    // This if should always be false, as we reflink copy the entire checkpoint to the tip
                    // It is left in mainly as defensive programming
                    if !wasm.raw_path().exists() {
                        ic_state_layout::utils::do_copy(log, path, wasm.raw_path()).map_err(
                            |io_err| CheckpointError::IoError {
                                path: path.to_path_buf(),
                                message: "failed to copy Wasm file".to_string(),
                                io_err: io_err.to_string(),
                            },
                        )?;
                    }
                }
                None => {
                    // Canister was installed/upgraded. Persist the new wasm binary.
                    canister_layout
                        .wasm()
                        .serialize(&execution_state.wasm_binary.binary)?;
                }
            }
            let memory_dst = PersistDestination::new(
                canister_layout.vmemory_0(),
                canister_layout.vmemory_0_overlay(tip.height()),
                lsmt_storage,
            );
            let stable_dst = PersistDestination::new(
                canister_layout.stable_memory_blob(),
                canister_layout.stable_memory_overlay(tip.height()),
                lsmt_storage,
            );
            execution_state
                .wasm_memory
                .page_map
                .persist_delta(memory_dst, metrics)?;
            execution_state
                .stable_memory
                .page_map
                .persist_delta(stable_dst, metrics)?;

            Some(ExecutionStateBits {
                exported_globals: execution_state.exported_globals.clone(),
                heap_size: execution_state.wasm_memory.size,
                exports: execution_state.exports.clone(),
                last_executed_round: execution_state.last_executed_round,
                metadata: execution_state.metadata.clone(),
                binary_hash: Some(execution_state.wasm_binary.binary.module_hash().into()),
                next_scheduled_method: execution_state.next_scheduled_method,
            })
        }
        None => {
            delete_pagemap_files(
                log,
                &canister_layout.vmemory_0(),
                &canister_layout.vmemory_0_overlays()?,
            );
            delete_pagemap_files(
                log,
                &canister_layout.stable_memory_blob(),
                &canister_layout.stable_memory_overlays()?,
            );
            canister_layout.wasm().try_delete_file()?;
            None
        }
    };

    let wasm_chunk_store_dst = PersistDestination::new(
        canister_layout.wasm_chunk_store(),
        canister_layout.wasm_chunk_store_overlay(tip.height()),
        lsmt_storage,
    );
    canister_state
        .system_state
        .wasm_chunk_store
        .page_map()
        .persist_delta(wasm_chunk_store_dst, metrics)?;

    // Priority credit must be zero at this point
    assert_eq!(canister_state.scheduler_state.priority_credit.get(), 0);
    canister_layout.canister().serialize(
        CanisterStateBits {
            controllers: canister_state.system_state.controllers.clone(),
            last_full_execution_round: canister_state.scheduler_state.last_full_execution_round,
            call_context_manager: canister_state.system_state.call_context_manager().cloned(),
            compute_allocation: canister_state.scheduler_state.compute_allocation,
            accumulated_priority: canister_state.scheduler_state.accumulated_priority,
            memory_allocation: canister_state.system_state.memory_allocation,
            freeze_threshold: canister_state.system_state.freeze_threshold,
            cycles_balance: canister_state.system_state.balance(),
            cycles_debit: canister_state.system_state.ingress_induction_cycles_debit(),
            reserved_balance: canister_state.system_state.reserved_balance(),
            reserved_balance_limit: canister_state.system_state.reserved_balance_limit(),
            execution_state_bits,
            status: canister_state.system_state.status.clone(),
            scheduled_as_first: canister_state
                .system_state
                .canister_metrics
                .scheduled_as_first,
            skipped_round_due_to_no_messages: canister_state
                .system_state
                .canister_metrics
                .skipped_round_due_to_no_messages,
            executed: canister_state.system_state.canister_metrics.executed,
            interrupted_during_execution: canister_state
                .system_state
                .canister_metrics
                .interrupted_during_execution,
            certified_data: canister_state.system_state.certified_data.clone(),
            consumed_cycles_since_replica_started: canister_state
                .system_state
                .canister_metrics
                .consumed_cycles_since_replica_started,
            stable_memory_size: canister_state
                .execution_state
                .as_ref()
                .map(|es| es.stable_memory.size)
                .unwrap_or_else(|| NumWasmPages::from(0)),
            heap_delta_debit: canister_state.scheduler_state.heap_delta_debit,
            install_code_debit: canister_state.scheduler_state.install_code_debit,
            time_of_last_allocation_charge_nanos: canister_state
                .scheduler_state
                .time_of_last_allocation_charge
                .as_nanos_since_unix_epoch(),
            task_queue: canister_state
                .system_state
                .task_queue
                .clone()
                .into_iter()
                .collect(),
            global_timer_nanos: canister_state
                .system_state
                .global_timer
                .to_nanos_since_unix_epoch(),
            canister_version: canister_state.system_state.canister_version,
            consumed_cycles_since_replica_started_by_use_cases: canister_state
                .system_state
                .canister_metrics
                .get_consumed_cycles_since_replica_started_by_use_cases()
                .clone(),
            canister_history: canister_state.system_state.get_canister_history().clone(),
            wasm_chunk_store_metadata: canister_state
                .system_state
                .wasm_chunk_store
                .metadata()
                .clone(),
            total_query_stats: canister_state.scheduler_state.total_query_stats.clone(),
            log_visibility: canister_state.system_state.log_visibility,
        }
        .into(),
    )?;
    Ok(())
}

/// Defragments part of the tip directory.
///
/// The way we use PageMap files in the tip, namely by having a
/// long-living file, that we alternatively write to in small 4KB
/// pages and reflink copy to the checkpoint folder, the files end up
/// fragmented on disk. In particular, the metadata the file system
/// keeps on which pages are shared between files and which pages are
/// unique to a file grows quite complicated, which noticebly slows
/// down reflink copying of those files. It can therefore be
/// beneficial to defragment files, especially in situations where a
/// file had a lot of writes in the past but is mostly being read now.
///
/// The current defragmentation strategy is to pseudorandomly choose a
/// chunk of size max_size among the eligible files, read it to memory,
/// and write it back to the file. The effect is that this chunk is
/// definitely unique to the tip at the end of defragmentation.
pub fn defrag_tip(
    tip: &CheckpointLayout<RwPolicy<TipHandler>>,
    page_maps: &[PageMapType],
    max_size: u64,
    max_files: usize,
    seed: u64,
) -> Result<(), CheckpointError> {
    let mut rng = ChaChaRng::seed_from_u64(seed);

    // We sample the set of page maps down in order to avoid reading
    // the metadata of each file. This is a compromise between
    // weighting the probabilities by size and picking a uniformly
    // random file.  The former (without subsampling) would be
    // unnecessarily expensive, the latter would perform poorly in a
    // situation with many empty files and a few large ones, doing
    // no-ops on empty files with high probability.
    let page_map_subset = page_maps.iter().choose_multiple(&mut rng, max_files);

    let path_with_sizes: Vec<(PathBuf, u64)> = page_map_subset
        .iter()
        .filter_map(|entry| {
            let path = entry.path(tip).ok()?;
            let size = path.metadata().ok()?.size();
            Some((path, size))
        })
        .collect();

    // We choose a file weighted by its size. This way, every bit in
    // the state has (roughly) the same probability of being
    // defragmented. If we chose the file uniformaly at random, we
    // would end up defragmenting the smallest file too often. The choice
    // failing is not an error, as it will happen if all files are
    // empty
    if let Ok((path, size)) = path_with_sizes.choose_weighted(&mut rng, |entry| entry.1) {
        let write_size = size.min(&max_size);
        let offset = rng.gen_range(0..=size - write_size);

        defrag_file_partially(path, offset, write_size.to_owned() as usize).map_err(|err| {
            CheckpointError::IoError {
                path: path.to_path_buf(),
                message: "failed to defrag file".into(),
                io_err: err.to_string(),
            }
        })?;
    }
    Ok(())
}

fn delete_pagemap_files(log: &ReplicaLogger, base: &Path, overlays: &[PathBuf]) {
    if base.exists() {
        std::fs::remove_file(base).unwrap_or_else(|err| {
            fatal!(
                log,
                "Failed to remove base file at {}: {}",
                base.display(),
                err
            );
        });
    }

    for overlay in overlays {
        std::fs::remove_file(overlay).unwrap_or_else(|err| {
            fatal!(
                log,
                "Failed to remove overlay file {}: {}",
                overlay.display(),
                err
            );
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_compute_manifest_request(
    thread_pool: &mut scoped_threadpool::Pool,
    metrics: &StateManagerMetrics,
    log: &ReplicaLogger,
    states: &parking_lot::RwLock<SharedState>,
    state_layout: &StateLayout,
    checkpoint_layout: &CheckpointLayout<ReadOnly>,
    manifest_delta: Option<crate::manifest::ManifestDelta>,
    persist_metadata_guard: &Arc<Mutex<()>>,
    #[allow(unused_variables)] malicious_flags: &MaliciousFlags,
) {
    let system_metadata = checkpoint_layout
        .system_metadata()
        .deserialize()
        .unwrap_or_else(|err| {
            fatal!(
                log,
                "Failed to decode system metadata @{}: {}",
                checkpoint_layout.height(),
                err
            )
        });

    let state_sync_version = system_metadata.state_sync_version.try_into().unwrap();

    assert!(
        state_sync_version <= MAX_SUPPORTED_STATE_SYNC_VERSION,
        "Unable to compute a manifest with version {:?}. \
                    Maximum supported StateSync version is {:?}",
        state_sync_version,
        MAX_SUPPORTED_STATE_SYNC_VERSION
    );

    let start = Instant::now();
    let manifest = crate::manifest::compute_manifest(
        thread_pool,
        &metrics.manifest_metrics,
        log,
        state_sync_version,
        checkpoint_layout,
        crate::state_sync::types::DEFAULT_CHUNK_SIZE,
        manifest_delta,
    )
    .unwrap_or_else(|err| {
        fatal!(
            log,
            "Failed to compute manifest for checkpoint @{} after {:?}: {}",
            checkpoint_layout.height(),
            start.elapsed(),
            err
        )
    });

    let elapsed = start.elapsed();
    metrics
        .checkpoint_op_duration
        .with_label_values(&["compute_manifest"])
        .observe(elapsed.as_secs_f64());

    info!(
        log,
        "Computed manifest version {:?} for state @{} in {:?}",
        manifest.version,
        checkpoint_layout.height(),
        elapsed
    );

    let state_size_bytes: i64 = manifest
        .file_table
        .iter()
        .map(|f| f.size_bytes as i64)
        .sum();

    metrics.state_size.set(state_size_bytes);
    metrics
        .last_computed_manifest_height
        .set(checkpoint_layout.height().get() as i64);

    // This is where we maliciously alter the root_hash!
    #[cfg(feature = "malicious_code")]
    let malicious_root_hash = crate::maliciously_return_wrong_hash(
        &manifest,
        log,
        malicious_flags,
        checkpoint_layout.height(),
    );

    let num_file_group_chunks = crate::manifest::build_file_group_chunks(&manifest).len();

    let bundled_manifest = compute_bundled_manifest(manifest);

    #[cfg(feature = "malicious_code")]
    let bundled_manifest = crate::BundledManifest {
        root_hash: malicious_root_hash,
        ..bundled_manifest
    };

    info!(
        log,
        "Computed root hash {:?} of state @{}",
        bundled_manifest.root_hash,
        checkpoint_layout.height()
    );

    metrics
        .manifest_metrics
        .file_group_chunks
        .set(num_file_group_chunks as i64);

    let file_group_chunk_id_range_length =
        (MANIFEST_CHUNK_ID_OFFSET - FILE_GROUP_CHUNK_ID_OFFSET) as usize;
    if num_file_group_chunks > file_group_chunk_id_range_length / 2 {
        error!(
            log,
            "{}: The number of file group chunks is greater than half of the available ID space in state sync. Number of file group chunks: {}, file group chunk ID range length: {}",
            CRITICAL_ERROR_CHUNK_ID_USAGE_NEARING_LIMITS,
            num_file_group_chunks,
            file_group_chunk_id_range_length,
        );
        metrics
            .manifest_metrics
            .chunk_id_usage_nearing_limits_critical
            .inc();
    }

    let num_sub_manifest_chunks = bundled_manifest.meta_manifest.sub_manifest_hashes.len();
    metrics
        .manifest_metrics
        .sub_manifest_chunks
        .set(num_sub_manifest_chunks as i64);

    let sub_manifest_chunk_id_range_length = u32::MAX - MANIFEST_CHUNK_ID_OFFSET + 1;
    if num_sub_manifest_chunks > sub_manifest_chunk_id_range_length as usize / 2 {
        error!(
            log,
            "{}: The number of sub-manifest chunks is greater than half of the available ID space in state sync. Number of sub-manifest chunks: {}, sub-manifest chunk ID range length: {}",
            CRITICAL_ERROR_CHUNK_ID_USAGE_NEARING_LIMITS,
            num_sub_manifest_chunks,
            sub_manifest_chunk_id_range_length,
        );
        metrics
            .manifest_metrics
            .chunk_id_usage_nearing_limits_critical
            .inc();
    }

    let mut states = states.write();

    if let Some(metadata) = states.states_metadata.get_mut(&checkpoint_layout.height()) {
        metadata.bundled_manifest = Some(bundled_manifest);
    }

    release_lock_and_persist_metadata(log, metrics, state_layout, states, persist_metadata_guard);
}

#[cfg(test)]
mod test {
    use super::*;
    use ic_config::state_manager::lsmt_storage_default;
    use ic_metrics::MetricsRegistry;
    use ic_test_utilities::types::ids::canister_test_id;
    use ic_test_utilities_logger::with_test_replica_logger;
    use ic_test_utilities_tmpdir::tmpdir;

    #[test]
    fn dont_crash_or_hang() {
        with_test_replica_logger(|log| {
            let tmp = tmpdir("checkpoint");
            let root = tmp.path().to_path_buf();
            let layout = StateLayout::try_new(log.clone(), root, &MetricsRegistry::new()).unwrap();
            let metrics_registry = ic_metrics::MetricsRegistry::new();
            let metrics = StateManagerMetrics::new(&metrics_registry, log.clone());
            let tip_handler = layout.capture_tip_handler();
            let (_h, _s) = spawn_tip_thread(
                log,
                tip_handler,
                layout,
                lsmt_storage_default(),
                metrics,
                MaliciousFlags::default(),
            );
        });
    }

    #[test]
    fn defrag_is_safe() {
        with_test_replica_logger(|log| {
            let tmp = tmpdir("checkpoint");
            let root = tmp.path().to_path_buf();
            let mut tip_handler = StateLayout::try_new(log, root, &MetricsRegistry::new())
                .unwrap()
                .capture_tip_handler();
            let tip = tip_handler.tip(Height::new(42)).unwrap();

            let defrag_size = 1 << 20; // 1MB

            let page_maps: Vec<PageMapType> = vec![
                PageMapType::StableMemory(canister_test_id(100)),
                PageMapType::WasmMemory(canister_test_id(100)),
                PageMapType::WasmMemory(canister_test_id(101)),
            ];

            let paths: Vec<PathBuf> = page_maps
                .iter()
                .map(|page_map_type| page_map_type.path(&tip).unwrap())
                .collect();

            for path in &paths {
                assert!(!path.exists());
            }

            defrag_tip(&tip, &page_maps, defrag_size, 100, 0).unwrap();

            for path in &paths {
                assert!(!path.exists());
            }

            for factor in 1..3 {
                let short_data: Vec<u8> = vec![42; (defrag_size / factor) as usize];
                let long_data: Vec<u8> = vec![43; (defrag_size * factor) as usize];
                let empty: &[u8] = &[];

                std::fs::write(&paths[0], &short_data).unwrap();
                std::fs::write(&paths[1], &long_data).unwrap();
                // third file is an empty file
                std::fs::write(&paths[2], empty).unwrap();

                let check_files = || {
                    assert_eq!(std::fs::read(&paths[0]).unwrap(), short_data);
                    assert_eq!(std::fs::read(&paths[1]).unwrap(), long_data);
                    assert!(paths[2].exists());
                    assert_eq!(std::fs::read(&paths[2]).unwrap(), empty);
                };

                check_files();

                for i in 0..100 {
                    defrag_tip(&tip, &page_maps, defrag_size, i as usize, i).unwrap();
                    check_files();
                }
            }
        });
    }
}
