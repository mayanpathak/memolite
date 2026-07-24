//! M10 — background maintenance controller.
//!
//! Spawns a single background Tokio task that periodically calls
//! [`crate::engine::MemoryEngine::purge_expired`] and
//! [`crate::engine::MemoryEngine::compress_old_memories`] on independent
//! intervals, until explicitly cancelled via [`MaintenanceHandle::shutdown`].
//!
//! Design notes:
//! - The task holds only a [`std::sync::Weak`] reference to the engine, so a
//!   running maintenance loop never keeps an otherwise-unused engine alive.
//! - `MemoryEngine::maintenance_running` (an `Arc<AtomicBool>` field on the
//!   engine itself) guarantees at most one maintenance loop is active per
//!   engine at a time; `start_maintenance` atomically claims it via `swap`,
//!   and `shutdown` releases it.
//! - Errors from `purge_expired`/`compress_old_memories` are logged via
//!   `tracing::warn!` and never stop the loop — a single bad run should not
//!   permanently disable maintenance.
//! - No lock is ever held across an `.await` point in this module.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::time::{interval_at, Duration, Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::error::{MemoliteError, Result};

/// Configuration for a background maintenance loop.
///
/// Both intervals must be non-zero — [`crate::engine::MemoryEngine::start_maintenance`]
/// rejects a zero-duration interval with `Err(InvalidArgument)` before ever
/// spawning a task.
#[derive(Debug, Clone)]
pub struct MaintenanceConfig {
    /// How often expired memories are purged.
    pub purge_interval: Duration,
    /// How often old, low-importance episodic memories are compressed.
    pub compress_interval: Duration,
}

/// Handle to a running maintenance loop.
///
/// Dropping this handle without calling [`shutdown`](MaintenanceHandle::shutdown)
/// leaves the background task running — call `shutdown` to stop it
/// deterministically and free up `start_maintenance` to be called again.
pub struct MaintenanceHandle {
    cancel: CancellationToken,
    join: tokio::task::JoinHandle<()>,
    running_flag: Arc<AtomicBool>,
}

impl std::fmt::Debug for MaintenanceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaintenanceHandle")
            .field("running", &self.running_flag.load(Ordering::SeqCst))
            .finish()
    }
}

impl MaintenanceHandle {
    /// Cancels the background task, awaits its completion, and clears the
    /// engine's `maintenance_running` flag so a new loop can be started
    /// later.
    ///
    /// Returns `Err(MemoliteError::Internal(..))` only if the background
    /// task itself panicked (a `JoinError`) — a normal cancelled exit is
    /// always `Ok(())`.
    pub async fn shutdown(self) -> Result<()> {
        self.cancel.cancel();

        let result = self
            .join
            .await
            .map_err(|e| MemoliteError::Internal(format!("maintenance task panicked: {e}")));

        // Always clear the flag, even if the task panicked, so the engine
        // is never left permanently unable to start maintenance again.
        self.running_flag.store(false, Ordering::SeqCst);

        result
    }
}

/// Internal constructor used by `MemoryEngine::start_maintenance`. Not part
/// of the public API — callers only ever see a [`MaintenanceHandle`].
pub(crate) fn spawn_maintenance_task(
    cancel: CancellationToken,
    join: tokio::task::JoinHandle<()>,
    running_flag: Arc<AtomicBool>,
) -> MaintenanceHandle {
    MaintenanceHandle {
        cancel,
        join,
        running_flag,
    }
}

/// Builds the two paused-until-first-tick interval timers used by the
/// maintenance loop, with `MissedTickBehavior::Skip` so a slow tick never
/// causes a burst of catch-up ticks.
pub(crate) fn build_intervals(
    config: &MaintenanceConfig,
) -> (tokio::time::Interval, tokio::time::Interval) {
    let now = Instant::now();

    let mut purge_tick = interval_at(now + config.purge_interval, config.purge_interval);
    purge_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut compress_tick = interval_at(now + config.compress_interval, config.compress_interval);
    compress_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    (purge_tick, compress_tick)
}

/// Validates that both configured intervals are non-zero.
pub(crate) fn validate_config(config: &MaintenanceConfig) -> Result<()> {
    if config.purge_interval.is_zero() || config.compress_interval.is_zero() {
        return Err(MemoliteError::InvalidArgument(
            "maintenance intervals must be non-zero".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_purge_interval_is_rejected() {
        let config = MaintenanceConfig {
            purge_interval: Duration::from_secs(0),
            compress_interval: Duration::from_secs(60),
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn zero_compress_interval_is_rejected() {
        let config = MaintenanceConfig {
            purge_interval: Duration::from_secs(60),
            compress_interval: Duration::from_secs(0),
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn nonzero_intervals_are_accepted() {
        let config = MaintenanceConfig {
            purge_interval: Duration::from_secs(60),
            compress_interval: Duration::from_secs(3600),
        };
        assert!(validate_config(&config).is_ok());
    }
}