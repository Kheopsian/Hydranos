//! Bench torrent_to_json — serde_json::Value build with 13k entries.
//! Flamegraph 2026-04-19 : ~588M samples in prod.
//! Compares current json!{} (BTreeMap-backed) vs a manual String-build variant.
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::path::PathBuf;
use std::sync::Arc;
use typhon_engine::torrent::meta::{TorrentMeta, TorrentState, FileEntry};
use typhon_engine::rpc::dispatch::torrent_to_json;

fn make_torrent(i: u32) -> Arc<TorrentState> {
    let meta = TorrentMeta {
        info_hash: [i as u8; 20],
        name: format!("torrent-{}", i),
        pieces: vec![[0u8; 20]; 500],         // ~500 pieces
        piece_length: 1 << 20,                // 1 MB pieces
        total_size: 500 * (1 << 20),
        files: vec![FileEntry {
            path: PathBuf::from(format!("file-{}.bin", i)),
            offset: 0,
            length: 500 * (1 << 20),
        }],
        trackers: vec![vec!["http://t.example/announce".to_string()]],
        private: false,
        multi_file: false,
    };
    let save_path = PathBuf::from("/data");
    let torrent_file_path = format!("/torrents/doc-{}.torrent", i);
    Arc::new(TorrentState::new(meta, save_path, torrent_file_path, true))
}

fn bench_json_one(c: &mut Criterion) {
    let t = make_torrent(1);
    c.bench_function("torrent_to_json_one", |b| {
        b.iter(|| {
            let v = torrent_to_json(&t);
            black_box(v);
        });
    });
}

fn bench_json_13k(c: &mut Criterion) {
    let torrents: Vec<_> = (0..13_000u32).map(make_torrent).collect();
    c.bench_function("torrent_to_json_13k", |b| {
        b.iter(|| {
            let jsons: Vec<_> = torrents.iter().map(|t| torrent_to_json(t)).collect();
            black_box(jsons);
        });
    });
}

fn bench_json_13k_serialize(c: &mut Criterion) {
    let torrents: Vec<_> = (0..13_000u32).map(make_torrent).collect();
    c.bench_function("torrent_to_json_13k_plus_serialize", |b| {
        b.iter(|| {
            let jsons: Vec<_> = torrents.iter().map(|t| torrent_to_json(t)).collect();
            let s = serde_json::to_string(&jsons).unwrap();
            black_box(s);
        });
    });
}

criterion_group!(benches, bench_json_one, bench_json_13k, bench_json_13k_serialize);
criterion_main!(benches);
