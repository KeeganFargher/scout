# Small-server harness

The Linux scripts behind the interleaved A/Bs (E16 on) and the cold/serve measurements
(E21 on). They run under WSL2 Ubuntu or any Linux with cgroup v2 and a systemd user
session, without root.

| script                          | what it does                                                                                                                                                                                                                                                                                                                                                                              |
| ------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `prof.sh <cpus> <mem> <cmd...>` | runs a command in a transient systemd scope: `CPUQuota`, `MemoryMax` with no swap, pinned to CPUs `0..cpus-1`; prints the scope's peak memory and OOM kills                                                                                                                                                                                                                               |
| `ab.sh`                         | interleaved A/B of two builds with `scout-bench` per profile (rounds alternate which arm goes first); an arm can name its own index (`arm=bin@index`, for a format change); `abagg.py` summarises it                                                                                                                                                                                      |
| `qbench.py`                     | times the bench queries one request at a time against `scout serve` or the CLI, warm or **cold**; cold evicts the index before every request and fails unless `mincore` shows at most 2 MiB of it resident and the request then takes a major fault. Records each request's major faults and MiB read from disk, and the index disk's read-ahead (see below; the run fails if it changes) |
| `pagecache.py`                  | `mincore` residency, `posix_fadvise` eviction and the read-ahead of a path's disk, for `qbench.py` and the shell scripts                                                                                                                                                                                                                                                                  |
| `run-e21.sh`                    | E21: CLI and serve, warm and cold, per profile, then the 1-CPU memory cliff; `e21sum.py` summarises it, including serve-vs-CLI result parity                                                                                                                                                                                                                                              |
| `serve-coldab.sh`               | interleaved A/B of two builds as `scout serve`, cold then warm, per profile; arms as in `ab.sh`; `coldabsum.py` summarises it and fails if its runs had different read-ahead (its parity line compares the two arms with each other; parity with rg is `scout-bench`'s)                                                                                                                   |
| `run-e24.sh`                    | E24: builds the v5 bench index and checks it (`v3v5same.py`, `filtercheck.py`), then the CLI warm, serve cold/warm, CLI cold and memory-cliff A/Bs against v3, and the CLI's map-vs-heap filter A/B. Arms that search different indexes are first warmed from the script's own cgroup (`warmpass.py`), so no profile scope is charged for either index                                    |
| `warmpass.py`                   | runs every bench query once per arm (`bin@index`), untimed, so the pages they need are cached and charged to the caller's cgroup                                                                                                                                                                                                                                                          |
| `v3v5same.py`                   | E24: checks a v5 index holds byte-identical shards to the v3 one but for the trigram filters, and that every shard's filter passes all of its own keys                                                                                                                                                                                                                                    |
| `filtercheck.py`                | E24: probes a v5 index's trigram filters per literal bench query with an independent copy of the hash, and checks `scout search --stats`'s `shards_filtered` against it                                                                                                                                                                                                                   |
| `shardfmt.py`                   | the v3 and v5 native shard layouts, and an independent copy of the filter hash and trigram packing, for the E24 scripts                                                                                                                                                                                                                                                                   |
| `bloomoracle.py`, `bloomsim.py` | E24's design runs on the v3 index: shards an exact or a Bloom filter would let through per literal query, by expected and by simulated false-positive rate                                                                                                                                                                                                                                |

## Expected layout

Scripts find each other through their own directory; data lives under `~/small`:

- `~/small/queries.tsv`: a copy of [`bench/queries.tsv`](../queries.tsv).
- `~/small/corpus/`: the bench repos mirrored onto the Linux disk (rg's parity runs read it).
- `~/small/bin/rg`: the rg binary parity is checked against.
- `~/small/<bin-dir>/{scout,scout-bench}`: one directory per build under test.
- `~/small/idx-e20-12cpu-infinity/`: the v3 index the A/Bs up to E23 search (built with
  `prof.sh 12 infinity scout --index-dir ... index`). `run-e21.sh` copies it to
  `~/small/idx-e21-serve/` and creates `~/small/tiny.git` and `~/small/serve-e21.toml`,
  which `serve-coldab.sh` reuses, because `serve` adds its mirrored repo to the index.
  `run-e24.sh` builds the v5 index `~/small/idx-e24-12cpu-infinity/` from the same repos
  and profile, and its primed serve copy `~/small/idx-e24-serve/`.
- Results go to `~/small/ab/<label>/`, `~/small/results/e21/` and
  `~/small/results/e24-index/`; the ones the experiment log cites are copied into
  [`bench/results/`](../results). Every script refuses to write into an output directory
  that already exists, because its summary reads every run in it: move an old one away to
  rerun.

Cold numbers depend on the read-ahead of the disk holding the index: a cold mmap fault
reads a window that size around the faulting page. The dev box's WSL2 virtual disks have
8 MiB (`/sys/block/sdX/queue/read_ahead_kb` = 8192), where a cloud VM's disk typically has
128 KiB, so a cold fault here reads up to 64 times as much (E24). Setting it needs root and
lasts until WSL restarts: `sudo sh -c 'echo 128 > /sys/block/sdc/queue/read_ahead_kb'`.

The serve benchmarks run the HTTP client on CPUs 8-11 (`taskset`), outside every profile's
CPU set, so they need a machine with 12 or more CPUs.
