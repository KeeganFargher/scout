# Background warm-up: how it competes with queries, and what to try next

Since written: E48 ran and was rejected (its fixed clauses sat inside the harness's noise),
and E49 (E48 reading backward again) was kept against an A/A run's noise; see
[experiments.md](../experiments.md), E48 and E49. The text below predates both.

Research notes, not experiments. Nothing here was run on the benchmark box. Written
2026-10-29. The measurements discussed are E47b's (plain `read` of each shard **backward**
in 256 KiB chunks, only after 100 ms with no search, polling every 5 ms while busy) and
its diagnosis run (`bench/results/E47b-warmup-confirm/diag-0-300`). The tree has since
moved to E48 (results pending): the walk waits on a condvar instead of polling, reads
shards **forward**, and skips shards that a previous walk read. Where E48 changes an
argument below, the text says so. Kernel claims were checked against v6.12 source (the test box runs the WSL2 6.6.y
kernel; the code paths quoted are the same in 6.6 as far as I can tell, but that is not
verified line by line). Each claim is marked **fact** (primary source quoted or linked) or
**inference** (my reasoning; test it before relying on it).

## The finding to lead with: the target box's real disk cap is lower than the emulator's

**Fact.** A 2 vCPU / 4 GiB EC2 instance has its own EBS cap, and it is lower than gp3's
volume cap ([AWS general-purpose instance specs](https://docs.aws.amazon.com/ec2/latest/instancetypes/gp.html),
[compute-optimized](https://docs.aws.amazon.com/ec2/latest/instancetypes/co.html)):

| instance              | EBS throughput MB/s, baseline / max | EBS IOPS, baseline / max |
| --------------------- | ----------------------------------- | ------------------------ |
| t3.medium, t4g.medium | 43.38 / 260.62                      | 2,000 / 11,800           |
| t3a.medium            | 43.75 / 260.62                      | 2,000 / 11,800           |
| c8g.large (4 GiB)     | 78.75 / 1,250                       | 3,600 / 40,000           |
| c8a.large (4 GiB)     | 81.25 / 1,250                       | 3,600 / 40,000           |

The footnote on these rows says they "can support maximum performance for 30 minutes at
least once every 24 hours, after which they revert to their baseline performance". gp3 is
125 MiB/s and 3,000 IOPS with no burst
([gp3 docs](https://docs.aws.amazon.com/ebs/latest/userguide/general-purpose.html)). The
two caps stack: the NVMe stats count time over each one separately
(`ebs_volume_performance_exceeded_tp` and `ec2_instance_ebs_performance_exceeded_tp`,
[detailed performance stats](https://docs.aws.amazon.com/ebs/latest/userguide/nvme-detailed-performance-stats.html)).

**Inference.** Consequences for scout:

- Right after boot, the instance probably has its burst allowance, so the walk can run near
  gp3's 125 MiB/s (~21 s for 2.5 GiB). Once the burst is spent (a second restart the same
  day, or 30 min of heavy cold queries), a t3/t4g.medium reads at ~43 MB/s: the walk takes
  ~60 s, and any foreground I/O shares 43 MB/s with it. The E47 clause "45 s after ready ≤
  0.50" assumed a rate the baseline box does not have.
- `ebs-run.sh 2 4G 125 3000` (which sets 125×10⁶ B/s, ≈119 MiB/s, a little under gp3's
  125 MiB/s) models a c8g/c8a.large or a t-class with burst left, not a t-class at
  baseline. A second profile, `ebs-run.sh 2 4G 43 2000`, is the worst case.
- t3/t4g are also burstable on **CPU** (credits, a fraction of each vCPU as baseline). The
  walk's CPU work (page-cache insertion, the copy) spends credits on exactly that box.
  That is another reason to skip the copy for chunks that are already cached (section 3).

## 1. How the caps schedule a small read behind a background reader

### blk-throttle (`io.max`), which is what the emulator uses

**Fact** (block/blk-throttle.c, v6.12):

- Bios are queued per cgroup and per direction, in order. `tg_within_limit()`:
  `/* throtl is FIFO - if bios are already queued, should queue */`. A query's read that
  would fit the budget on its own still waits if one of the walk's bios is queued ahead of
  it in the same cgroup. Across cgroups, queued bios are taken round-robin by qnode
  (blk-throttle.h: "dispatched in round-robin order"), but the server and its walk thread
  are one cgroup.
- The budget is granted per slice. `tg_within_bps_limit()` allows
  `bps_limit * roundup(elapsed, throtl_slice) / HZ` bytes, plus carryover; a bio over that
  waits `extra_bytes * HZ / bps_limit` jiffies **plus the rest of the current slice**
  (`jiffy_wait + (jiffy_elapsed_rnd - jiffy_elapsed)`), and at least 1 jiffy.
- The slice is `DFL_THROTL_SLICE_SSD = HZ/50` (20 ms) when the queue is non-rotational,
  else `DFL_THROTL_SLICE_HD = HZ/10` (100 ms) (`blk_throtl_init`).
- The fast path: with nothing queued and the bio within budget, it is charged and passes
  straight through (`__blk_throtl_bio`). Nothing in blk-throttle reads the I/O priority
  (no `ioprio` reference in the file).
- The WSL2 kernel is `CONFIG_HZ=250`
  ([config-wsl, 6.6.y](https://github.com/microsoft/WSL2-Linux-Kernel/blob/linux-msft-wsl-6.6.y/arch/x86/configs/config-wsl)),
  so 1 jiffy is 4 ms, and a slice is 20 ms or 100 ms.

**Inference**, what this does to scout on the emulator:

- With dm-delay at 0 the WSL disk serves a 256 KiB read in well under 1 ms. `run-e47b.sh`
  does not set the delay; the box keeps the last value `ebs-setup.sh` was given, so check
  it. At low delay the walk does not run at 125 MB/s: it runs at disk speed until the slice's budget (2.5 MB in a 20 ms
  slice, 12.5 MB in a 100 ms one) is gone, then its next bio queues. A query bio that
  arrives then joins the FIFO behind it and waits to the next slice boundary: a few ms up to
  a whole slice, never less than 4 ms. That fits E47b's 300 ms-spaced pass: `literal_none`
  +6.5 ms, `llvm_regex` +16 ms, `selective_symbol` +26 ms are one to a few jiffies or one
  slice. `literal_none` is not I/O-free cold (17.8 ms at ready against 9.3 ms after the
  walk), and the pass's "MiB read" column truncates (`/ 1048576`), so sub-MiB reads show
  as 0. That column counts the **whole cgroup's** reads, the walk's included. In the 0:300
  pass the on-arm read 2 MiB during `llvm_regex` and `selective_long` and 1 MiB during
  `selective_symbol`, where the off-arm read 0. That is direct evidence the walk's I/O
  overlapped those queries, possibly in the window between the request arriving and
  `activity.start()`.
- The idle gate (E47 `busy()`, E48 `idle()`) is checked once per chunk, so a chunk already submitted when a query
  starts stays in the queue; the gate cannot take it back.
- **Which slice the emulator gets is not settled here**: it depends on whether
  `/sys/block/dm-*/queue/rotational` is 0 (dm copies the flag from the loop device, which
  copies it from the backing disk). Check it on the box: a 100 ms slice makes the waits
  five times as long as a 20 ms one.
- blk-throttle's slice-and-jiffy granularity is an emulator artifact. It makes the
  emulator a pessimistic model of how long one small read waits behind the walk.

### Real gp3 / Nitro NVMe

**Fact.** AWS documents caps, not the pacing algorithm. EBS counts SSD I/O in units of up
to 256 KiB, merges sequential small I/Os and splits larger ones; "Consistently driving more
IOPS to a volume than it has available can cause increased I/O latency"; for consistency it
recommends an average queue depth of 1 per 1,000 provisioned IOPS
([I/O characteristics](https://docs.aws.amazon.com/ebs/latest/userguide/ebs-io-characteristics.html)).
gp3 promises "single-digit millisecond latency" and provisioned performance "99 percent of
the time".

**Weak / unknown.** No primary source says how finely EBS meters the cap (per ms? per
second?) or whether a small read jumps the queue when a large one is pending. The only
measurement tool is on the instance: `nvme amzn stats /dev/nvme0n1` (or the `ebsnvme`
script) gives the time spent over each cap in microseconds, `volume_queue_length`, and read
latency histograms. Reading these before and after a warm-up, with and without small
queries running, is how to find out whether the walk pushes a query over the cap on real
EBS.

**Inference.** E47b read backward, which defeats readahead (each `read` looks random), so
it had at most one synchronous 256 KiB read (1–2 bios) in flight. **E48 reads forward**,
which triggers async readahead: the kernel can keep up to another `read_ahead_kb` (128 KiB)
in flight beyond the read that blocks, and that I/O is still in flight after the idle gate
trips. That is small, but it should be measured, not assumed. `POSIX_FADV_RANDOM` on the
shard fd turns readahead off if it shows up. At 125 MiB/s the walk alone fills the cap, so any
foreground read pushes total demand over it, and something waits. The only fix that works
on every layer is to keep the walk's demand under the cap with room to spare.

## 2. I/O priorities and the cgroup io controllers

**Fact:**

- `ioprio_set` classes are "io scheduler dependent and currently supported by bfq and
  mq-deadline" ([ioprio docs](https://docs.kernel.org/block/ioprio.html)). `none` ignores
  them. mq-deadline keeps three priority queues (RT/BE/IDLE) and dispatches a
  lower-priority request ahead once it has waited `prio_aging_expire = 10 * HZ` (10 s)
  (block/mq-deadline.c). An unprivileged process may set IDLE class for its own threads
  (ioprio_set(2), from the man page; not re-fetched for this note).
- The default elevator is `none` for any device with more than one hardware queue
  (`elevator_get_default`: `if (q->nr_hw_queues != 1 && !blk_mq_is_shared_tags(...)) return
  NULL;`). EBS on Nitro is NVMe; whether a 2 vCPU instance gets more than one queue is
  **not verified**: check `cat /sys/block/nvme1n1/queue/scheduler` and `ls
  /sys/block/nvme1n1/mq`.
- The order of the layers is: blk-throttle at bio submission, then rq-qos (`io.latency`,
  `io.cost`), then the elevator, then the driver, then EBS. A bio stuck in the throttle FIFO
  cannot be reordered by anything after it.
- `io.latency` throttles _peer_ cgroups that miss their target ("The limits are only
  applied at the peer level in the hierarchy"). `io.cost.qos` and `io.cost.model` "exist
  only on the root cgroup". `io.weight` needs iocost or BFQ. `io.prio.class` needs
  `CONFIG_BLK_CGROUP_IOPRIO` (unset in the WSL config; IOCOST is unset there too, IOLATENCY
  is set) ([cgroup-v2](https://docs.kernel.org/admin-guide/cgroup-v2.html)).
- **The io controller is not threaded**: "the following controllers are threaded and can be
  enabled in a threaded cgroup: cpu, cpuset, perf_event, pids". One thread of a process
  cannot have its own io limits.

**Inference:**

- None of these can rank the walk below a query inside one process without root. On EBS the
  queue that matters is remote, so host priorities can only help by limiting how many
  requests are outstanding, and the walk already has at most ~1. IDLE ioprio is free to set,
  and does nothing under `none` or blk-throttle; under mq-deadline it would help only when
  requests are queued in the host at the same moment.
- The cgroup route would need the walk in a **separate process** in a sibling cgroup (for
  example a systemd unit with `IOReadBandwidthMax=`, or `IODeviceLatencyTargetSec=` on the
  server). That is deploy-time admin setup, not something the server can do itself. Cost: page
  cache is charged to the cgroup that read it in ("A memory area is charged to the cgroup
  which instantiated it"), so the server's memory budget would no longer count the warmed
  pages.
- What does work per thread, unprivileged, on any stock kernel: the **CPU** scheduling
  class (section 5).

## 3. Alternatives to `read()` into a buffer

**Fact** (v6.12):

- `posix_fadvise(WILLNEED)`, `readahead(2)` and `madvise(MADV_WILLNEED)` on a file mapping all
  end in `force_page_cache_ra()` (mm/fadvise.c → mm/internal.h `force_page_cache_readahead`;
  `ksys_readahead` calls `vfs_fadvise(..., POSIX_FADV_WILLNEED)`; `madvise_willneed` drops
  `mmap_lock` around `vfs_fadvise`). They submit reads and return without copying.
  `force_page_cache_ra` **clamps each call** to `max(bdi->io_pages, ra->ra_pages)` pages
  (`nr_to_read = min_t(unsigned long, nr_to_read, max_pages)`), in 2 MiB sub-chunks. With
  `read_ahead_kb` 128, one call reads 128 KiB or `max_sectors_kb`, whichever is larger, not
  the whole range asked for.
- `MADV_POPULATE_READ` (5.14+) faults the range into the page tables through
  `faultin_page_range()`, entered with `mmap_lock` held for read (mm/gup.c: "mm->mmap_lock
  must be held"; `madvise_need_mmap_write` returns 0 for it). It passes `FOLL_UNLOCKABLE`,
  so a fault may drop the lock while it waits for I/O, and `madvise_populate` takes it back.
- PostgreSQL's `pg_prewarm` offers `prefetch` (asynchronous OS prefetch), `read`
  (synchronous) and `buffer` ([pg_prewarm](https://www.postgresql.org/docs/current/pgprewarm.html)).

**Inference:**

- fadvise/readahead save the 256 KiB copy (~25–50 µs a chunk, ~1–2% of a CPU at 125 MB/s)
  and nothing else: page allocation, page-cache insertion, LRU add and memcg charging are
  the same for every method. They also lose the completion signal, so the walk could no
  longer pace itself by waiting for its own reads. It would need a timer or
  `cachestat(2)`/`mincore` to see what landed, and a loop that issues calls faster than the
  disk can serve them builds a queue in blk-throttle (or at EBS) that a query must wait
  behind. **Keep the synchronous read.** It is the simplest bounded-in-flight design.
- `MADV_POPULATE_READ` on the shared 2.5 GiB mapping takes `mmap_lock` (read) again and
  again for the page-table work. It can drop the lock during I/O waits, so the cost is
  smaller than holding it throughout. Still, every retake competes with `mmap_lock`
  writers: the allocator growing or trimming its heap with mmap/munmap, a new thread
  stack. It would also fill the server's page tables for all 2.5 GiB and needs `unsafe`
  pointer ranges. It gains nothing over `read`. **Avoid it.**
- io_uring adds no new page-cache mechanism here. Buffered reads go through the same path
  (or io-wq threads). Skip it.
- A real saving: **skip chunks that are already cached.** The walk currently reads (copies)
  every chunk, including those the queries already brought in. `preadv2(..., RWF_NOWAIT)`
  returns `EAGAIN` when the data is not cached, and `cachestat(2)` (6.5+) counts cached pages
  in a range without reading them. Both cut CPU time, not disk time.
- Lock or LRU contention slowing a **cached** query: weak. A minor fault on a cached page
  looks the page up under RCU and does not take the LRU lock. The walk's page-cache insertion
  takes the mapping's xarray lock and per-CPU LRU batches. I found no primary source showing
  this hurting a concurrent reader at this rate, so treat it as a hypothesis of last resort.

## 4. How other systems warm after a restart

| system                                                                | how                                                                                                                                                                                                                                                                                                 | throttle                                                                                                                                                                                                                                                                                                                                                                                                           | idle detection                                                                                                                                                                                                |
| --------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| MySQL InnoDB buffer pool load                                         | background thread reloads the saved page list ("does not delay database startup")                                                                                                                                                                                                                   | `buf_load_throttle_if_needed()` (storage/innobase/buf/buf0dump.cc): every `innodb_io_capacity` I/Os, if `srv_get_activity_count()` changed, sleep the rest of the second since the last check. That gives roughly `io_capacity` IOPS (default 200, ~3 MB/s at 16 KiB), but in bursts: up to `io_capacity` I/Os back to back, and the first check after activity starts does not sleep (the code comment admits it) | **rate-based**: unthrottled while the server does nothing else, throttled to `io_capacity` as soon as there is any activity ([docs](https://dev.mysql.com/doc/refman/8.4/en/innodb-preload-buffer-pool.html)) |
| PostgreSQL autoprewarm                                                | 2 background workers reload the dumped block list, sorted (`qsort` in contrib/pg_prewarm/autoprewarm.c)                                                                                                                                                                                             | none; stops when `have_free_buffer()` is false                                                                                                                                                                                                                                                                                                                                                                     | none                                                                                                                                                                                                          |
| RocksDB `RateLimiter` (compaction/flush, not warm-up)                 | token bucket                                                                                                                                                                                                                                                                                        | `refill_period_us` 100 ms by default; high-priority requests go first, low-priority ones get a turn 1 in `fairness` (10); `auto_tuned` moves the rate within a range (include/rocksdb/rate_limiter.h)                                                                                                                                                                                                              | priority, not idleness                                                                                                                                                                                        |
| Lucene `MMapDirectory.setPreload` / ES `index.store.preload`          | madvise-based load when a file is opened, best effort (MMapDirectory javadoc). ES: files "only become available after data have been loaded", "may slow down the opening of indices" ([ES](https://www.elastic.co/guide/en/elasticsearch/reference/current/preload-data-to-file-system-cache.html)) | none (blocking, at open)                                                                                                                                                                                                                                                                                                                                                                                           | none                                                                                                                                                                                                          |
| ClickHouse `prewarm_mark_cache` / `prewarm_primary_key_cache` (24.12) | fills its own mark / PK caches on insert, merge, fetch and server start ([settings](https://clickhouse.com/docs/reference/settings/merge-tree-settings/prewarm))                                                                                                                                    | not documented (weak)                                                                                                                                                                                                                                                                                                                                                                                              | —                                                                                                                                                                                                             |
| Zoekt (Sourcegraph)                                                   | `unix.Mmap(..., PROT_READ, MAP_SHARED)` with no madvise or warm-up (index/indexfile.go)                                                                                                                                                                                                             | —                                                                                                                                                                                                                                                                                                                                                                                                                  | —                                                                                                                                                                                                             |

**Inference.** The nearest precedent to scout's problem is InnoDB: a cheap activity counter
plus a rate cap that applies only while there is activity. Scout's idle gate does the
first part. What scout lacks is a rate cap. Nobody in this set warms at full disk speed
alongside live queries; the ones that warm at full speed (ES, Lucene) do it before serving.

## 5. CPU-side reasons a tiny cached query could be slower

What the repo's own runs already say (**fact** from the results files, **inference** where
marked):

- In the back-to-back pass at ready the walk read nothing, and small queries still spread
  0.76–1.16x between arms (2 reps). On sub-100 ms queries that is the harness's noise floor,
  and it is about as large as the 1.06–1.4x effect.
- The "steady warm" numbers are p50s of **3** qbench rounds (`warmup-ebs.sh` runs `qbench.py
  ... warm 3`). A median of 3 samples of a 1.4 ms query cannot resolve 6%. The repo rule is
  `--runs 100` for p99. The same scenario gave steady 1.037 in E47b and 1.006 in the
  diagnosis run.
- The diagnosis run (`diag-0-300`, experiments.md) captured `/status` after qbench's rounds.
  The walk read 50–107 MiB during ~9 s of rounds, far less than it could. `literal_none`'s
  steady ratio follows whether the walk is **alive**, not how much it reads: 1.06 at 0:0
  (walk had read 256 KiB), 1.106 and 1.425 at 0:300, 0.823 at 45:0 (walk finished).
  **Inference:** that points at something the live walk does while it is not reading, which
  in E47b was the 5 ms poll. E48 replaced the poll with a condvar, so E48's steady numbers
  test this directly. If E48 still shows the effect, the bursts (1) and preemption (3) below
  are next.

Hypotheses for the 2 pinned vCPUs (search uses a thread pool, so a 1.4 ms query ends when
its slowest worker ends; anything that takes a CPU for 100 µs delays it):

1. **CPU bursts from the walk on the emulator** (inference). Under blk-throttle the walk does
   a slice's worth of chunks at disk speed, then nothing. During the burst it keeps one
   vCPU busy with page allocation, insertion, copying and completions. A query that lands in
   the burst has ~1.x CPUs. On real EBS the same work is spread at one chunk per ~2 ms. Test:
   `/proc/<pid>/task/<tid>/schedstat` field 2 (run-queue wait) for the search workers,
   summed per query, walk on against walk off.
2. **5 ms polling** (inference; E48 removed it, so E48's results test it). 200 timer
   wakeups a second; in a VM each one can be a vCPU exit and IPI. A naive estimate (~0.3
   wakeups per 1.4 ms query) says it should be small, but the diagnosis run's "alive, not
   reading" pattern points here more than anywhere else.
3. **Wakeup preemption by the walk thread** (fact about the scheduler, inference about the
   effect). Under EEVDF a woken normal task can preempt a running one. kernel/sched/fair.c
   `check_preempt_wakeup_fair`: "Preempt an idle entity in favor of a non-idle entity (and
   don't preempt in the inverse case)": a `SCHED_IDLE` walk thread never preempts a query
   worker, and a woken worker gets the CPU at the walk's next preemption point. sched(7) (man page, not re-fetched): an unprivileged thread may switch
   itself to `SCHED_IDLE`. The WSL config is `PREEMPT_NONE` (dynamic), so a walk thread
   inside `read()` yields only at `cond_resched` points (per folio batch in `filemap_read`).
4. **Cache pollution** (inference, weak). Each chunk streams 256 KiB of page cache through
   the core into a 256 KiB buffer: 512 KiB through L2. Test: `perf stat -e
   LLC-load-misses` on the workers (WSL may not expose PMUs).
5. **Frequency scaling**: not under the guest's control in a VM (EC2 or WSL). Not
   testable from the guest, so unlikely to be worth pursuing.

## Recommendations (each a testable experiment)

1. **Read E48's results first, then attribute what is left.** E48 tests the polling
   hypothesis. If any steady or spaced clause still fails, run a factorial A/B in the spaced
   and steady scenarios, `--runs 100`: (a) walk off; (b) walk alive but never reading
   (idle gate always busy); (c) walk reading files already in cache (CPU cost, no I/O);
   (d) the real walk, with `io.max` lifted; (e) the real walk. Record the search workers'
   schedstat run-delay, `io.stat` deltas per query, `/status` before and after the rounds,
   `queue/rotational` and the dm-delay of the volume. (b) isolates being alive, (c) CPU, (d)
   against (e) blk-throttle queueing. Set the keep/reject clauses before the run, as
   E45–E48 did.
2. **Pace the walk below the cap, in user space.** A token bucket at about half of
   min(volume, instance) baseline (e.g. 60 MB/s on the emulator profile, ~20 MB/s for a
   t-class at baseline), one 256 KiB chunk per tick, keeping the idle gate. Add an
   InnoDB-style twist: run at the cap while idle, throttle as soon as there is activity. Or
   add an adaptive step: if a chunk takes more than ~2x its unloaded time, halve the rate,
   then grow it back slowly. Mechanism (fact + inference): demand under the cap keeps
   blk-throttle on its fast path, so a query bio never queues behind the walk; on EBS it
   keeps `*_performance_exceeded_tp` at 0; and it spreads the walk's CPU work evenly instead
   of in slice-sized bursts. Cost: ~42 s instead of ~21 s to warm at 60 MB/s; the 45 s
   clause must be restated for the real box (~60 s at a t-class baseline even unpaced).
   Also run the emulator at `43 2000`.
3. **Make the walk thread cheap to preempt.** `sched_setattr(SCHED_IDLE)` on the walk
   thread (unprivileged, stock kernels): a waking query worker then always preempts it at
   its next preemption point, and it never preempts a worker. E48 already has the condvar
   wait and skips whole shards that an earlier walk read. Still open: skip already-cached
   **chunks** with `preadv2(RWF_NOWAIT)` or `cachestat(2)`, which saves copies and t-class
   CPU credits. Setting `IOPRIO_CLASS_IDLE` costs one syscall, but it does nothing under
   `none` or blk-throttle; it matters only under mq-deadline/BFQ. Accepted cost: under
   sustained CPU load a `SCHED_IDLE` walk may starve, as the idle gate already lets it.

## Sources

- Repo: `docs/experiments.md` (E47b diagnosis, E48 entry), `bench/results/E47b-warmup-confirm/diag-0-300`.
- Kernel v6.12: block/blk-throttle.c, block/blk-throttle.h, block/mq-deadline.c,
  block/elevator.c, mm/fadvise.c, mm/readahead.c, mm/internal.h, mm/madvise.c, mm/gup.c,
  kernel/sched/fair.c (github.com/torvalds/linux, tag v6.12).
- [cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html),
  [ioprio](https://docs.kernel.org/block/ioprio.html).
- [WSL2 kernel config 6.6.y](https://github.com/microsoft/WSL2-Linux-Kernel/blob/linux-msft-wsl-6.6.y/arch/x86/configs/config-wsl).
- AWS: [gp3](https://docs.aws.amazon.com/ebs/latest/userguide/general-purpose.html),
  [I/O characteristics](https://docs.aws.amazon.com/ebs/latest/userguide/ebs-io-characteristics.html),
  [NVMe detailed stats](https://docs.aws.amazon.com/ebs/latest/userguide/nvme-detailed-performance-stats.html),
  [general-purpose instance EBS specs](https://docs.aws.amazon.com/ec2/latest/instancetypes/gp.html),
  [compute-optimized](https://docs.aws.amazon.com/ec2/latest/instancetypes/co.html).
- MySQL 8.4 storage/innobase/buf/buf0dump.cc and
  [buffer pool preload](https://dev.mysql.com/doc/refman/8.4/en/innodb-preload-buffer-pool.html);
  PostgreSQL REL_17 contrib/pg_prewarm/autoprewarm.c and
  [pg_prewarm](https://www.postgresql.org/docs/current/pgprewarm.html);
  RocksDB include/rocksdb/rate_limiter.h; Lucene MMapDirectory.java;
  [Elasticsearch preload](https://www.elastic.co/guide/en/elasticsearch/reference/current/preload-data-to-file-system-cache.html);
  [ClickHouse prewarm settings](https://clickhouse.com/docs/reference/settings/merge-tree-settings/prewarm);
  sourcegraph/zoekt index/indexfile.go.
- Not verified from a primary source here: EBS's internal metering granularity and queue
  order; the JDK's `MemorySegment.load()` semantics behind Lucene preload; the 6.6 code
  matching 6.12 line for line; EC2 NVMe queue count on 2 vCPUs.
