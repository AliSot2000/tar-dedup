use std::io::{self, IsTerminal, Write};
use std::mem::take;
use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::common::formatting::{format_bytes_per_sec, format_elapsed};

use indicatif::{MultiProgress, ProgressStyle};
use tracing::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::{FilterExt, LevelFilter, filter_fn};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry;

use crate::cli::LogLevel;


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
    /// Like [`Bytes`](BarKind::Bytes) but a `{prefix}` slot is fed a cumulative
    /// `@ avg (elapsed)` by a refresher thread started at construction (instead of
    /// indicatif's sliding `@ {bytes_per_sec}`). The thread computes `avg` from the
    /// bar's own `position()`/elapsed and exits once the bar is finished/gone.
    BytesRate,
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
        BarKind::BytesRate => {
            "{spinner} {prefix} {msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes}"
        }
    };
    ProgressStyle::with_template(template)
        .expect("valid indicatif template")
        .progress_chars("=>-")
        .tick_strings(tick_strings())
}

/// Spinner frames shared by the phase bar styles.
fn tick_strings() -> &'static [&'static str] {
    &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
}

/// Milliseconds to sleep before each bar increment (`debug-slow-progress` feature);
/// read once from `TAR_DEDUP_SLOW_PROGRESS_MS`, default 100.
#[cfg(feature = "debug-slow-progress")]
fn slow_sleep_duration() -> Duration {
    static MS: OnceLock<u64> = OnceLock::new();
    let ms = *MS.get_or_init(|| {
        std::env::var("TAR_DEDUP_SLOW_PROGRESS_MS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(100)
    });
    Duration::from_millis(ms)
}

#[cfg(feature = "debug-slow-progress")]
fn slow_sleep() {
    thread::sleep(slow_sleep_duration());
}

#[cfg(not(feature = "debug-slow-progress"))]
fn slow_sleep() {}

/// Wrapper around `indicatif::ProgressBar` so *every* increment in the crate —
/// the `ProgressBarSet` helpers and the sub/integrity/thread bars it hands out —
/// can be slowed at one choke point instead of touching every call site. Only the
/// `inc` increment is wrapped (the crate uses nothing else); the remaining API
/// passes through `Deref`.
#[derive(Clone)]
pub struct ProgressBar(pub indicatif::ProgressBar);

impl ProgressBar {
    pub fn new(len: u64) -> Self {
        Self(indicatif::ProgressBar::new(len))
    }
    pub fn new_spinner() -> Self {
        Self(indicatif::ProgressBar::new_spinner())
    }
    pub fn hidden() -> Self {
        Self(indicatif::ProgressBar::hidden())
    }

    pub fn inc(&self, delta: u64) {
        slow_sleep();
        self.0.inc(delta);
    }
}

impl Deref for ProgressBar {
    type Target = indicatif::ProgressBar;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
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
    /// Whether `global` is currently a member of `mp` (and therefore anchors the
    /// phase bar via `insert_before`). `false` only while the single-bar footer
    /// step has removed it (`unregister_global`); `begin_phase` / `thread_bar`
    /// assert this so any progress after that footer panics with a clear message.
    global_registered: Mutex<bool>,
    phase: Mutex<Option<ProgressBar>>,
    /// While `Some`, every phase-targeting operation drives this bar instead of the
    /// real phase bar, and `inc_both`/`inc_global` stop advancing the global. Used
    /// to show a self-contained sub-step (e.g. the eager extract filter) above a
    /// running phase without disturbing it. See [`Self::redirect_phase`].
    override_phase: Mutex<Option<ProgressBar>>,
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

// TODO think through the phase redirect.
// INFO: To share the ProgressBarSet amongst multiple threads all elements inside need to be thread safe. Rust does not
//  know that ProgressBar is thread safe -> MutEx
impl ProgressBarSet {
    pub fn new(multiplier: u64) -> Self {
        let mp = Arc::new(MultiProgress::new());
        let global = ProgressBar::new(0);
        global.set_style(
            ProgressStyle::with_template(GLOBAL_TEMPLATE).expect("valid indicatif template"),
        );
        global.set_message("tar-dedup");
        mp.add(global.0.clone());
        Self {
            mp,
            global,
            global_registered: Mutex::new(true),
            phase: Mutex::new(None),
            override_phase: Mutex::new(None),
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

    /// (Re-)join the global bar to the [`MultiProgress`], clearing the internal
    /// presence flag. Idempotent — no-op when it is already registered.
    pub fn register_global(&self) {
        let mut f = self.global_registered
            .lock()
            .expect("global-registered lock poisoned");

        if !*f {
            self.mp.add(self.global.0.clone());
            *f = true;
        }
    }

    /// Detach the global bar from the [`MultiProgress`] (it stays fully
    /// functional as a handle, just not drawn) and set the internal presence
    /// flag. Idempotent — no-op when it is already unregistered. Used by the
    /// single-bar footer step; while unregistered, `begin_phase`/`thread_bar`
    /// panic because they need the global as their `insert_before` anchor.
    pub fn unregister_global(&self) {
        let mut f = self.global_registered
            .lock()
            .expect("global-registered lock poisoned");

        if *f {
            self.mp.remove(&self.global.0);
            *f = false;
        }
    }

    /// Tear the phase and global bars down so only the caller's own sub-bar
    /// renders — the terminal step for an archive's footer integrity hash. The
    /// phase bar is dropped (the next `begin_phase` rebuilds it) and the global
    /// is unregistered; nothing on this `ProgressBarSet` may draw a new phase
    /// afterwards.
    pub fn single_bar(&self) {
        if let Some(p) = self.phase
            .lock()
            .expect("progress phase lock poisoned").take() {

            reset_bar(&p, ResetKind::Finish);
            self.mp.remove(&p);
        }
        self.unregister_global();
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
        assert!(
            *self.global_registered.lock().expect("global-registered lock poisoned"),
            "no global progressbar: `unregister_global()`/`single_bar()` removed it as the \
             last bar-modifying (footer) step; nothing may `begin_phase` on this \
             `ProgressBarSet` afterwards"
        );
        self.mp.insert_before(&self.global, bar.0.clone());
        *self.phase.lock().expect("progress phase lock poisoned") = Some(bar);
    }

    /// The current phase target: the override bar while a redirect is active,
    /// otherwise the real phase bar.
    fn phase_target(&self) -> Option<ProgressBar> {
        match self.override_phase.lock().expect("progress override lock poisoned").as_ref() {
            Some(o) => Some(o.clone()),
            None => self.phase.lock().expect("progress phase lock poisoned").clone(),
        }
    }

    pub fn set_phase_total(&self, n: u64) {
        if let Some(b) = self.phase_target() {
            b.set_length(n);
        }
    }

    pub fn set_phase_position(&self, pos: u64) {
        if let Some(b) = self.phase_target() {
            b.set_position(pos);
        }
    }

    pub fn set_phase_msg(&self, s: &str) {
        if let Some(b) = self.phase_target() {
            b.set_message(s.to_string());
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
        if let Some(b) = self.phase_target() {
            b.set_style(style_for(kind));
        }
    }

    /// Insert a sub-bar on top of the sub-bar stack (newest above older ones,
    /// above the phase bar / global). Used for complex phases.
    pub fn push_sub_bar(&self, msg: &str, kind: BarKind) -> ProgressBar {
        let bar = make_bar(kind, msg);
        self.mp.insert(0, bar.0.clone());
        self.subs.lock().expect("progress subs lock poisoned").push(bar.clone());
        bar
    }

    /// Finish and remove a sub-bar (and drop it from the tracked list). Used once a
    /// transient step (e.g. filter loading) completes so it stops drawing.
    pub fn hide_bar(&self, bar: &ProgressBar) {
        bar.finish_and_clear();
        self.mp.remove(bar);
        self.subs
            .lock()
            .expect("progress subs lock poisoned")
            .retain(|b| !std::ptr::eq(b, bar));
    }

    /// Redirect phase-targeting operations to `bar` (typically a sub-bar) and
    /// decouple them from the global until the returned guard is dropped. On drop
    /// the previous override is restored and `bar` is finished + removed.
    pub fn redirect_phase(&self, bar: ProgressBar) -> PhaseRedirect<'_> {
        let prev = self
            .override_phase
            .lock()
            .expect("progress override lock poisoned")
            .replace(bar.clone());
        PhaseRedirect { set: self, prev, bar }
    }

    /// Start a fraction bar for a self-contained step between phases (e.g. the
    /// cross-phase integrity battery). `total` is the number of steps; advance
    /// with `inc(1)` on the returned handle. Torn down with the phase bars.
    pub fn begin_integrity(&self, total: u64) -> ProgressBar {
        let bar = self.push_sub_bar("integrity checks: ", BarKind::Count);
        bar.set_length(total);
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
            Some(p) => self.mp.insert_before(p, bar.0.clone()),
            None => {
                assert!(
                    *self.global_registered.lock().expect("global-registered lock poisoned"),
                    "no global progressbar: `unregister_global()`/`single_bar()` removed it as \
                     the last bar-modifying (footer) step; no thread bar may be created \
                     afterwards"
                );
                self.mp.insert_before(&self.global, bar.0.clone())
            }
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

    /// Advance the phase bar and the global by `n`. While a redirect is active,
    /// advances only the override bar (global decoupled).
    pub fn inc_both(&self, n: u64) {
        let redirected = self.override_phase
            .lock()
            .expect("progress override lock poisoned")
            .is_some();
        if let Some(b) = self.phase_target() {
            b.inc(n);
            if !redirected {
                self.global.inc(n);
            }
        } else {
            self.global.inc(n);
        }
    }

    /// Advance only the global (bulk SQL promotions of rows leaving the phase).
    /// While a redirect is active, advances the override bar instead.
    pub fn inc_global(&self, n: u64) {
        if let Some(o) = self.override_phase
            .lock()
            .expect("progress override lock poisoned")
            .as_ref()
        {
            o.inc(n);
        } else {
            self.global.inc(n);
        }
    }

    /// Advance only the phase bar (byte-level progress inside a file).
    pub fn inc_phase(&self, n: u64) {
        if let Some(b) = self.phase_target() {
            b.inc(n);
        }
    }

    /// Current position of the active phase bar (the override bar while a
    /// redirect is active, else the real phase bar).
    pub fn phase_position(&self) -> u64 {
        self.phase_target().map_or(0, |b| b.position())
    }

    /// Current position of the global bar.
    pub fn global_position(&self) -> u64 {
        self.global.position()
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

/// Scoped phase override: while alive, phase-targeting operations drive the
/// override bar and the global stays put. Restores the previous override and
/// finishes + removes the override bar on drop.
pub struct PhaseRedirect<'a> {
    set: &'a ProgressBarSet,
    prev: Option<ProgressBar>,
    bar: ProgressBar,
}

impl Drop for PhaseRedirect<'_> {
    fn drop(&mut self) {
        *self.set
            .override_phase
            .lock()
            .expect("progress override lock poisoned") = self.prev.take();
        self.set.hide_bar(&self.bar);
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
        BarKind::Count | BarKind::Bytes | BarKind::BytesRate => ProgressBar::new(0),
    };
    bar.set_style(style_for(kind));
    bar.set_message(msg.to_string());
    bar.enable_steady_tick(Duration::from_millis(100));
    if kind == BarKind::BytesRate {
        start_byte_rate_refresher(&bar);
    }
    bar
}

/// Start the cumulative `@ avg (elapsed)` refresher for a [`BarKind::BytesRate`]
/// bar. Each second it reads the bar's own `position()` and the elapsed since
/// construction, writes the rate into the `{prefix}` slot, and exits once the bar
/// is finished/removed. Progress is driven entirely through the normal `inc`.
fn start_byte_rate_refresher(bar: &ProgressBar) {
    let inner = bar.0.clone();
    let start = Instant::now();
    thread::Builder::new()
        .name("tar-dedup-byte-rate".into())
        .spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(1));
                if inner.is_finished() {
                    return;
                }
                let elapsed = start.elapsed();
                let secs = elapsed.as_secs_f64().max(1e-3);
                let prefix = format!(
                    "@ {} ({})",
                    format_bytes_per_sec(inner.position() as f64 / secs),
                    format_elapsed(elapsed)
                );
                inner.set_prefix(prefix);
            }
        })
        .expect("spawn byte-rate refresher");
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
/// Base verbosity is INFO unless `--log-level` / `RUST_LOG` override it. The
/// two axes are resolved by the caller (`config::resolve_log_flags`): `long_time`
/// picks seconds vs sub-second, `target_visible` shows/hides the module target.
/// A `--log-time` template (chrono strftime) overrides the time axis entirely.
pub fn init_tracing(log_level: Option<LogLevel>, long_time: bool, target_visible: bool, log_time: Option<&str>) {
    let default_directive = log_level.map(level_filter).unwrap_or(LevelFilter::INFO);
    let env = EnvFilter::builder()
        .with_default_directive(default_directive.into())
        .from_env_lossy();

    // Timestamp precedence: user template > short timer > long (sub-second) default.
    if let Some(template) = log_time.filter(|s| !s.trim().is_empty()) {
        let t = template.to_string();
        install_tracer(target_visible, move || StrfTimer(t.clone()), env);
    } else if !long_time {
        install_tracer(target_visible, || SecondTimer, env);
    } else {
        install_tracer(target_visible, || tracing_subscriber::fmt::time::SystemTime, env);
    }
}

/// Build and install the two-layer subscriber with a shared timer. `with_timer`
/// must run before `with_filter` (which wraps the layer into a `Filtered`).
fn install_tracer<M, F>(
    target_visible: bool,
    make_timer: F,
    env: EnvFilter)
where
    M: FormatTime + Send + Sync + 'static,
    F: Fn() -> M,
{
    // `Level` ordinals invert with severity here, so use explicit predicates
    // rather than `LevelFilter` to split ERROR (stderr) from the rest (stdout).
    let error_only = filter_fn(|meta| *meta.level() == Level::ERROR);
    let below_error = filter_fn(|meta| *meta.level() != Level::ERROR);

    let error_layer = tracing_subscriber::fmt::layer()
        .with_writer(io::stderr)
        .with_ansi(io::stderr().is_terminal())
        .with_target(target_visible)
        .with_timer(make_timer())
        .with_filter(env.clone().and(error_only));
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_writer(LogWriter)
        // ANSI-free: colored lines would break indicatif's width/line accounting
        // when they are piped through `MultiProgress::println`.
        .with_ansi(false)
        .with_target(target_visible)
        .with_timer(make_timer())
        .with_filter(env.and(below_error));

    registry().with(error_layer).with(stdout_layer).init();
}

/// UTC timestamp truncated to whole seconds: `2026-11-06T01:12:28Z`.
#[derive(Clone, Copy, Default)]
struct SecondTimer;

impl FormatTime for SecondTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"))
    }
}

/// `--log-time`: render the current UTC time with a user-supplied chrono
/// strftime template. Unknown specifiers render literally (best-effort, no panic).
#[derive(Clone)]
struct StrfTimer(String);

impl FormatTime for StrfTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Utc::now().format(&self.0))
    }
}

/// Map the `--log-level` enum onto `tracing`'s `LevelFilter`.
fn level_filter(level: LogLevel) -> LevelFilter {
    match level {
        LogLevel::Off => LevelFilter::OFF,
        LogLevel::Error => LevelFilter::ERROR,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Trace => LevelFilter::TRACE,
    }
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

//==================================================================================================
// Testing
//==================================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_sub_bar_creates_independent_handles() {
        // Sub-bars are independent handles tracked alongside the phase bar;
        // advancing one never moves the phase or global bar.
        let set = ProgressBarSet::new(EXTRACT_MULTIPLIER);
        set.begin_phase(0, "scan", BarKind::Counter);
        let a = set.push_sub_bar("a", BarKind::Counter);
        let b = set.push_sub_bar("b", BarKind::Counter);
        a.inc(2);
        b.inc(3);
        assert_eq!(a.position(), 2);
        assert_eq!(b.position(), 3);
        assert_eq!(set.phase_position(), 0);
        assert_eq!(set.global_position(), 0);
    }

    #[test]
    fn redirect_phase_routes_progress_decouples_global_and_restores() {
        let set = ProgressBarSet::new(EXTRACT_MULTIPLIER);
        set.begin_phase(0, "scan", BarKind::Counter);
        set.set_phase_total(100);
        set.set_phase_position(10);
        set.inc_both(5);
        let base_phase = set.phase_position();
        let base_global = set.global_position();
        assert_eq!(base_phase, 15, "10 seeded + 5 inc_both");
        assert_eq!(base_global, 5);

        // While redirected, everything drives the override bar; global + real
        // phase bar stay frozen.
        let fbar = set.push_sub_bar("filter", BarKind::Count);
        let guard = set.redirect_phase(fbar.clone());
        set.set_phase_total(50);
        set.inc_both(7);
        set.inc_global(3);
        set.inc_phase(2);
        assert_eq!(fbar.position(), 12, "7 inc_both + 3 inc_global + 2 inc_phase");
        assert_eq!(fbar.length().unwrap_or(0), 50, "set_phase_total redirects");
        assert_eq!(set.global_position(), base_global, "global decoupled");
        assert_eq!(set.phase_position(), 12, "phase_position follows the override");
        drop(guard);

        // Restored: phase ops hit the real phase bar again, global untouched.
        assert_eq!(set.phase_position(), base_phase, "real phase bar restored");
        assert_eq!(set.global_position(), base_global);
        set.inc_both(1);
        assert_eq!(set.phase_position(), base_phase + 1);
        assert_eq!(set.global_position(), base_global + 1);
    }

    #[test]
    fn register_unregister_global_toggles_flag_and_begin_phase_panics_while_unregistered() {
        // The global is the `insert_before` anchor for a new phase; with it
        // unregistered (as the single-bar footer leaves it) any `begin_phase` is
        // a structural misuse and must panic with an explicit message rather than
        // falling through to indicatif's bare `index().unwrap()`.
        let set = ProgressBarSet::new(EXTRACT_MULTIPLIER);
        set.begin_phase(0, "scan", BarKind::Counter); // registered by default -> ok
        set.unregister_global();

        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            set.begin_phase(1, "scan", BarKind::Counter);
        }));
        let msg = res.err().expect("begin_phase must panic after unregister_global");
        let msg = msg.downcast_ref::<&str>().expect("panic payload is a &str");
        assert!(msg.contains("no global progressbar"), "{msg}");

        // register_global restores the anchor.
        set.register_global();
        set.begin_phase(1, "scan", BarKind::Counter);
    }

    #[test]
    fn single_bar_removes_phase_and_global_leaving_one_bar() {
        // A fresh phase bar exists; `single_bar` drops it (and the global), so the
        // caller's own sub-bar becomes the only rendered bar and `finish()` still
        // completes without touching a stale anchor.
        let set = ProgressBarSet::new(EXTRACT_MULTIPLIER);
        set.begin_phase(0, "scan", BarKind::Counter);
        set.set_phase_total(100);
        set.inc_both(5);
        assert_eq!(set.phase_position(), 5);

        set.single_bar();
        assert_eq!(set.phase_position(), 0, "phase bar dropped");

        let hash = set.push_sub_bar("archive integrity: ", BarKind::Bytes);
        hash.inc(7);
        assert_eq!(hash.position(), 7, "the lone sub-bar carries the progress");
        set.finish(); // no panic: the removed global's final draw is a safe no-op
    }

    fn fmt_timer(timer: &impl FormatTime) -> String {
        struct Fmt<'a>(&'a dyn FormatTime);
        impl std::fmt::Display for Fmt<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.format_time(&mut Writer::new(f))
            }
        }
        Fmt(timer).to_string()
    }

    #[test]
    fn second_timer_renders_whole_seconds_utc() {
        let s = fmt_timer(&SecondTimer);
        assert_eq!(s.len(), 20, "expected `YYYY-MM-DDTHH:MM:SSZ`, got {s:?}");
        assert!(s.ends_with('Z'), "must end with Z: {s}");
        // `T` at index 10, `Z` last, no fractional part.
        assert!(s.as_bytes()[10] == b'T');
        assert!(s.chars().nth(19) == Some('Z'));
        assert!(!s.contains('.'), "no sub-second digits: {s}");
        let (date, _rest) = s.split_once('T').expect("T");
        assert_eq!(date.len(), 10, "date {date}");
    }

    #[test]
    fn strf_timer_uses_provided_template() {
        let s = fmt_timer(&StrfTimer("%H:%M:%S".to_string()));
        assert_eq!(s.len(), 8, "HH:MM:SS expected, got {s:?}");
        assert!(s.as_bytes()[2] == b':' && s.as_bytes()[5] == b':');
        assert!(s.as_bytes().iter().all(|b| b.is_ascii_digit() || *b == b':'));

        let literal = fmt_timer(&StrfTimer("fixed".to_string()));
        assert_eq!(literal, "fixed");
    }

    #[test]
    fn log_level_maps_to_level_filter() {
        use tracing_subscriber::filter::LevelFilter;
        let cases = [
            (LogLevel::Off, LevelFilter::OFF),
            (LogLevel::Error, LevelFilter::ERROR),
            (LogLevel::Warn, LevelFilter::WARN),
            (LogLevel::Info, LevelFilter::INFO),
            (LogLevel::Debug, LevelFilter::DEBUG),
            (LogLevel::Trace, LevelFilter::TRACE),
        ];
        for (lv, want) in cases {
            assert_eq!(level_filter(lv), want, "{lv:?}");
        }
    }
}