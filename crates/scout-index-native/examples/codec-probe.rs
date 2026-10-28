//! Compares lz4 encoders for content blocks on a real native index: stored size, encode
//! time and decode time with the reader's decoder, block by block as the writer cuts them.

use std::hint::black_box;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use scout_core::indexer::{UNIT_TARGET, unit_spans};
use scout_core::ports::IndexBackend;
use scout_index_native::{ContentCodec, LookupHome, NativeBackend};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: codec-probe <shards-directory> <shard-count>".into());
    }
    let take: usize = args[2].parse()?;
    if take == 0 {
        return Err("shard count must be positive".into());
    }
    let backend = NativeBackend::new(LookupHome::Map, ContentCodec::Raw);
    let mut paths: Vec<PathBuf> =
        std::fs::read_dir(&args[1])?.map(|entry| entry.map(|entry| entry.path())).collect::<std::io::Result<_>>()?;
    paths.retain(|path| path.extension().is_some_and(|ext| ext == "native"));
    paths.sort();
    // Spread the sample over the index instead of taking one repo's shards.
    let step = paths.len().div_ceil(take).max(1);
    let mut blocks: Vec<Vec<u8>> = Vec::new();
    let mut buf = Vec::new();
    for path in paths.iter().step_by(step) {
        let shard = backend.open(path)?;
        for doc in 0..shard.doc_count() {
            let content = shard.content(doc, &mut buf)?;
            // As the writer: blocks under 4 KiB stay raw.
            blocks.extend(unit_spans(content, UNIT_TARGET).into_iter().map(|s| content[s].to_vec()));
        }
    }
    let raw: usize = blocks.iter().map(Vec::len).sum();
    let mut out = std::io::stdout().lock();
    writeln!(out, "blocks={} raw_mib={:.1}", blocks.len(), raw as f64 / f64::from(1 << 20))?;
    // lz4_flex's encoder (the writer's before E38), lz4's fast mode, then the HC levels.
    let modes: Vec<(String, Option<lz4::block::CompressionMode>)> =
        [("flex".to_string(), None), ("fast".to_string(), Some(lz4::block::CompressionMode::DEFAULT))]
            .into_iter()
            .chain(
                [3, 6, 9, 12]
                    .map(|level| (format!("hc{level}"), Some(lz4::block::CompressionMode::HIGHCOMPRESSION(level)))),
            )
            .collect();
    let encode = |b: &[u8], mode: Option<lz4::block::CompressionMode>| match mode {
        None => Ok(lz4_flex::block::compress(b)),
        Some(mode) => lz4::block::compress(b, Some(mode), false),
    };
    for (name, mode) in modes {
        let started = Instant::now();
        let stored: Vec<Option<Vec<u8>>> = blocks
            .iter()
            .map(|b| -> std::io::Result<_> {
                Ok(if b.len() >= 4096 { Some(encode(b, mode)?).filter(|c| c.len() < b.len()) } else { None })
            })
            .collect::<std::io::Result<_>>()?;
        let encode_time = started.elapsed();
        let size: usize = stored.iter().zip(&blocks).map(|(s, b)| s.as_ref().map_or(b.len(), Vec::len)).sum();
        // A block can pass the target: spans end at a newline, and a line can be long.
        let mut dst_buf = vec![0u8; blocks.iter().map(Vec::len).max().unwrap_or(0)];
        let mut best = Duration::MAX;
        for _ in 0..5 {
            let started = Instant::now();
            for (s, b) in stored.iter().zip(&blocks) {
                let dst = &mut dst_buf[..b.len()];
                match s {
                    Some(c) => {
                        let n = lz4::block::decompress_to_buffer(c, Some(i32::try_from(b.len())?), dst)?;
                        assert_eq!(n, b.len());
                    }
                    None => dst.copy_from_slice(b),
                }
                black_box(&dst_buf);
            }
            best = best.min(started.elapsed());
        }
        writeln!(
            out,
            "{name} stored_mib={:.1} ratio={:.3} encode_mb_s={:.0} decode_ms={:.1} decode_gb_s={:.2}",
            size as f64 / f64::from(1 << 20),
            raw as f64 / size as f64,
            raw as f64 / 1e6 / encode_time.as_secs_f64(),
            best.as_secs_f64() * 1000.0,
            raw as f64 / 1e9 / best.as_secs_f64(),
        )?;
    }
    Ok(())
}
