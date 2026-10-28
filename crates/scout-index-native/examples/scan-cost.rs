//! Separates metadata, decoding and byte-search cost on a real native index.

use std::hint::black_box;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;

use scout_core::ports::{IndexBackend, Shard};
use scout_index_native::{ContentCodec, LookupHome, NativeBackend};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: scan-cost <shards-directory> <threads> <runs>".into());
    }
    let threads: usize = args[2].parse()?;
    let runs: usize = args[3].parse()?;
    if threads == 0 || runs == 0 {
        return Err("threads and runs must be positive".into());
    }
    let backend = NativeBackend::new(LookupHome::Map, ContentCodec::Raw);
    let mut paths: Vec<PathBuf> =
        std::fs::read_dir(&args[1])?.map(|entry| entry.map(|entry| entry.path())).collect::<std::io::Result<_>>()?;
    paths.retain(|path| path.extension().is_some_and(|ext| ext == "native"));
    paths.sort();
    if paths.is_empty() {
        return Err("no native shards".into());
    }
    let shards: Vec<Box<dyn Shard>> = paths.iter().map(|path| backend.open(path)).collect::<scout_core::Result<_>>()?;
    let mut out = std::io::stdout().lock();
    for mode in ["metadata", "decode", "decode-newlines", "decode-if", "decode-date-unicode", "decode-date-ascii"] {
        for run in 0..=runs {
            let next = AtomicUsize::new(0);
            let started = Instant::now();
            let totals = std::thread::scope(|scope| {
                let workers: Vec<_> = (0..threads)
                    .map(|_| {
                        let (shards, next) = (&shards, &next);
                        scope.spawn(move || -> scout_core::Result<(u64, u64)> {
                            let mut buf = Vec::new();
                            let (mut bytes, mut hits) = (0, 0);
                            let finder = memchr::memmem::Finder::new(b"if");
                            let regex = RegexMatcherBuilder::new()
                                .multi_line(true)
                                .line_terminator(Some(b'\n'))
                                .build(if mode == "decode-date-ascii" {
                                    r"(?-u:\d{4}-\d{2}-\d{2})"
                                } else {
                                    r"\d{4}-\d{2}-\d{2}"
                                })
                                .map_err(|e| scout_core::Error::Pattern(e.to_string()))?;
                            loop {
                                let index = next.fetch_add(1, Ordering::Relaxed);
                                let Some(shard) = shards.get(index) else { break };
                                for doc in 0..shard.doc_count() {
                                    if mode == "metadata" {
                                        let meta = shard.meta(doc)?;
                                        bytes += meta.size;
                                        hits += meta.path.len() as u64;
                                        black_box(meta);
                                    } else {
                                        let content = shard.content(doc, &mut buf)?;
                                        bytes += content.len() as u64;
                                        hits += match mode {
                                            "decode-newlines" => memchr::memchr_iter(b'\n', content).count() as u64,
                                            "decode-if" => finder.find_iter(content).count() as u64,
                                            "decode-date-unicode" | "decode-date-ascii" => {
                                                let mut count = 0;
                                                regex
                                                    .find_iter(content, |_| {
                                                        count += 1;
                                                        true
                                                    })
                                                    .map_err(|e| scout_core::Error::Invalid(e.to_string()))?;
                                                count
                                            }
                                            _ => 0,
                                        };
                                        black_box(content);
                                    }
                                }
                            }
                            Ok((bytes, hits))
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .map(|worker| worker.join().expect("scan worker panicked"))
                    .collect::<scout_core::Result<Vec<_>>>()
            })?;
            let bytes: u64 = totals.iter().map(|t| t.0).sum();
            let hits: u64 = totals.iter().map(|t| t.1).sum();
            if run > 0 {
                writeln!(
                    out,
                    "mode={mode} run={run} elapsed_ms={:.3} bytes={bytes} hits={hits}",
                    started.elapsed().as_secs_f64() * 1000.0
                )?;
                out.flush()?;
            }
        }
    }
    Ok(())
}
