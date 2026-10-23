//! Real-file, byte-for-byte companion to tools/io-bench/redpanda_append_bench.cc.
//! Use tools/io-bench/compare.py to build and run the paired experiment.
use shep_append::{AppendFile, BufferPool, Config as AppendConfig};
use shep_smp_io::{Buffer, Config as IoConfig, FileIo, Io, OpenOptions};
use shep_smp_io_scheduler::{Device, File, Scheduler};
use std::{
    cell::Cell,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

const ALIGNMENT: usize = 4096;
const READ_SIZE: usize = 64 << 10;
const READ_BLOCKS: usize = 512;
/// Common workload geometry, shared with the native Redpanda companion.
#[derive(Clone)]
struct Options {
    /// Fresh directory containing this process's benchmark files.
    directory: PathBuf,
    /// Durability workload: durable, stream, or mixed.
    mode: String,
    /// Concurrent files, each driven by one sequential producer.
    groups: usize,
    /// Measured appends per file, excluding warmup.
    iterations: usize,
    /// Untimed appends per file, synchronized before measurement.
    warmup: usize,
    /// Logical payload bytes per append.
    bytes: usize,
    /// Appender chunk size in bytes, matched to Redpanda.
    chunk: usize,
    /// Shared chunk-allocation budget in bytes.
    pool: usize,
    /// Byte increment reserved ahead of the current logical end.
    preallocate: u64,
    /// Concurrent direct range readers in mixed mode.
    readers: usize,
}
impl Options {
    /// Parse the common geometry and reject settings that cannot be matched by Redpanda.
    fn parse() -> Result<Self, Box<dyn std::error::Error>> {
        let mut opts = Self {
            directory: PathBuf::new(),
            mode: "durable".into(),
            groups: 8,
            iterations: 1000,
            warmup: 64,
            bytes: 4096,
            chunk: 64 << 10,
            pool: 30 << 20,
            preallocate: 2 << 20,
            readers: 4,
        };
        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            let value = args.next().ok_or("missing option value")?;
            match flag.as_str() {
                "--directory" => opts.directory = value.into(),
                "--mode" => opts.mode = value,
                "--groups" => opts.groups = value.parse()?,
                "--iterations" => opts.iterations = value.parse()?,
                "--warmup" => opts.warmup = value.parse()?,
                "--payload" => opts.bytes = value.parse()?,
                "--chunk" => opts.chunk = value.parse()?,
                "--pool" => opts.pool = value.parse()?,
                "--preallocate" => opts.preallocate = value.parse()?,
                "--readers" => opts.readers = value.parse()?,
                _ => return Err(format!("unknown option {flag}").into()),
            }
        }
        if opts.directory.as_os_str().is_empty()
            || opts.groups == 0
            || opts.iterations == 0
            || opts.bytes == 0
            || opts.chunk < ALIGNMENT
            || !opts.chunk.is_multiple_of(ALIGNMENT)
            || opts.pool < opts.chunk
            || !opts.pool.is_multiple_of(15)
            || !opts.pool.is_multiple_of(opts.chunk)
            || opts.preallocate == 0
            || !opts.preallocate.is_multiple_of(ALIGNMENT as u64)
            || !matches!(opts.mode.as_str(), "durable" | "stream" | "mixed")
        {
            return Err("invalid benchmark geometry".into());
        }
        Ok(opts)
    }
}
/// Completed physical I/O totals; before/after snapshots exclude setup work.
#[derive(Clone, Copy, Default)]
struct Counts {
    /// Completed physical write calls during the accounting window.
    writes: u64,
    /// Physical bytes written, including padding and rewrites.
    bytes: u64,
    /// Completed data synchronizations; validates workload durability.
    fsyncs: u64,
    /// Successful preallocation calls during the accounting window.
    allocations: u64,
}
/// Match Redpanda's chunk alignment instead of taking advantage of a smaller
/// native filesystem minimum. The wrapper also measures actual successful I/O.
#[derive(Clone)]
struct Metered {
    /// Scheduled file retaining its driver and device admission class.
    file: File,
    /// Shared physical-I/O accounting across all benchmark appender files.
    counts: Rc<Cell<Counts>>,
}
impl FileIo for Metered {
    fn alignment(&self) -> usize {
        ALIGNMENT
    }
    async fn read_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        self.file.read_at(buffer, offset).await
    }
    /// Count completed physical writes, including direct-I/O padding and retries.
    async fn write_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        let (result, buffer) = self.file.write_at(buffer, offset).await;
        if let Ok(bytes) = result {
            let mut c = self.counts.get();
            c.writes += 1;
            c.bytes += bytes as u64;
            self.counts.set(c);
        }
        (result, buffer)
    }
    /// Count only successful synchronizations so the runner can assert durability.
    async fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data().await?;
        let mut c = self.counts.get();
        c.fsyncs += 1;
        self.counts.set(c);
        Ok(())
    }
    async fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all().await
    }
    /// Track preallocation extensions occurring inside the measurement window.
    async fn allocate(&self, offset: u64, len: u64) -> io::Result<()> {
        self.file.allocate(offset, len).await?;
        let mut c = self.counts.get();
        c.allocations += 1;
        self.counts.set(c);
        Ok(())
    }
    async fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len).await
    }
    async fn len(&self) -> io::Result<u64> {
        self.file.len().await
    }
}
/// Process-wide resource snapshot, including helper threads rather than just the reactor.
struct Usage(libc::rusage);
impl Usage {
    /// Snapshot CPU use and context switches across all process threads.
    fn now() -> io::Result<Self> {
        // rusage consists of integer counters and timevals; zero is valid.
        let mut value = unsafe { std::mem::zeroed() };
        // The pointer refers to a live, correctly sized rusage output structure.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut value) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(value))
    }
    /// Combine user and kernel CPU seconds for the before/after delta.
    fn cpu(&self) -> f64 {
        (self.0.ru_utime.tv_sec + self.0.ru_stime.tv_sec) as f64
            + (self.0.ru_utime.tv_usec + self.0.ru_stime.tv_usec) as f64 / 1e6
    }
}
/// Select exactly the same integer-rank percentile as the C++ companion.
fn percentile(values: &mut [u64], p: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[(values.len() - 1) * p / 1000]
}
/// Check every logical byte and EOF after close, outside the measured window.
fn verify(path: &Path, expected: u64) -> io::Result<()> {
    assert_eq!(fs::metadata(path)?.len(), expected);
    let mut input = fs::File::open(path)?;
    let mut buffer = vec![0; 1 << 20];
    let mut checked = 0;
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        assert!(buffer[..n].iter().all(|b| *b == b'Z'));
        checked += n as u64;
    }
    assert_eq!(checked, expected);
    Ok(())
}
/// Preserve unsorted operation latencies for independent percentile validation.
fn samples(path: &Path, writes: &[u64], reads: &[u64]) -> io::Result<()> {
    let mut file = io::BufWriter::new(fs::File::create(path)?);
    writeln!(file, "operation,latency_ns")?;
    for ns in writes {
        writeln!(file, "append,{ns}")?;
    }
    for ns in reads {
        writeln!(file, "read,{ns}")?;
    }
    file.flush()
}
/// Prepare identical files, release all workers together, and measure until durable.
async fn run(opts: Options) -> Result<(), Box<dyn std::error::Error>> {
    let io = Io::new(IoConfig::default())?;
    let scheduler = Scheduler::new(io.clone(), Device::default(), 256)?;
    let writes = scheduler.class("writes", 1000)?;
    let reads = scheduler.class("reads", 1000)?;
    let metadata = scheduler.class("metadata", 1000)?;
    let pool = BufferPool::new(opts.pool);
    let counts = Rc::new(Cell::new(Counts::default()));
    let payload = Rc::new(vec![b'Z'; opts.bytes]);
    let mut appenders = Vec::new();
    let mut paths = Vec::new();
    for i in 0..opts.groups {
        let path = opts.directory.join(format!("log-{i}"));
        let file = writes
            .open(
                path.clone(),
                OpenOptions {
                    create_new: true,
                    direct: true,
                    ..Default::default()
                },
            )
            .await?;
        if !ALIGNMENT.is_multiple_of(file.alignment()) {
            return Err("native file alignment exceeds common alignment".into());
        }
        let file = Metered {
            file,
            counts: counts.clone(),
        };
        let appender = AppendFile::open(
            file,
            pool.clone(),
            AppendConfig {
                chunk_size: opts.chunk,
                preallocate: opts.preallocate,
                idle: Duration::from_millis(1000),
            },
            0,
        )
        .await?;
        for _ in 0..opts.warmup {
            let end = appender.append(&payload).await?;
            if opts.mode != "stream" {
                appender.sync_through(end).await?;
            }
        }
        appender.sync_through(appender.positions().appended).await?;
        paths.push(path);
        appenders.push(appender);
    }
    let mut reader_file = None;
    if opts.mode == "mixed" {
        let file = reads
            .open(
                opts.directory.join("read-data"),
                OpenOptions {
                    create_new: true,
                    direct: true,
                    ..Default::default()
                },
            )
            .await?;
        let mut buffer = Buffer::from_slice(&vec![b'Z'; READ_SIZE], ALIGNMENT)?;
        for i in 0..READ_BLOCKS {
            let (n, returned) = file.write_at(buffer, (i * READ_SIZE) as u64).await;
            buffer = returned;
            assert_eq!(n?, READ_SIZE);
        }
        file.sync_data().await?;
        reader_file = Some(file);
    }
    metadata.sync_dir(opts.directory.clone()).await?;
    let stop = Rc::new(Cell::new(false));
    let (start, begin) = tokio::sync::watch::channel(false);
    let mut writer_tasks = Vec::new();
    let mut reader_tasks = Vec::new();
    for appender in &appenders {
        let appender = appender.clone();
        let payload = payload.clone();
        let opts = opts.clone();
        let mut begin = begin.clone();
        writer_tasks.push(tokio::task::spawn_local(async move {
            let mut latencies = Vec::with_capacity(opts.iterations);
            begin.wait_for(|b| *b).await.unwrap();
            for _ in 0..opts.iterations {
                let before = Instant::now();
                let end = appender.append(&payload).await?;
                if opts.mode != "stream" {
                    appender.sync_through(end).await?;
                }
                latencies.push(before.elapsed().as_nanos() as u64);
            }
            appender.sync_through(appender.positions().appended).await?;
            Ok::<_, io::Error>(latencies)
        }));
    }
    if let Some(file) = reader_file {
        for i in 0..opts.readers {
            let file = file.clone();
            let stop = stop.clone();
            let mut begin = begin.clone();
            reader_tasks.push(tokio::task::spawn_local(async move {
                let mut index = i;
                let mut latencies = Vec::new();
                let mut buffer = Buffer::new(READ_SIZE, ALIGNMENT)?;
                begin.wait_for(|b| *b).await.unwrap();
                while !stop.get() {
                    index = (index * 73 + 17) % READ_BLOCKS;
                    let before = Instant::now();
                    let (n, returned) = file.read_at(buffer, (index * READ_SIZE) as u64).await;
                    buffer = returned;
                    assert_eq!(n?, READ_SIZE);
                    assert_eq!(buffer[0], b'Z');
                    assert_eq!(buffer[READ_SIZE - 1], b'Z');
                    latencies.push(before.elapsed().as_nanos() as u64);
                }
                Ok::<_, io::Error>(latencies)
            }));
        }
    }
    let before = counts.get();
    let cpu_before = Usage::now()?;
    let began = Instant::now();
    start.send_replace(true);
    let mut write_samples = Vec::new();
    let mut read_samples = Vec::new();
    for task in writer_tasks {
        write_samples.extend(task.await??);
    }
    let duration_ns = began.elapsed().as_nanos() as u64;
    let cpu_after = Usage::now()?;
    stop.set(true);
    let after = counts.get();
    for task in reader_tasks {
        read_samples.extend(task.await??);
    }
    let expected_syncs = if opts.mode == "stream" {
        opts.groups
    } else {
        opts.groups * opts.iterations
    };
    assert_eq!(after.fsyncs - before.fsyncs, expected_syncs as u64);
    for appender in &appenders {
        appender.finish().await?;
    }
    for path in paths {
        verify(
            &path,
            (opts.warmup + opts.iterations) as u64 * opts.bytes as u64,
        )?;
    }
    samples(
        &opts.directory.join("latencies.csv"),
        &write_samples,
        &read_samples,
    )?;
    println!(
        "SHEP_IO_BENCH {{\"engine\":\"shep\",\"layer\":\"append-file\",\"backend\":\"io_uring\",\"mode\":\"{}\",\"groups\":{},\"iterations\":{},\"warmup\":{},\"payload_bytes\":{},\"chunk_bytes\":{},\"pool_bytes\":{},\"alignment_bytes\":{},\"preallocate_bytes\":{},\"wall_ns\":{},\"cpu_seconds\":{},\"voluntary_switches\":{},\"involuntary_switches\":{},\"write_calls\":{},\"write_bytes\":{},\"fsyncs\":{},\"allocations\":{},\"p50_ns\":{},\"p99_ns\":{},\"p999_ns\":{},\"reads\":{},\"read_p99_ns\":{},\"verified\":true}}",
        opts.mode,
        opts.groups,
        opts.iterations,
        opts.warmup,
        opts.bytes,
        opts.chunk,
        opts.pool,
        ALIGNMENT,
        opts.preallocate,
        duration_ns,
        cpu_after.cpu() - cpu_before.cpu(),
        cpu_after.0.ru_nvcsw - cpu_before.0.ru_nvcsw,
        cpu_after.0.ru_nivcsw - cpu_before.0.ru_nivcsw,
        after.writes - before.writes,
        after.bytes - before.bytes,
        after.fsyncs - before.fsyncs,
        after.allocations - before.allocations,
        percentile(&mut write_samples, 500),
        percentile(&mut write_samples, 990),
        percentile(&mut write_samples, 999),
        read_samples.len(),
        percentile(&mut read_samples, 990)
    );
    scheduler.shutdown().await?;
    Ok(())
}
/// Embed the production stack in a local runtime; the runner supplies CPU affinity.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opts = Options::parse()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(tokio::runtime::LocalOptions::default())?;
    runtime.block_on(run(opts))
}
