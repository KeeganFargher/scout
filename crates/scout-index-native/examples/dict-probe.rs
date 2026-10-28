//! How small can content blocks be stored, and at what decode cost? Cold queries on gp3 are
//! bound by the bytes they read once E42 joined their reads, and the broad ones' candidates
//! nearly all match (`--stats`, E43), so only the stored size can shrink those bytes.
//! Blocks average under 9 KiB, too little history for lz4 to find much. This re-encodes
//! the blocks of sampled shards with lz4 HC 6 as the writer does, then: with every block
//! compressed, small ones too (E43), or those of 1 KiB and up; with a 64 KiB dictionary
//! trained (zstd's trainer) on all the sample's blocks or on each shard's; and with a
//! shard's dictionary used only for single-block docs. Decoding uses the reader's decoder
//! without a dictionary and lzzzz's (the same C library) with one. A raw block is timed as
//! a copy, which the reader does not pay (it borrows the map), so the arms that store fewer
//! blocks raw look cheaper to decode than they are.

use std::hint::black_box;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use scout_core::indexer::{UNIT_TARGET, unit_spans};
use scout_core::ports::IndexBackend;
use scout_index_native::{ContentCodec, LookupHome, NativeBackend};

/// The writer before E43: blocks under 4 KiB stayed raw.
const MIN_COMPRESSED: usize = 4096;
/// The writer's `LZ4_HC_LEVEL` (E38).
const LEVEL: i32 = 6;
/// lz4 refers back at most 64 KiB, so a larger dictionary is never read.
const DICT_BYTES: usize = 64 << 10;

/// A sampled block: its shard, whether it is its doc's only block, and its bytes.
struct Block {
    shard: usize,
    alone: bool,
    bytes: Vec<u8>,
}

/// A block as stored: raw (`None`) or lz4, with the dictionary it was encoded against.
struct Stored {
    lz4: Option<Vec<u8>>,
    dict: Option<usize>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: dict-probe <shards-directory> <shard-count>".into());
    }
    let take: usize = args[2].parse()?;
    if take == 0 {
        return Err("shard count must be positive".into());
    }
    let blocks = sample(&args[1], take)?;
    if blocks.is_empty() {
        return Err(format!("no blocks sampled under {}", args[1]).into());
    }
    let shards = blocks.last().map_or(0, |b| b.shard + 1);
    let raw: usize = blocks.iter().map(|b| b.bytes.len()).sum();
    let mut out = std::io::stdout().lock();
    writeln!(out, "shards={shards} blocks={} raw_mib={:.1}", blocks.len(), mib(raw))?;
    let small: usize = blocks.iter().filter(|b| b.bytes.len() < MIN_COMPRESSED).map(|b| b.bytes.len()).sum();
    let alone: usize = blocks.iter().filter(|b| b.alone).map(|b| b.bytes.len()).sum();
    writeln!(
        out,
        "under 4 KiB: {:.1}% of raw bytes; in single-block docs: {:.1}%",
        100.0 * small as f64 / raw as f64,
        100.0 * alone as f64 / raw as f64
    )?;

    // Without a dictionary: the writer before E43, then every block compressed (E43).
    for (name, min) in [("hc6", MIN_COMPRESSED), ("hc6+min1k", 1024), ("hc6+small", 1)] {
        let started = Instant::now();
        let stored = blocks.iter().map(|b| encode(&b.bytes, None, min)).collect::<Result<Vec<_>, _>>()?;
        report(&mut out, name, &blocks, &stored, &[], started.elapsed())?;
    }

    // One dictionary for the sample, trained on all of it: the most a shared one could do.
    let global = vec![train(&blocks.iter().map(|b| b.bytes.as_slice()).collect::<Vec<_>>())?];
    let started = Instant::now();
    let stored = blocks.iter().map(|b| encode(&b.bytes, Some((0, &global[0])), 1)).collect::<Result<Vec<_>, _>>()?;
    report(&mut out, "hc6+small+global", &blocks, &stored, &global, started.elapsed())?;

    // A dictionary per shard, trained on its first blocks only (up to `train_bytes`), as a
    // streaming writer could: on every block, then on single-block docs only. Decoding a
    // whole doc puts each block after the one before it, so only a doc's first block would
    // follow a dictionary placed before the buffer; single-block docs are the simplest
    // subset that always does (a reader decoding one block alone could place it before any).
    for train_bytes in [2usize << 20, 8 << 20] {
        let started = Instant::now();
        let dicts = (0..shards)
            .map(|s| {
                let mut total = 0;
                let head: Vec<&[u8]> = blocks
                    .iter()
                    .filter(|b| b.shard == s)
                    .take_while(|b| {
                        total += b.bytes.len();
                        total <= train_bytes
                    })
                    .map(|b| b.bytes.as_slice())
                    .collect();
                train(&head)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let train_time = started.elapsed();
        writeln!(out, "per-shard dictionaries ({}m): trained in {:.1} s", train_bytes >> 20, train_time.as_secs_f64())?;
        for alone_only in [false, true] {
            let started = Instant::now();
            let stored = blocks
                .iter()
                .map(|b| {
                    let dict = (b.alone || !alone_only).then(|| (b.shard, dicts[b.shard].as_slice()));
                    encode(&b.bytes, dict, 1)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let name = format!("hc6+small+shard{}m{}", train_bytes >> 20, if alone_only { "+alone" } else { "" });
            report(&mut out, &name, &blocks, &stored, &dicts, started.elapsed())?;
        }
    }
    Ok(())
}

/// Every block of up to `take` shards spread over the index, in shard and doc order.
fn sample(dir: &str, take: usize) -> Result<Vec<Block>, Box<dyn std::error::Error>> {
    let backend = NativeBackend::new(LookupHome::Map, ContentCodec::Raw);
    let mut paths: Vec<PathBuf> =
        std::fs::read_dir(dir)?.map(|entry| entry.map(|entry| entry.path())).collect::<std::io::Result<_>>()?;
    paths.retain(|path| path.extension().is_some_and(|ext| ext == "native"));
    paths.sort();
    let step = paths.len().div_ceil(take).max(1);
    let mut blocks = Vec::new();
    let mut buf = Vec::new();
    for (shard, path) in paths.iter().step_by(step).enumerate() {
        let opened = backend.open(path)?;
        for doc in 0..opened.doc_count() {
            let content = opened.content(doc, &mut buf)?;
            let spans = unit_spans(content, UNIT_TARGET);
            let alone = spans.len() == 1;
            blocks.extend(spans.into_iter().map(|s| Block { shard, alone, bytes: content[s].to_vec() }));
        }
    }
    Ok(blocks)
}

/// A 64 KiB dictionary from zstd's trainer. Its header (magic, id, entropy tables) is
/// noise to lz4, which reads the dictionary only as history; the trained content is at
/// the end, so all of it is in reach.
fn train(blocks: &[&[u8]]) -> std::io::Result<Vec<u8>> {
    zstd::dict::from_samples(blocks, DICT_BYTES)
}

/// Raw when under `min` or when lz4 does not shrink it, as the writer. A new HC stream per
/// block reloads the dictionary each time, so the dictionary arms' encode speed is a floor:
/// a writer would load it once per shard and attach it to each block's stream.
fn encode(b: &[u8], dict: Option<(usize, &[u8])>, min: usize) -> std::io::Result<Stored> {
    if b.len() < min {
        return Ok(Stored { lz4: None, dict: None });
    }
    let c = match dict {
        None => lz4::block::compress(b, Some(lz4::block::CompressionMode::HIGHCOMPRESSION(LEVEL)), false)?,
        Some((_, d)) => {
            let mut comp = lzzzz::lz4_hc::Compressor::with_dict(d, LEVEL)?;
            let mut c = Vec::with_capacity(lzzzz::lz4::max_compressed_size(b.len()));
            comp.next_to_vec(b, &mut c)?;
            c
        }
    };
    Ok(if c.len() < b.len() {
        Stored { lz4: Some(c), dict: dict.map(|(i, _)| i) }
    } else {
        Stored { lz4: None, dict: None }
    })
}

fn report(
    out: &mut impl Write,
    name: &str,
    blocks: &[Block],
    stored: &[Stored],
    dicts: &[Vec<u8>],
    encode_time: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let raw: usize = blocks.iter().map(|b| b.bytes.len()).sum();
    let size: usize = stored.iter().zip(blocks).map(|(s, b)| s.lz4.as_ref().map_or(b.bytes.len(), Vec::len)).sum();
    // The dictionary is copied right before the output, so lz4 decodes it as a prefix of
    // the block, its fast path, where a dictionary elsewhere in memory takes a slower one.
    // It is copied when the dictionary changes, as a reader would once per shard.
    let max = blocks.iter().map(|b| b.bytes.len()).max().unwrap_or(0);
    let mut dst_buf = vec![0u8; DICT_BYTES + max];
    // Pass 0 checks every block round-trips, untimed; the best of the other 5 is the time.
    let mut best = Duration::MAX;
    for pass in 0..6 {
        let mut loaded = usize::MAX;
        let started = Instant::now();
        for (s, b) in stored.iter().zip(blocks) {
            let len = b.bytes.len();
            if let Some(d) = s.dict
                && loaded != d
            {
                dst_buf[DICT_BYTES - dicts[d].len()..DICT_BYTES].copy_from_slice(&dicts[d]);
                loaded = d;
            }
            let (prefix, rest) = dst_buf.split_at_mut(DICT_BYTES);
            let dst = &mut rest[..len];
            match (&s.lz4, s.dict) {
                (Some(c), None) => {
                    assert_eq!(lz4::block::decompress_to_buffer(c, Some(i32::try_from(len)?), dst)?, len);
                }
                (Some(c), Some(d)) => {
                    assert_eq!(lzzzz::lz4::decompress_with_dict(c, dst, &prefix[DICT_BYTES - dicts[d].len()..])?, len);
                }
                (None, _) => dst.copy_from_slice(&b.bytes),
            }
            if pass == 0 {
                assert!(*dst == b.bytes[..], "{name} does not round-trip");
            }
            black_box(&dst_buf);
        }
        if pass > 0 {
            best = best.min(started.elapsed());
        }
    }
    writeln!(
        out,
        "{name} stored_mib={:.1} ratio={:.3} encode_mb_s={:.0} decode_ms={:.1} decode_gb_s={:.2}",
        mib(size),
        raw as f64 / size as f64,
        raw as f64 / 1e6 / encode_time.as_secs_f64(),
        best.as_secs_f64() * 1000.0,
        raw as f64 / 1e9 / best.as_secs_f64(),
    )?;
    Ok(())
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / f64::from(1 << 20)
}
