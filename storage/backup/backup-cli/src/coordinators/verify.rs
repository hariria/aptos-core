// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{
    backup_types::{
        epoch_ending::restore::EpochHistoryRestoreController,
        state_snapshot::restore::{StateSnapshotRestoreController, StateSnapshotRestoreOpt},
        transaction::restore::TransactionRestoreBatchController,
    },
    metadata,
    metadata::cache::MetadataCacheOpt,
    metrics::verify::{
        VERIFY_COORDINATOR_FAIL_TS, VERIFY_COORDINATOR_START_TS, VERIFY_COORDINATOR_SUCC_TS,
    },
    storage::BackupStorage,
    utils::{unix_timestamp_sec, GlobalRestoreOptions, RestoreRunMode, TrustedWaypointOpt},
};
use anyhow::Result;
use aptos_executor_types::VerifyExecutionMode;
use aptos_logger::prelude::*;
use aptos_types::transaction::Version;
use std::sync::Arc;

pub struct VerifyCoordinator {
    storage: Arc<dyn BackupStorage>,
    metadata_cache_opt: MetadataCacheOpt,
    trusted_waypoints_opt: TrustedWaypointOpt,
    concurrent_downloads: usize,
}

impl VerifyCoordinator {
    pub fn new(
        storage: Arc<dyn BackupStorage>,
        metadata_cache_opt: MetadataCacheOpt,
        trusted_waypoints_opt: TrustedWaypointOpt,
        concurrent_downloads: usize,
    ) -> Result<Self> {
        Ok(Self {
            storage,
            metadata_cache_opt,
            trusted_waypoints_opt,
            concurrent_downloads,
        })
    }

    pub async fn run(self) -> Result<()> {
        info!("Verify coordinator started.");
        VERIFY_COORDINATOR_START_TS.set(unix_timestamp_sec());

        let ret = self.run_impl().await;

        if let Err(e) = &ret {
            error!(
                error = ?e,
                "Verify coordinator failed."
            );
            VERIFY_COORDINATOR_FAIL_TS.set(unix_timestamp_sec());
        } else {
            info!("Verify coordinator exiting with success.");
            VERIFY_COORDINATOR_SUCC_TS.set(unix_timestamp_sec());
        }

        ret
    }

    async fn run_impl(self) -> Result<()> {
        let metadata_view = metadata::cache::sync_and_load(
            &self.metadata_cache_opt,
            Arc::clone(&self.storage),
            self.concurrent_downloads,
        )
        .await?;
        let ver_max = Version::max_value();
        let state_snapshot = metadata_view.select_state_snapshot(ver_max)?;
        let transactions = metadata_view.select_transaction_backups(0, ver_max)?;
        let epoch_endings = metadata_view.select_epoch_ending_backups(ver_max)?;

        let global_opt = GlobalRestoreOptions {
            target_version: ver_max,
            trusted_waypoints: Arc::new(self.trusted_waypoints_opt.verify()?),
            run_mode: Arc::new(RestoreRunMode::Verify),
            concurrent_downloads: self.concurrent_downloads,
            replay_concurrency_level: 0, // won't replay, doesn't matter
        };

        let epoch_history = Arc::new(
            EpochHistoryRestoreController::new(
                epoch_endings
                    .into_iter()
                    .map(|backup| backup.manifest)
                    .collect(),
                global_opt.clone(),
                self.storage.clone(),
            )
            .run()
            .await?,
        );

        if let Some(backup) = state_snapshot {
            StateSnapshotRestoreController::new(
                StateSnapshotRestoreOpt {
                    manifest_handle: backup.manifest,
                    version: backup.version,
                    validate_modules: false,
                },
                global_opt.clone(),
                Arc::clone(&self.storage),
                Some(Arc::clone(&epoch_history)),
            )
            .run()
            .await?;
        }

        let txn_manifests = transactions.into_iter().map(|b| b.manifest).collect();
        TransactionRestoreBatchController::new(
            global_opt,
            self.storage,
            txn_manifests,
            None, /* replay_from_version */
            Some(epoch_history),
            VerifyExecutionMode::NoVerify,
        )
        .run()
        .await?;

        Ok(())
    }
}
