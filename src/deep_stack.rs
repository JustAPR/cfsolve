//! Runs recursive work on a dedicated worker pool with large stacks, independent of the caller's runtime.

use std::sync::LazyLock;

use anyhow::{Context, Result};
use tokio::sync::oneshot;

/// Stack size for recursive deobfuscation, matching the usual Linux main-thread stack (8MB).
const STACK_SIZE: usize = 8 << 20;

static WORKER_POOL: LazyLock<rayon::ThreadPool> = LazyLock::new(|| {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    rayon::ThreadPoolBuilder::new()
        .thread_name(|i| format!("cfsolve-worker-{i}"))
        .stack_size(STACK_SIZE)
        .num_threads(threads)
        .build()
        .expect("Failed to initialize worker thread pool")
});

/// Runs `work` on the worker pool and awaits its result without blocking the runtime.
pub(crate) async fn run<T: Send + 'static>(
    name: &str,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T> {
    let (sender, receiver) = oneshot::channel();
    let name_owned = name.to_string();
    WORKER_POOL.spawn(move || {
        // The receiver is gone only if the caller stopped waiting.
        let _ = sender.send(work());
    });
    receiver
        .await
        .with_context(|| format!("the {name_owned} worker task failed"))
}
