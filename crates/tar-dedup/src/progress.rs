use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::mem::take;
use tracing::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::{FilterExt, LevelFilter, filter_fn};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry;


/// Number of element-iterating phases in the archive pipeline (Cleanup-style
/// no-op phases excluded). Every `files` row advances the global bar once per
/// phase, so the table size is scaled by this.
pub const ARCHIVE_MULTIPLIER: u64 = 7;
/// Number of element-iterating phases in the extract pipeline. `Cleanup` does
/// no per-element work and is excluded.
pub const EXTRACT_MULTIPLIER: u64 = 6;

/// Style kind for a phase bar. The template is chosen at set time; `Count` and
/// `Bytes` need a length (`set_phase_total`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BarKind {
    /// Spinner + running count: `{spinner} {msg} {pos}`.
    Counter,
    /// Spinner + fraction bar: `{spinner} {msg} [{bar:40}] {pos}/{len}`.
    Count,
    /// Spinner + byte bar: `{spinner} {msg} [{bar:40}] {bytes}/{total_bytes} @ {bytes_per_sec}`.
    Bytes,
}

const GLOBAL_TEMPLATE: &str = "{msg} [{bar:48.green/black}] {percent:>3}%";

// TODO wide
fn style_for(kind: BarKind) -> ProgressStyle {
    let template = match kind {
        BarKind::Counter => "{spinner} {msg} {pos}",
        BarKind::Count => "{spinner} {msg} [{bar:40.cyan/blue}] {pos}/{len}",
        BarKind::Bytes => {
            "{spinner} {msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} @ {bytes_per_sec}"
        }
    };
    ProgressStyle::with_template(template)
        .expect("valid indicatif template")
        .progress_chars("=>-")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
}

/// The `MultiProgress` currently driving the terminal, if any. Registered by
/// `ProgressBarSet` while a run is live; the tracing stdout writer consults it
/// so log lines are printed above the bars.
static LOG_MPB: Mutex<Option<Arc<MultiProgress>>> = Mutex::new(None);

// INFO: Since we are partially capturing the ProgressBarSet for rayon / multiprocessing. We need
//  to make all things either behind one single mutex or multiple mutex-es to ensure we don't end
//  with memory corruption
/// One shared bar set: a fixed bottom global bar (percent only) plus the
/// current phase bar(s) above it. All bars draw to stdout; `MultiProgress`
/// auto-hides when stdout is not a terminal — `suspend`-printed logs still emit.
pub struct ProgressBarSet {
    mp: Arc<MultiProgress>,
    global: ProgressBar,
    phase: Mutex<Option<ProgressBar>>,
    subs: Mutex<Vec<ProgressBar>>,
    thread_bars: Mutex<ThreadBars>,
    table_size: Mutex<u64>,
    multiplier: u64,
}

/// Lazy pool of per-thread sub-bars, one per rayon worker (`rayon::current_thread_index()`).
/// Bars are only materialized on first use by their thread and live until
/// [`ProgressBarSet::create_thread_bars`] resets the pool or `drop_thread_bars` removes them.
struct ThreadBars {
    kind: Option<BarKind>,
    count: usize,
    bars: Vec<Option<ProgressBar>>,
}

// TODO why is phase behind mutex? Doesn't progress already do mutex et al?
impl ProgressBarSet {
    pub fn new(multiplier: u64) -> Self {
        let mp = Arc::new(MultiProgress::new());
        let global = ProgressBar::new(0);
        global.set_style(
            ProgressStyle::with_template(GLOBAL_TEMPLATE).expect("valid indicatif template"),
        );
        global.set_message("tar-dedup");
        mp.add(global.clone());
        Self {
            mp,
            global,
            phase: Mutex::new(None),
            subs: Mutex::new(Vec::new()),
            thread_bars: Mutex::new(ThreadBars {
                kind: None,
                count: 0,
                bars: Vec::new(),
            }),
            table_size: Mutex::new(1),
            multiplier,
        }
    }

    /// Scale the global bar to the full table: length = `n * multiplier`.
    pub fn set_table_size(&self, n: u64) {
        *self.table_size.lock().expect("progress size lock poisoned") = n;
        self.global.set_length(n.saturating_mul(self.multiplier));
    }

    /// Drop the previous phase bar(s) and start a new phase. Snapping the
    /// global to `idx * table_size` both "finishes off" the previous phase and
    /// anchors resume runs mid-pipeline.
    pub fn begin_phase(&self, idx: u64, msg: &str, kind: BarKind) {
        let total = *self.table_size.lock().expect("progress size lock poisoned");
        self.global.set_position(idx.saturating_mul(total));
        self.finish_current(ResetKind::Finish);
        let bar = make_bar(kind, msg);
        self.mp.insert_before(&self.global, bar.clone());
        *self.phase.lock().expect("progress phase lock poisoned") = Some(bar);
    }

    pub fn set_phase_total(&self, n: u64) {
        if let Some(p) = self.phase.lock().expect("progress phase lock poisoned").as_ref() {
            p.set_length(n);
        }
    }

    pub fn set_phase_position(&self, pos: u64) {
        if let Some(p) = self.phase.lock().expect("progress phase lock poisoned").as_ref() {
            p.set_position(pos);
        }
    }

    pub fn set_phase_msg(&self, s: &str) {
        if let Some(p) = self.phase.lock().expect("progress phase lock poisoned").as_ref() {
            p.set_message(s.to_string());
        }
    }

    /// Shortened `label path` message on the phase bar (mimics the old
    /// `set_file` helpers).
    pub fn set_phase_file(&self, label: &str, file: impl AsRef<Path>) {
        let short = truncate_middle(&file.as_ref().to_string_lossy(), 56);
        self.set_phase_msg(&format!("{label} {short}"));
    }

    /// Switch the phase bar style without dropping it (spinner → bar once a
    /// total is known). Keeps the current position.
    pub fn set_phase_kind(&self, kind: BarKind) {
        if let Some(p) = self.phase.lock().expect("progress phase lock poisoned").as_ref() {
            p.set_style(style_for(kind));
        }
    }

    /// Insert a sub-bar just above the phase bar (used for complex phases).
    pub fn push_sub_bar(&self, msg: &str, kind: BarKind) -> ProgressBar {
        let bar = make_bar(kind, msg);
        {
            let phase = self.phase.lock().expect("progress phase lock poisoned");
            match phase.as_ref() {
                Some(p) => self.mp.insert_before(p, bar.clone()),
                None => self.mp.add(bar.clone()),
            };
        }
        self.subs.lock().expect("progress subs lock poisoned").push(bar.clone());
        bar
    }

    /// Prepare a lazy per-thread bar pool for the current phase. Call once at the
    /// start of a parallel phase; bars are materialized on first use by each
    /// thread index (`thread_bar`) and torn down with [`Self::drop_thread_bars`]
    /// at the end. `count` is the worker pool size (`effective_jobs`).
    pub fn create_thread_bars(&self, kind: BarKind, count: usize) {
        debug_assert!(count > 0, "thread bar pool must have at least one slot");
        // Defensive: rip out anything a previous phase forgot to drop. Runs
        // before taking the lock below — `drop_thread_bars` locks this Mutex.
        self.drop_thread_bars();
        let mut tb = self.thread_bars
            .lock()
            .expect("progress thread-bars lock poisoned");
        tb.kind = Some(kind);
        tb.count = count;
        tb.bars.resize_with(count, || None);
    }

    /// Fetch the bar for a worker thread, creating it on first use. The returned
    /// handle is reused by that thread index for the whole phase (reset + resize
    /// per new file); collected by `drop_thread_bars`.
    pub fn thread_bar(&self, idx: usize) -> ProgressBar {
        // Attempt to get an existing bar and validate the idx.
        let mut tb = self.thread_bars
            .lock()
            .expect("progress thread-bars lock poisoned");
        if tb.bars.len() <= idx {
            panic!("Index out of bounds {idx} is not in bars with len: {}", tb.bars.len());
        }
        if let Some(b) = &tb.bars[idx] {
            return b.clone();
        }

        // PRECONDITION: No bar present in the vec.
        let kind = tb.kind.expect(
            "INVARIANT ERROR: create_thread_bars must be called before thread_bar");
        let bar = make_bar(kind, "");

        // Insert the bar into the MultiProgress bar.
        match self.phase.lock().expect("progress phase lock poisoned").as_ref() {
            Some(p) => self.mp.insert_before(p, bar.clone()),
            None => self.mp.insert_before(&self.global, bar.clone()),
        };

        // Update the vector with the new bar.
        tb.bars[idx] = Some(bar.clone());
        bar
    }

    /// Remove every thread bar created since the last `create_thread_bars`,
    /// leaving only the phase bar and the global bar. Idempotent.
    pub fn drop_thread_bars(&self) {
        let mut tb = self.thread_bars
            .lock()
            .expect("progress thread-bars lock poisoned");

        // Drop the bars
        for slot in take(&mut tb.bars) {
            if let Some(b) = slot {
                b.finish_and_clear();
                self.mp.remove(&b);
            }
        }
        tb.count = 0;
        tb.kind = None;
    }

    /// Advance the phase bar and the global by `n`.
    pub fn inc_both(&self, n: u64) {
        if let Some(p) = self.phase
            .lock()
            .expect("progress phase lock poisoned").as_ref() {
            p.inc(n);
        }
        self.global.inc(n);
    }

    /// Advance only the global (bulk SQL promotions of rows leaving the phase).
    pub fn inc_global(&self, n: u64) {
        self.global.inc(n);
    }

    /// Advance only the phase bar (byte-level progress inside a file).
    pub fn inc_phase(&self, n: u64) {
        if let Some(p) = self.phase
            .lock()
            .expect("progress phase lock poisoned")
            .as_ref() {
            p.inc(n);
        }
    }

    /// Complete every bar; the global ends at 100%.
    pub fn finish(&self) {
        self.finish_current(ResetKind::Finish);
        self.global
            .set_position(self.global.length().unwrap_or(0));
        self.global.finish();
    }

    /// Abandon every bar (interrupt / exit-after-stage). Leaves no live bar.
    pub fn abandon(&self) {
        self.finish_current(ResetKind::Abandon);
        self.global.abandon();
    }

    pub fn mp_handle(&self) -> Arc<MultiProgress> {
        self.mp.clone()
    }

    fn finish_current(&self, kind: ResetKind) {
        if let Some(p) = self.phase
            .lock()
            .expect("progress phase lock poisoned")
            .take() {
            reset_bar(&p, kind);
            self.mp.remove(&p);
        }
        let subs = take(&mut *self
            .subs
            .lock()
            .expect("progress subs lock poisoned"));
        for s in subs {
            reset_bar(&s, kind);
            self.mp.remove(&s);
        }
        self.drop_thread_bars();
    }
}

/// What to do with a bar being torn down.
#[derive(Clone, Copy)]
enum ResetKind {
    Finish,
    Abandon,
}

fn reset_bar(bar: &ProgressBar, kind: ResetKind) {
    match kind {
        ResetKind::Finish => bar.finish_and_clear(),
        ResetKind::Abandon => bar.abandon(),
    }
}

impl Drop for ProgressBarSet {
    fn drop(&mut self) {
        unregister_mpb();
    }
}

fn make_bar(kind: BarKind, msg: &str) -> ProgressBar {
    let bar = match kind {
        BarKind::Counter => ProgressBar::new_spinner(),
        BarKind::Count | BarKind::Bytes => ProgressBar::new(0),
    };
    bar.set_style(style_for(kind));
    bar.set_message(msg.to_string());
    bar.enable_steady_tick(Duration::from_millis(100));
    bar
}

/// Local scope wrapper: finishes the bars on success, abandons them on any
/// error return (Drop runs when `?` propagates out of the run loop).
pub struct BarScope<'a> {
    set: &'a ProgressBarSet,
    done: bool,
}

impl<'a> BarScope<'a> {
    pub fn new(set: &'a ProgressBarSet) -> Self {
        Self { set, done: false }
    }

    pub fn finish(&mut self) {
        self.set.finish();
        self.done = true;
    }
}

impl Drop for BarScope<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.set.abandon();
        }
    }
}

/// Install the two-layer tracing subscriber:
/// - `tracing::error` → plain stderr;
/// - warn/info/debug/trace → stdout above the bars via the MPB.
///
/// Base verbosity is INFO unless RUST_LOG overrides it.
pub fn init_tracing() {
    let env = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    // `Level` ordinals invert with severity here, so use explicit predicates
    // rather than `LevelFilter` to split ERROR (stderr) from the rest (stdout).
    let error_only = filter_fn(|meta| *meta.level() == Level::ERROR);
    let below_error = filter_fn(|meta| *meta.level() != Level::ERROR);

    let error_layer = tracing_subscriber::fmt::layer()
        .with_writer(io::stderr)
        .with_ansi(io::stderr().is_terminal())
        .with_filter(env.clone().and(error_only));

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_writer(LogWriter)
        // ANSI-free: colored lines would break indicatif's width/line accounting
        // when they are piped through `MultiProgress::println`.
        .with_ansi(false)
        .with_filter(env.and(below_error));

    registry().with(error_layer).with(stdout_layer).init();
}

/// Writes to stdout, suspending the active bar set for the duration so the
/// line is not clobbered. Without a registered MPB (or while piped, where the
/// MPB is hidden but `suspend` still runs) it falls back to plain stdout.
struct LogWriter;

impl<'a> MakeWriter<'a> for LogWriter {
    type Writer = LogLine;

    fn make_writer(&self) -> Self::Writer {
        LogLine
    }
}

struct LogLine;

impl Write for LogLine {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Log lines go through the MPB's own draw path (`println`) whenever the
        // bars are live: it renders them above all bars as one coordinated
        // frame, so multi-line/wrapped events cannot desync the terminal.
        // Raw `suspend` writes are not usable here — an event may span more
        // terminal lines than the bar area, misaligning the cursor.
        //
        // Events are queued and flushed by a dedicated thread (~25 Hz) so a
        // burst (e.g. the WARN storm of many worker threads) collapses into a
        // few frames instead of one redraw per event.
        log_queue().push(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Coalesced stdout log sink. `MultiProgress::println` is called from a
/// dedicated thread, draining all events that arrived during a ~40 ms window
/// into a single frame above the bars (Docker-style multi-line output).
struct LogQueue {
    buf: Mutex<Vec<Vec<u8>>>,
    cv: Condvar,
}

static LOG_QUEUE: OnceLock<Arc<LogQueue>> = OnceLock::new();

fn log_queue() -> &'static Arc<LogQueue> {
    LOG_QUEUE.get_or_init(|| {
        let q = Arc::new(LogQueue {
            buf: Mutex::new(Vec::new()),
            cv: Condvar::new(),
        });
        let q2 = q.clone();
        thread::Builder::new()
            .name("tar-dedup-log".into())
            .spawn(move || log_flush_loop(q2))
            .expect("spawn log flusher");
        q
    })
}

impl LogQueue {
    fn push(&self, buf: &[u8]) {
        let mut guard = self.buf.lock().expect("log queue lock poisoned");
        guard.push(buf.to_vec());
        self.cv.notify_one();
    }

    fn drain(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.buf.lock().expect("log queue lock poisoned"))
    }
}

fn log_flush_loop(q: Arc<LogQueue>) {
    loop {
        let mut guard = q.buf.lock().expect("log queue lock poisoned");
        while guard.is_empty() {
            guard = q
                .cv
                .wait_timeout(guard, Duration::from_millis(40))
                .unwrap()
                .0;
        }
        let batch = std::mem::take(&mut *guard);
        drop(guard);
        if !batch.is_empty() {
            log_flush_batch(&batch);
        }
    }
}

fn log_flush_batch(batch: &[Vec<u8>]) {
    let mpb = LOG_MPB.lock().expect("LOG_MPB lock poisoned").clone();
    match mpb {
        Some(mp) if !mp.is_hidden() => {
            let mut msg = String::new();
            for b in batch {
                msg.push_str(&String::from_utf8_lossy(b));
            }
            let _ = mp.println(msg);
        }
        _ => {
            let mut out = io::stdout();
            for b in batch {
                let _ = out.write_all(b);
            }
            let _ = out.flush();
        }
    }
}

/// Register the bar set with the log writer; called when a run starts.
pub fn register_mpb(mp: Arc<MultiProgress>) {
    *LOG_MPB.lock().expect("LOG_MPB lock poisoned") = Some(mp);
}

pub fn unregister_mpb() {
    *LOG_MPB.lock().expect("LOG_MPB lock poisoned") = None;
    // Drain the queue so the tail of the run is not lost between run end and
    // process exit (the flusher thread stays alive for later runs).
    if let Some(q) = LOG_QUEUE.get() {
        let tail = q.drain();
        if !tail.is_empty() {
            log_flush_batch(&tail);
        }
    }
}

fn truncate_middle(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1) / 2;
    let prefix: String = s.chars().take(keep).collect();
    let suffix: String = s
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}…{suffix}")
}