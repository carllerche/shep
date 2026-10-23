//! Run: cargo run --release -p shep-storage --example io_bench --
//! [groups=8] [batches=500] [payload=4096] [durable|stream|mixed]
//! [direct|buffered]
use shep_storage::{LogConfig, Record, Storage, StorageConfig};
use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

fn percentile(values: &mut [Duration], p: usize) -> Duration {
    values.sort_unstable();
    values[(values.len() - 1) * p / 1000]
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let groups = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(8usize);
    let batches = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(500usize);
    let bytes = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(4096usize);
    let mode = args.get(4).map(String::as_str).unwrap_or("durable");
    assert!(matches!(mode, "durable" | "stream" | "mixed"));
    let durable = mode != "stream";
    assert!(groups > 0 && batches > 0);
    let root = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(tokio::runtime::LocalOptions::default())
        .unwrap();
    runtime.block_on(async {
        assert!(args.len() <= 6, "expected [groups] [batches] [payload] [durable|stream|mixed] [direct|buffered]");
        let config = StorageConfig { direct: args.get(5).is_none_or(|s| s != "buffered"), ..Default::default() };
        let storage = Storage::open(root.path(), config).await.unwrap();
        let mut logs = Vec::new();
        for id in 0..groups { logs.push(storage.open_log(id as u64).await.unwrap()); }
        for log in &logs {
            log.append([Record::new(0, 1, 0, vec![0; bytes])]).await.unwrap(); log.flush().await.unwrap();
        }
        let stop = Rc::new(Cell::new(false)); let mut readers = Vec::new(); let mut read_log = None;
        if mode == "mixed" {
            let log = storage.open_log_with(u64::MAX, LogConfig { cache_bytes: 0, ..Default::default() }).await.unwrap();
            for index in 0..512 { log.append([Record::new(index, 1, 0, vec![3; 64 << 10])]).await.unwrap(); }
            log.flush().await.unwrap();
            for reader in 0..4 {
                let log = log.clone(); let stop = stop.clone();
                readers.push(tokio::task::spawn_local(async move {
                    let mut index = reader; let mut latencies = Vec::new();
                    while !stop.get() {
                        index = (index * 73 + 17) % 512;
                        let started = Instant::now();
                        assert_eq!(log.read(index..index+1, 64 << 10).await.unwrap().len(), 1);
                        latencies.push(started.elapsed());
                    }
                    latencies
                }));
            }
            read_log = Some(log);
        }
        let started = Instant::now(); let mut tasks = Vec::new();
        for log in &logs {
            let log = log.clone();
            tasks.push(tokio::task::spawn_local(async move {
                let mut latencies = Vec::with_capacity(batches);
                for index in 1..=batches {
                    let t = Instant::now();
                    log.append([Record::new(index as u64, 1, 0, vec![0; bytes])]).await.unwrap();
                    if durable { log.flush().await.unwrap(); }
                    latencies.push(t.elapsed());
                }
                log.flush().await.unwrap(); latencies
            }));
        }
        let mut latencies = Vec::new();
        for task in tasks { latencies.extend(task.await.unwrap()); }
        let elapsed = started.elapsed(); stop.set(true);
        println!("mode={mode} backend=io_uring groups={groups} batches={batches} payload={bytes} throughput={:.1} MiB/s p50={:?} p99={:?} p999={:?} elapsed={elapsed:?}",
            (groups * batches * bytes) as f64 / elapsed.as_secs_f64() / (1024.0 * 1024.0),
            percentile(&mut latencies, 500), percentile(&mut latencies, 990), percentile(&mut latencies, 999));
        let mut read_latencies = Vec::new();
        for task in readers { read_latencies.extend(task.await.unwrap()); }
        if !read_latencies.is_empty() {
            println!("reads={} read-p99={:?}", read_latencies.len(), percentile(&mut read_latencies, 990));
        }
        for log in logs { log.close().await.unwrap(); }
        if let Some(log) = read_log { log.close().await.unwrap(); }
        storage.shutdown().await.unwrap();
    });
}
