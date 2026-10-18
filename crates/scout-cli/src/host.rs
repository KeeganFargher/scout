//! Host introspection used to adapt to the machine: cores and usable memory.

use std::num::NonZeroUsize;

#[must_use]
pub fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

/// Memory this process may use, in bytes: physical RAM capped by a cgroup limit on Linux.
pub fn usable_memory() -> anyhow::Result<u64> {
    use anyhow::Context as _;
    let total = scout_source_fs::usable_memory().context("reading usable memory (pass --memory-mb to skip)")?;
    // A zero would plan a zero budget; the OS never reports it, so it means a broken read.
    anyhow::ensure!(total > 0, "the OS reported 0 bytes of usable memory");
    Ok(total)
}
