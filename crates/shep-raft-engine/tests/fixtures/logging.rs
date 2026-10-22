use std::env;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use tracing::Event;
use tracing::Subscriber;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Registry;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::fmt::FormatEvent;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::fmt::FormattedFields;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::time::SystemTime;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

pub struct EventFormatter {}

impl<S, N> FormatEvent<S, N> for EventFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(&self, ctx: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &Event<'_>) -> std::fmt::Result {
        let meta = event.metadata();

        SystemTime {}.format_time(&mut writer)?;
        writer.write_char(' ')?;

        let fmt_level = meta.level().as_str();
        write!(writer, "{:>5} ", fmt_level)?;

        write!(writer, "{:0>2?} ", std::thread::current().id())?;

        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                write!(writer, "{}", span.metadata().target())?;
                write!(writer, "@{}", span.metadata().name())?;
                write!(writer, "#{:x}", span.id().into_u64())?;

                let ext = span.extensions();
                if let Some(fields) = &ext.get::<FormattedFields<N>>()
                    && !fields.is_empty()
                {
                    write!(writer, "{{{}}}", fields)?;
                }
                write!(writer, ": ")?;
            }
        };

        ctx.format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// Where test logs go: under the target directory, never the source tree.
fn log_dir() -> PathBuf {
    let exe = env::current_exe().ok();
    let binary = exe
        .as_deref()
        .and_then(Path::file_stem)
        .and_then(|s| s.to_str())
        .unwrap_or("tests");
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("shep-raft-engine-logs").join(binary)
}

/// One test's log file. It's deleted when the test passes, and kept, with
/// its path printed, when the test fails or panics.
pub struct TestLog {
    path: PathBuf,
    done: bool,
}

impl TestLog {
    /// Creates a log file for `test` and a subscriber that writes to it, at
    /// `RUST_LOG`'s level or else `level`.
    pub fn create(test: &str, level: &str) -> (TestLog, impl Subscriber + Send + Sync + 'static) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = log_dir();
        std::fs::create_dir_all(&dir).expect("creating the test log directory");
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{test}-{}-{n}.log", std::process::id()));
        let file = File::create(&path).expect("creating a test log");

        let f_layer = fmt::Layer::new()
            .with_span_events(fmt::format::FmtSpan::NONE)
            .with_writer(Mutex::new(BufWriter::new(file)))
            .with_ansi(false)
            .event_format(EventFormatter {});

        // Use env RUST_LOG to initialize log if present.
        // Otherwise use the specified level.
        let directives = env::var(EnvFilter::DEFAULT_ENV).unwrap_or_else(|_x| level.to_string());
        let env_filter = EnvFilter::new(directives);

        let subscriber = Registry::default().with(env_filter).with(f_layer);
        (TestLog { path, done: false }, subscriber)
    }

    /// The test finished: delete the log if it passed.
    pub fn finish(mut self, passed: bool) {
        self.done = true;
        if passed {
            let _ = std::fs::remove_file(&self.path);
        } else {
            eprintln!("test log: {}", self.path.display());
        }
    }
}

impl Drop for TestLog {
    fn drop(&mut self) {
        // Dropped unfinished: the test panicked.
        if !self.done {
            eprintln!("test log: {}", self.path.display());
        }
    }
}
