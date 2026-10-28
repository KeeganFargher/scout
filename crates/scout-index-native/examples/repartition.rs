//! Stress fixture: repartition real indexed text into many small repos, cycling it
//! to reach a requested size. Per-document compression prevents duplication from
//! creating an unrealistically tiny cross-file compressed index. This is not NX data.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use scout_core::catalog::{Catalog, RepoEntry, RepoIndex};
use scout_core::indexer::{UNIT_TARGET, unit_spans};
use scout_core::ports::{FileMeta, IndexBackend, Shard, ShardStore};
use scout_core::trigram::TrigramExtractor;
use scout_index_native::{ContentCodec, LookupHome, NativeBackend};
use scout_store_fs::FsStore;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        return Err("usage: repartition <source-index> <new-index> <repo-count> <target-GiB>".into());
    }
    let source = FsStore::open(&PathBuf::from(&args[1]))?;
    let dest = PathBuf::from(&args[2]);
    let count: usize = args[3].parse()?;
    let target = args[4].parse::<u64>()?.checked_mul(1 << 30).ok_or("target size overflow")?;
    if count == 0 || target == 0 || dest.exists() {
        return Err("positive sizes and a new output directory are required".into());
    }
    let backend = NativeBackend::new(LookupHome::Map, ContentCodec::Lz4);
    let original = source.load_catalog()?;
    let mut shards = Vec::new();
    for name in original.live_shards() {
        shards.push(backend.open(&source.local_path(name)?)?);
    }
    let store = FsStore::open(&dest)?;
    let partition = partition(&shards, count, target)?;
    let groups = partition.groups;
    let next = AtomicUsize::new(0);
    let started = Instant::now();
    let results = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let (next, groups, shards, store, backend) = (&next, &groups, &shards, &store, &backend);
                scope.spawn(move || -> scout_core::Result<Vec<(usize, RepoEntry)>> {
                    let mut result = Vec::new();
                    let mut scratch = Vec::new();
                    let mut extractor = TrigramExtractor::default();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(group) = groups.get(i) else { break };
                        let name = format!("repo-{i:03}");
                        let shard_name = format!("{name}.native");
                        let path = store.staging_path(&shard_name)?;
                        let mut writer = backend.create(&path)?;
                        let mut bytes = 0;
                        for (at, &(si, doc)) in group.iter().enumerate() {
                            let content = shards[si].content(doc, &mut scratch)?;
                            let old = shards[si].meta(doc)?;
                            let units: Vec<_> = unit_spans(content, UNIT_TARGET)
                                .into_iter()
                                .map(|r| extractor.extract(&content[r]).to_vec())
                                .collect();
                            let meta = FileMeta {
                                path: format!("source-{at:06}/{}", old.path),
                                size: content.len() as u64,
                                mtime_ns: 0,
                            };
                            writer.add(&meta, content, &units)?;
                            bytes += content.len() as u64;
                        }
                        writer.finish()?;
                        store.publish(&path, &shard_name)?;
                        let root = store.dir().join("sources").join(&name);
                        std::fs::create_dir_all(&root).map_err(|e| scout_core::Error::io(&root, e))?;
                        let root = root
                            .to_str()
                            .ok_or_else(|| scout_core::Error::Invalid("non-UTF-8 fixture path".into()))?
                            .to_owned();
                        result.push((
                            i,
                            RepoEntry {
                                name,
                                root,
                                remote: None,
                                index: Some(RepoIndex {
                                    backend: "native".into(),
                                    shards: vec![shard_name],
                                    listing_digest: None,
                                    commit: None,
                                    files: group.len() as u64,
                                    content_bytes: bytes,
                                    binary_files: 0,
                                    indexed_at_unix: 0,
                                }),
                            },
                        ));
                    }
                    Ok(result)
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().expect("fixture worker panicked")).collect::<scout_core::Result<Vec<_>>>()
    })?;
    let mut entries: Vec<_> = results.into_iter().flatten().collect();
    entries.sort_by_key(|(i, _)| *i);
    let catalog = Catalog { repos: entries.into_iter().map(|(_, r)| r).collect(), retired: Vec::new() };
    store.save_catalog(&catalog)?;
    let bytes: u64 = catalog.repos.iter().filter_map(|r| r.index.as_ref()).map(|i| i.content_bytes).sum();
    let evidence = serde_json::json!({"source": args[1], "repos":count,"requested_bytes":target,"indexed_bytes":bytes,
        "source_documents":partition.source_docs,"document_visits":partition.visits,"method":"cycle and repartition existing plain text; no generated filler; no cross-file compression",
        "elapsed_secs": started.elapsed().as_secs_f64()});
    std::fs::write(dest.join("fixture.json"), serde_json::to_vec_pretty(&evidence)?)?;
    std::io::Write::write_all(&mut std::io::stdout().lock(), format!("{evidence}\n").as_bytes())?;
    Ok(())
}

struct Partition {
    groups: Vec<Vec<(usize, u32)>>,
    source_docs: usize,
    visits: usize,
}

fn partition(shards: &[Box<dyn Shard>], count: usize, target: u64) -> scout_core::Result<Partition> {
    let mut docs = Vec::new();
    let mut source_bytes = 0u64;
    for (si, shard) in shards.iter().enumerate() {
        for doc in 0..shard.doc_count() {
            if shard.markers().reader_mode.binary_search(&doc).is_err() {
                let length = shard.meta(doc)?.size;
                source_bytes += length;
                docs.push((si, doc, length));
            }
        }
    }
    if source_bytes == 0 {
        return Err(scout_core::Error::Invalid("no nonempty plain-text documents".into()));
    }
    let per_repo = target.div_ceil(count as u64);
    let mut groups = Vec::new();
    let mut cursor = 0;
    for _ in 0..count {
        let mut group = Vec::new();
        let mut size = 0;
        while size < per_repo {
            let (si, doc, length) = docs[cursor % docs.len()];
            cursor += 1;
            group.push((si, doc));
            size += length.max(1);
        }
        groups.push(group);
    }
    Ok(Partition { groups, source_docs: docs.len(), visits: cursor })
}
