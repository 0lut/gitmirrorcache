//! Randomized concurrency fuzz harness for gitmirrorcache.
//!
//! The fuzz targets live in `tests/` and hammer the crate's shared-state
//! surfaces (disk manager, update coordinator, materializer) with many
//! concurrent tasks performing randomized operation sequences. Every target
//! runs under a hard deadline so a deadlock or livelock fails the test
//! instead of hanging forever.
//!
//! Intensity is tunable via environment variables so CI stays fast while
//! local runs can be cranked up:
//!
//! - `GIT_CACHE_FUZZ_SEED`: RNG seed (default: derived from the clock, and
//!   always printed so failures are reproducible)
//! - `GIT_CACHE_FUZZ_TASKS`: concurrent tasks per target
//! - `GIT_CACHE_FUZZ_OPS`: operations per task
//! - `GIT_CACHE_FUZZ_DEADLINE_SECS`: per-target deadlock deadline

use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy)]
pub struct FuzzConfig {
    pub seed: u64,
    pub tasks: usize,
    pub ops_per_task: usize,
    pub deadline: Duration,
}

impl FuzzConfig {
    /// Read intensity knobs from the environment, falling back to the given
    /// CI-friendly defaults. The chosen seed is printed so any failure can be
    /// replayed with `GIT_CACHE_FUZZ_SEED=<seed>`.
    pub fn from_env(target: &str, default_tasks: usize, default_ops: usize) -> Self {
        let seed = env_u64("GIT_CACHE_FUZZ_SEED").unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x5eed)
        });
        let tasks = env_u64("GIT_CACHE_FUZZ_TASKS")
            .map(|v| v as usize)
            .unwrap_or(default_tasks)
            .max(1);
        let ops_per_task = env_u64("GIT_CACHE_FUZZ_OPS")
            .map(|v| v as usize)
            .unwrap_or(default_ops)
            .max(1);
        let deadline = Duration::from_secs(env_u64("GIT_CACHE_FUZZ_DEADLINE_SECS").unwrap_or(120));

        eprintln!(
            "[{target}] fuzz config: GIT_CACHE_FUZZ_SEED={seed} \
             GIT_CACHE_FUZZ_TASKS={tasks} GIT_CACHE_FUZZ_OPS={ops_per_task} \
             GIT_CACHE_FUZZ_DEADLINE_SECS={}",
            deadline.as_secs()
        );

        Self {
            seed,
            tasks,
            ops_per_task,
            deadline,
        }
    }

    /// Deterministic per-task RNG so concurrent tasks do not contend on a
    /// shared RNG while still being replayable from the top-level seed.
    pub fn task_rng(&self, task: usize) -> fastrand::Rng {
        fastrand::Rng::with_seed(
            self.seed
                .wrapping_add(0x9e37_79b9_7f4a_7c15u64.wrapping_mul(task as u64 + 1)),
        )
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}
