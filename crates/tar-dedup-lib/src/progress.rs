use std::io::{self, IsTerminal, Write};
use std::mem::take;
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::common::formatting::{format_bytes_per_sec, format_count_si, format_elapsed};

use console::measure_text_width;
use indicatif::style::ProgressTracker;
use indicatif::{HumanBytes, MultiProgress, ProgressDrawTarget, ProgressState, ProgressStyle};
use tracing::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::{FilterExt, LevelFilter, filter_fn};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry;

use crate::cli::{LogLevel, ProgressMode};


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
    /// Like [`Bytes`](BarKind::Bytes), but `{rate}` is a cumulative
    /// `@ avg (elapsed)` computed from the bar's own position/elapsed at draw
    /// time via an indicatif custom tracker. `elapsed` resets with `reset()`, so
    /// dynamic per-file bars use the new interval; no background thread runs.
    /// This keeps a useful rate while the producer stalls, e.g. xz compression.
    BytesRate,
}

const GLOBAL_TEMPLATE: &str = "{msg} [{bar:48.green/black}] {percent:>3}%";
const MIN_MSG_NORMAL: usize = 12;
const MIN_MSG_COMPACT: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BarRole {
    Global,
    Phase,
    Sub,
    Thread,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LayoutMode {
    Normal,
    Compact,
    Percent,
}

fn normal_template(role: BarRole, kind: BarKind) -> &'static str {
    match (role, kind) {
        (BarRole::Global, _) => GLOBAL_TEMPLATE,
        (BarRole::Thread, BarKind::Counter) => "{spinner} {prefix}: {msg} {pos}",
        (BarRole::Thread, BarKind::Count) => {
            "{spinner} {prefix}: {msg} [{bar:40.cyan/blue}] {pos}/{len}"
        }
        (BarRole::Thread, BarKind::Bytes) => {
            "{spinner} {prefix}: {msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} @ {bytes_per_sec}"
        }
        (BarRole::Thread, BarKind::BytesRate) => {
            "{spinner} {prefix}: {msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} {rate}"
        }
        (_, BarKind::Counter) => "{spinner} {msg} {pos}",
        (_, BarKind::Count) => "{spinner} {msg} [{bar:40.cyan/blue}] {pos}/{len}",
        (_, BarKind::Bytes) => {
            "{spinner} {msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} @ {bytes_per_sec}"
        }
        (_, BarKind::BytesRate) => {
            "{spinner} {msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} {rate}"
        }
    }
}

fn compact_template(role: BarRole, kind: BarKind) -> &'static str {
    match (role, kind) {
        (BarRole::Global, _) => "{spinner} {msg} {percent:>3}%",
        (BarRole::Thread, BarKind::Counter) => "{spinner} {prefix}: {msg} {pos}",
        (BarRole::Thread, _) => "{spinner} {prefix}: {msg} {percent:>3}%",
        (_, BarKind::Counter) => "{spinner} {msg} {pos}",
        (_, BarKind::Count) => "{spinner} {msg} {pos}/{len}",
        (_, BarKind::Bytes | BarKind::BytesRate) => "{spinner} {msg} {bytes}/{total_bytes}",
    }
}

fn percent_template(role: BarRole, kind: BarKind) -> &'static str {
    match (role, kind) {
        (BarRole::Global, _) => "{msg} {percent:>3}%",
        // Thread bars are hidden in Percent; keep a harmless style if ever drawn.
        (BarRole::Thread, _) => "{spinner} {prefix}: {msg} {percent:>3}%",
        (_, BarKind::Counter) => "{msg} {si_pos}",
        _ => "{msg} {percent:>3}%",
    }
}

fn style_for_mode(role: BarRole, kind: BarKind, mode: LayoutMode) -> ProgressStyle {
    let template = match mode {
        LayoutMode::Normal => normal_template(role, kind),
        LayoutMode::Compact => compact_template(role, kind),
        LayoutMode::Percent => percent_template(role, kind),
    };
    let style = ProgressStyle::with_template(template)
        .expect("valid indicatif template")
        .progress_chars("=>-")
        .tick_strings(tick_strings());
    match (mode, kind) {
        (LayoutMode::Normal, BarKind::BytesRate) => style.with_key("rate", ByteRateTracker),
        (LayoutMode::Percent, BarKind::Counter)
            if role != BarRole::Global && role != BarRole::Thread =>
        {
            style.with_key("si_pos", CountSiTracker)
        }
        _ => style,
    }
}

/// Cumulative `@ avg (elapsed)` tracker for [`BarKind::BytesRate`]. The rate is
/// consumed bytes divided by the bar's elapsed time, so a producer stall does
/// not collapse it to zero. `elapsed` resets with `ProgressBar::reset()`, which
/// makes the same tracker correct for per-file dynamic bars.
#[derive(Clone, Copy)]
struct ByteRateTracker;

impl ProgressTracker for ByteRateTracker {
    fn clone_box(&self) -> Box<dyn ProgressTracker> {
        Box::new(*self)
    }

    fn tick(&mut self, _state: &ProgressState, _now: Instant) {}

    fn reset(&mut self, _state: &ProgressState, _now: Instant) {}

    fn write(&self, state: &ProgressState, w: &mut dyn std::fmt::Write) {
        let _ = write!(w, "{}", format_avg_rate(state.pos(), state.elapsed()));
    }
}

/// SI-abbreviated running count for a Percent-mode [`BarKind::Counter`].
#[derive(Clone, Copy)]
struct CountSiTracker;

impl ProgressTracker for CountSiTracker {
    fn clone_box(&self) -> Box<dyn ProgressTracker> {
        Box::new(*self)
    }

    fn tick(&mut self, _state: &ProgressState, _now: Instant) {}

    fn reset(&mut self, _state: &ProgressState, _now: Instant) {}

    fn write(&self, state: &ProgressState, w: &mut dyn std::fmt::Write) {
        let _ = write!(w, "{}", format_count_si(state.pos()));
    }
}

/// Cumulative effective transfer rate: consumed bytes divided by elapsed time.
fn format_avg_rate(pos: u64, elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64().max(1e-3);
    format!(
        "@ {} ({})",
        format_bytes_per_sec(pos as f64 / secs),
        format_elapsed(elapsed),
    )
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
pub struct ProgressBar(pub indicatif::ProgressBar, Option<Arc<BarMeta>>);

impl ProgressBar {
    pub fn new(len: u64) -> Self {
        Self(indicatif::ProgressBar::new(len), None)
    }
    pub fn new_spinner() -> Self {
        Self(indicatif::ProgressBar::new_spinner(), None)
    }
    pub fn hidden() -> Self {
        Self(indicatif::ProgressBar::hidden(), None)
    }

    pub fn inc(&self, delta: u64) {
        slow_sleep();
        self.0.inc(delta);
    }

    /// Set the original (unfitted) message. The displayed form is derived from
    /// the current layout width; the raw string is retained for resize refits.
    pub fn set_message(&self, msg: impl Into<String>) {
        let raw = msg.into();
        if let Some(meta) = &self.1 {
            *meta.raw.lock().expect("progress raw lock poisoned") = raw;
            self.refit();
        } else {
            self.0.set_message(raw);
        }
    }

    fn refit(&self) {
        let Some(meta) = &self.1 else { return };
        let raw = meta.raw.lock().expect("progress raw lock poisoned").clone();
        let rendered = meta.render(&raw, self.0.position(), self.0.length());
        self.0.set_message(rendered);
    }

    fn sync_style(&self) {
        let Some(meta) = &self.1 else { return };
        self.0.set_style(style_for_mode(meta.role, meta.kind(), meta.layout.mode()));
    }

    fn set_kind(&self, kind: BarKind) {
        let Some(meta) = &self.1 else { return };
        meta.set_kind(kind);
        self.sync_style();
        self.refit();
    }

    /// (Normal fixed width, Compact fixed width), excluding the message itself.
    fn required_widths(&self) -> (usize, usize) {
        let Some(meta) = &self.1 else { return (0, 0) };
        meta.fixed_widths(self.0.position(), self.0.length())
    }
}

impl Deref for ProgressBar {
    type Target = indicatif::ProgressBar;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

struct BarMeta {
    role: BarRole,
    kind: Mutex<BarKind>,
    raw: Mutex<String>,
    prefix: String,
    layout: Arc<LayoutState>,
}

impl BarMeta {
    fn kind(&self) -> BarKind {
        *self.kind.lock().expect("progress kind lock poisoned")
    }

    fn set_kind(&self, kind: BarKind) {
        *self.kind.lock().expect("progress kind lock poisoned") = kind;
    }

    fn fixed_widths(&self, pos: u64, len: Option<u64>) -> (usize, usize) {
        (self.fixed_normal(pos, len), self.fixed_compact(pos, len))
    }

    fn fixed_normal(&self, pos: u64, len: Option<u64>) -> usize {
        if self.role == BarRole::Global {
            return self.raw.lock().expect("progress raw lock poisoned").len() + 56;
        }
        let thread = self.role == BarRole::Thread;
        let prefix = if thread { self.prefix.len() } else { 0 };
        match self.kind() {
            BarKind::Counter if thread => 5 + prefix + digits(pos),
            BarKind::Counter => 3 + digits(pos),
            BarKind::Count if thread => 48 + prefix + suffix_count(pos, len),
            BarKind::Count => 46 + suffix_count(pos, len),
            BarKind::Bytes if thread => {
                48 + prefix + suffix_bytes(pos, len, thread) + 3 + MAX_RATE_WIDTH
            }
            BarKind::Bytes => 46 + suffix_bytes(pos, len, thread) + 3 + MAX_RATE_WIDTH,
            BarKind::BytesRate if thread => {
                48 + prefix + suffix_bytes(pos, len, thread) + 1 + MAX_AVG_RATE_WIDTH
            }
            BarKind::BytesRate => {
                46 + suffix_bytes(pos, len, thread) + 1 + MAX_AVG_RATE_WIDTH
            }
        }
    }

    fn fixed_compact(&self, pos: u64, len: Option<u64>) -> usize {
        if self.role == BarRole::Global {
            return self.raw.lock().expect("progress raw lock poisoned").len() + 7;
        }
        let thread = self.role == BarRole::Thread;
        let prefix = if thread { self.prefix.len() } else { 0 };
        match self.kind() {
            BarKind::Counter if thread => 5 + prefix + digits(pos),
            BarKind::Counter => 3 + digits(pos),
            BarKind::Count if thread => 9 + prefix,
            BarKind::Count => 3 + suffix_count(pos, len),
            BarKind::Bytes | BarKind::BytesRate if thread => 9 + prefix,
            BarKind::Bytes | BarKind::BytesRate => 3 + suffix_bytes(pos, len, thread),
        }
    }

    fn fixed_percent(&self, _pos: u64, _len: Option<u64>) -> usize {
        if self.role == BarRole::Global {
            return self.raw.lock().expect("progress raw lock poisoned").len() + 5;
        }
        if self.role == BarRole::Thread {
            return 9 + self.prefix.len();
        }
        match self.kind() {
            BarKind::Counter => 1 + COUNT_SI_WIDTH,
            BarKind::Count | BarKind::Bytes | BarKind::BytesRate => 5,
        }
    }

    fn render(&self, raw: &str, pos: u64, len: Option<u64>) -> String {
        let mode = self.layout.mode();
        let width = self.layout.width();
        if self.role == BarRole::Global {
            if mode != LayoutMode::Percent {
                return raw.to_string();
            }
            let budget = width.saturating_sub(5);
            return pad_to(fit_tail(raw, budget), budget);
        }
        if self.role == BarRole::Thread && mode == LayoutMode::Percent {
            return raw.to_string();
        }
        let fixed = match mode {
            LayoutMode::Normal => self.fixed_normal(pos, len),
            LayoutMode::Compact => self.fixed_compact(pos, len),
            LayoutMode::Percent => self.fixed_percent(pos, len),
        };
        let budget = width.saturating_sub(fixed);
        let fitted = match mode {
            LayoutMode::Percent => fit_tail(raw, budget),
            LayoutMode::Normal | LayoutMode::Compact => fit_middle(raw, budget),
        };
        pad_to(fitted, budget)
    }
}

const MAX_RATE_WIDTH: usize = 16;
const MAX_AVG_RATE_WIDTH: usize = 40;
const MAX_BYTES_WIDTH: usize = 12;
const COUNT_SI_WIDTH: usize = 6;

fn digits(n: u64) -> usize {
    if n == 0 { 1 } else { n.ilog10() as usize + 1 }
}

fn thread_prefix(idx: usize, count: usize) -> String {
    let width = digits(count as u64);
    format!("T{:0width$}", idx + 1, width = width)
}

fn suffix_count(pos: u64, len: Option<u64>) -> usize {
    match len {
        Some(len) => digits(pos) + 1 + digits(len),
        None => digits(pos),
    }
}

fn suffix_bytes(pos: u64, len: Option<u64>, thread: bool) -> usize {
    if thread {
        return MAX_BYTES_WIDTH;
    }
    match len {
        Some(len) => {
            format!("{}", HumanBytes(pos)).len() + 1 + format!("{}", HumanBytes(len)).len()
        }
        None => format!("{}", HumanBytes(pos)).len(),
    }
}

struct LayoutState {
    progress_mode: ProgressMode,
    width: AtomicUsize,
    mode: Mutex<LayoutMode>,
}

impl LayoutState {
    fn new(progress_mode: ProgressMode) -> Self {
        Self {
            progress_mode,
            width: AtomicUsize::new(terminal_cols()),
            mode: Mutex::new(LayoutMode::Normal),
        }
    }

    fn width(&self) -> usize {
        self.width.load(Ordering::Relaxed)
    }

    fn mode(&self) -> LayoutMode {
        *self.mode.lock().expect("progress layout lock poisoned")
    }

    fn set_mode(&self, mode: LayoutMode) {
        *self.mode.lock().expect("progress layout lock poisoned") = mode;
    }
}

fn terminal_cols() -> usize {
    if let Ok(value) = std::env::var("TAR_DEDUP_TERM_WIDTH") {
        if let Ok(cols) = value.trim().parse::<usize>() {
            if cols > 0 {
                return cols;
            }
        }
    }
    console::Term::stderr()
        .size_checked()
        .map(|(_, cols)| usize::from(cols))
        .unwrap_or(120)
}

fn fit_middle(s: &str, budget: usize) -> String {
    if measure_text_width(s) <= budget {
        return s.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    let ell = "…";
    let ell_width = measure_text_width(ell);
    if budget <= ell_width {
        return take_start_width(s, budget);
    }
    let left = (budget - ell_width) / 2;
    let right = budget - ell_width - left;
    format!(
        "{}{ell}{}",
        take_start_width(s, left),
        take_end_width(s, right),
    )
}

fn fit_tail(s: &str, budget: usize) -> String {
    if measure_text_width(s) <= budget {
        return s.to_string();
    }
    const ELL: &str = "...";
    if budget <= ELL.len() {
        return ".".repeat(budget);
    }
    let keep = budget - ELL.len();
    format!("{}{ELL}", take_start_width(s, keep))
}

fn pad_to(mut s: String, budget: usize) -> String {
    let used = measure_text_width(&s);
    if used < budget {
        s.push_str(&" ".repeat(budget - used));
    }
    s
}

fn take_start_width(s: &str, budget: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = measure_text_width(&ch.to_string());
        if used + w > budget {
            break;
        }
        used += w;
        out.push(ch);
    }
    out
}

fn take_end_width(s: &str, budget: usize) -> String {
    let mut out = Vec::new();
    let mut used = 0;
    for ch in s.chars().rev() {
        let w = measure_text_width(&ch.to_string());
        if used + w > budget {
            break;
        }
        used += w;
        out.push(ch);
    }
    out.into_iter().rev().collect()
}

/// The `MultiProgress` currently driving the terminal, if any. Registered by
/// `ProgressBarSet` while a run is live; the tracing stdout writer consults it
/// so log lines are printed above the bars.
static LOG_MPB: Mutex<Option<Arc<MultiProgress>>> = Mutex::new(None);

// INFO: Since we are partially capturing the ProgressBarSet for rayon / multiprocessing. We need
//  to make all things either behind one single mutex or multiple mutex-es to ensure we don't end
//  with memory corruption
struct Inner {
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
    /// running phase without disturbing it. See [`ProgressBarSet::redirect_phase`].
    override_phase: Mutex<Option<ProgressBar>>,
    subs: Mutex<Vec<ProgressBar>>,
    thread_bars: Mutex<ThreadBars>,
    table_size: Mutex<u64>,
    multiplier: u64,
    layout: Arc<LayoutState>,
    /// Serializes layout transitions with structural bar changes.
    apply_lock: Mutex<()>,
}

/// Lazy pool of per-thread sub-bars, one per rayon worker (`rayon::current_thread_index()`).
/// Bars are only materialized on first use by their thread and live until
/// [`ProgressBarSet::create_thread_bars`] resets the pool or `drop_thread_bars` removes them.
struct ThreadBars {
    kind: Option<BarKind>,
    count: usize,
    bars: Vec<Option<ProgressBar>>,
    /// Thread bars stay alive while detached; this tracks whether they are in `mp`.
    attached: bool,
}

/// One shared bar set: a fixed bottom global bar (percent only) plus the
/// current phase bar(s) above it. All bars draw to stdout; `MultiProgress`
/// auto-hides when stdout is not a terminal — `suspend`-printed logs still emit.
pub struct ProgressBarSet {
    inner: Arc<Inner>,
    watcher_stop: Arc<AtomicBool>,
    watcher: Mutex<Option<thread::JoinHandle<()>>>,
}

impl ProgressBarSet {
    pub fn new(multiplier: u64) -> Self {
        Self::with_mode(multiplier, ProgressMode::PerThread)
    }

    pub fn with_mode(multiplier: u64, mode: ProgressMode) -> Self {
        let inner = Arc::new(Inner::new(multiplier, mode));
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = start_width_watcher(&inner, &stop);
        let set = Self {
            inner,
            watcher_stop: stop,
            watcher: Mutex::new(watcher),
        };
        set.inner.apply_layout();
        set
    }

    pub fn register_global(&self) {
        self.inner.register_global();
    }

    pub fn unregister_global(&self) {
        self.inner.unregister_global();
    }

    pub fn single_bar(&self) {
        self.inner.single_bar();
    }

    pub fn set_table_size(&self, n: u64) {
        self.inner.set_table_size(n);
    }

    pub fn begin_phase(&self, idx: u64, msg: &str, kind: BarKind) {
        self.inner.begin_phase(idx, msg, kind);
    }

    pub fn set_phase_total(&self, n: u64) {
        self.inner.set_phase_total(n);
    }

    pub fn set_phase_position(&self, pos: u64) {
        self.inner.set_phase_position(pos);
    }

    pub fn set_phase_msg(&self, s: &str) {
        self.inner.set_phase_msg(s);
    }

    pub fn set_phase_file(&self, label: &str, file: impl AsRef<Path>) {
        self.inner.set_phase_file(label, file);
    }

    pub fn set_phase_kind(&self, kind: BarKind) {
        self.inner.set_phase_kind(kind);
    }

    pub fn push_sub_bar(&self, msg: &str, kind: BarKind) -> ProgressBar {
        self.inner.push_sub_bar(msg, kind)
    }

    pub fn hide_bar(&self, bar: &ProgressBar) {
        self.inner.hide_bar(bar);
    }

    pub fn redirect_phase(&self, bar: ProgressBar) -> PhaseRedirect<'_> {
        self.inner.redirect_phase(bar)
    }

    pub fn begin_integrity(&self, total: u64) -> ProgressBar {
        self.inner.begin_integrity(total)
    }

    pub fn create_thread_bars(&self, kind: BarKind, count: usize) {
        self.inner.create_thread_bars(kind, count);
    }

    pub fn thread_bar(&self, idx: usize) -> ProgressBar {
        self.inner.thread_bar(idx)
    }

    pub fn drop_thread_bars(&self) {
        self.inner.drop_thread_bars();
    }

    pub fn inc_both(&self, n: u64) {
        self.inner.inc_both(n);
    }

    pub fn inc_global(&self, n: u64) {
        self.inner.inc_global(n);
    }

    pub fn inc_phase(&self, n: u64) {
        self.inner.inc_phase(n);
    }

    pub fn phase_position(&self) -> u64 {
        self.inner.phase_position()
    }

    pub fn global_position(&self) -> u64 {
        self.inner.global_position()
    }

    pub fn finish(&self) {
        self.inner.finish();
    }

    pub fn abandon(&self) {
        self.inner.abandon();
    }

    pub fn mp_handle(&self) -> Arc<MultiProgress> {
        self.inner.mp.clone()
    }

    #[cfg(test)]
    pub(crate) fn set_width_for_test(&self, cols: usize) {
        self.inner.layout.width.store(cols, Ordering::Relaxed);
        self.inner.apply_layout();
    }
}

impl Inner {
    fn new(multiplier: u64, progress_mode: ProgressMode) -> Self {
        let layout = Arc::new(LayoutState::new(progress_mode));
        let mp = Arc::new(MultiProgress::new());
        if progress_mode == ProgressMode::None {
            mp.set_draw_target(ProgressDrawTarget::hidden());
        }
        let global = make_global_bar(layout.clone());
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
                attached: false,
            }),
            table_size: Mutex::new(1),
            multiplier,
            layout,
            apply_lock: Mutex::new(()),
        }
    }

    fn progress_mode(&self) -> ProgressMode {
        self.layout.progress_mode
    }

    fn register_global(&self) {
        let mut f = self.global_registered
            .lock()
            .expect("global-registered lock poisoned");
        if !*f {
            self.mp.add(self.global.0.clone());
            *f = true;
        }
    }

    fn unregister_global(&self) {
        let mut f = self.global_registered
            .lock()
            .expect("global-registered lock poisoned");
        if *f {
            self.mp.remove(&self.global.0);
            *f = false;
        }
    }

    fn single_bar(&self) {
        if self.progress_mode() != ProgressMode::PerThread {
            return;
        }
        let _guard = self.apply_lock.lock().expect("progress apply lock poisoned");
        if let Some(p) = self.phase
            .lock()
            .expect("progress phase lock poisoned")
            .take()
        {
            reset_bar(&p, ResetKind::Finish);
            self.mp.remove(&p);
        }
        self.drop_thread_bars();
        self.unregister_global();
    }

    fn set_table_size(&self, n: u64) {
        *self.table_size.lock().expect("progress size lock poisoned") = n;
        self.global.set_length(n.saturating_mul(self.multiplier));
    }

    fn begin_phase(&self, idx: u64, msg: &str, kind: BarKind) {
        if self.progress_mode() == ProgressMode::PerThread {
            assert!(
                *self.global_registered.lock().expect("global-registered lock poisoned"),
                "no global progressbar: `unregister_global()`/`single_bar()` removed it as the \
                 last bar-modifying (footer) step; nothing may `begin_phase` on this \
                 `ProgressBarSet` afterwards"
            );
        }
        let _guard = self.apply_lock.lock().expect("progress apply lock poisoned");
        let total = *self.table_size.lock().expect("progress size lock poisoned");
        self.global.set_position(idx.saturating_mul(total));
        self.finish_current(ResetKind::Finish);
        let bar = make_bar(kind, BarRole::Phase, "", msg, self.layout.clone());
        if self.progress_mode() == ProgressMode::PerThread {
            self.mp.insert_before(&self.global, bar.0.clone());
        }
        *self.phase.lock().expect("progress phase lock poisoned") = Some(bar);
        drop(_guard);
        self.apply_layout();
    }

    fn phase_target(&self) -> Option<ProgressBar> {
        match self.override_phase.lock().expect("progress override lock poisoned").as_ref() {
            Some(o) => Some(o.clone()),
            None => self.phase.lock().expect("progress phase lock poisoned").clone(),
        }
    }

    fn set_phase_total(&self, n: u64) {
        if let Some(b) = self.phase_target() {
            b.set_length(n);
            self.apply_layout();
        }
    }

    fn set_phase_position(&self, pos: u64) {
        if let Some(b) = self.phase_target() {
            b.set_position(pos);
        }
    }

    fn set_phase_msg(&self, s: &str) {
        if let Some(b) = self.phase_target() {
            b.set_message(s);
        }
    }

    fn set_phase_file(&self, label: &str, file: impl AsRef<Path>) {
        if let Some(b) = self.phase_target() {
            b.set_message(format!("{label} {}", file.as_ref().display()));
        }
    }

    fn set_phase_kind(&self, kind: BarKind) {
        if let Some(b) = self.phase_target() {
            b.set_kind(kind);
            self.apply_layout();
        }
    }

    fn push_sub_bar(&self, msg: &str, kind: BarKind) -> ProgressBar {
        let _guard = self.apply_lock.lock().expect("progress apply lock poisoned");
        let bar = make_bar(kind, BarRole::Sub, "", msg, self.layout.clone());
        if self.progress_mode() == ProgressMode::PerThread {
            self.mp.insert(0, bar.0.clone());
        }
        self.subs.lock().expect("progress subs lock poisoned").push(bar.clone());
        drop(_guard);
        self.apply_layout();
        bar
    }

    fn hide_bar(&self, bar: &ProgressBar) {
        let _guard = self.apply_lock.lock().expect("progress apply lock poisoned");
        bar.finish_and_clear();
        if self.progress_mode() == ProgressMode::PerThread {
            self.mp.remove(bar);
        }
        self.subs
            .lock()
            .expect("progress subs lock poisoned")
            .retain(|b| !std::ptr::eq(b, bar));
        drop(_guard);
        self.apply_layout();
    }

    fn redirect_phase(&self, bar: ProgressBar) -> PhaseRedirect<'_> {
        let prev = self
            .override_phase
            .lock()
            .expect("progress override lock poisoned")
            .replace(bar.clone());
        PhaseRedirect { set: self, prev, bar }
    }

    fn begin_integrity(&self, total: u64) -> ProgressBar {
        let bar = self.push_sub_bar("integrity checks: ", BarKind::Count);
        bar.set_length(total);
        self.apply_layout();
        bar
    }

    fn create_thread_bars(&self, kind: BarKind, count: usize) {
        debug_assert!(count > 0, "thread bar pool must have at least one slot");
        self.drop_thread_bars();
        let mut tb = self.thread_bars
            .lock()
            .expect("progress thread-bars lock poisoned");
        tb.kind = Some(kind);
        tb.count = count;
        tb.bars.resize_with(count, || None);
        tb.attached = false;
        drop(tb);
        self.apply_layout();
    }

    fn thread_bar(&self, idx: usize) -> ProgressBar {
        let mut tb = self.thread_bars
            .lock()
            .expect("progress thread-bars lock poisoned");
        if tb.bars.len() <= idx {
            panic!("Index out of bounds {idx} is not in bars with len: {}", tb.bars.len());
        }
        if let Some(b) = &tb.bars[idx] {
            return b.clone();
        }

        let kind = tb.kind.expect(
            "INVARIANT ERROR: create_thread_bars must be called before thread_bar");
        let prefix = thread_prefix(idx, tb.count);
        let bar = make_bar(kind, BarRole::Thread, &prefix, "", self.layout.clone());
        if self.should_show_threads() {
            self.attach_bar(&bar);
            tb.attached = true;
        }
        tb.bars[idx] = Some(bar.clone());
        bar
    }

    fn drop_thread_bars(&self) {
        let mut tb = self.thread_bars
            .lock()
            .expect("progress thread-bars lock poisoned");
        for slot in take(&mut tb.bars) {
            if let Some(b) = slot {
                b.finish_and_clear();
                if tb.attached {
                    self.mp.remove(&b);
                }
            }
        }
        tb.count = 0;
        tb.kind = None;
        tb.attached = false;
    }

    fn inc_both(&self, n: u64) {
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

    fn inc_global(&self, n: u64) {
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

    fn inc_phase(&self, n: u64) {
        if let Some(b) = self.phase_target() {
            b.inc(n);
        }
    }

    fn phase_position(&self) -> u64 {
        self.phase_target().map_or(0, |b| b.position())
    }

    fn global_position(&self) -> u64 {
        self.global.position()
    }

    fn finish(&self) {
        let _guard = self.apply_lock.lock().expect("progress apply lock poisoned");
        self.finish_current(ResetKind::Finish);
        self.global
            .set_position(self.global.length().unwrap_or(0));
        self.global.finish();
    }

    fn abandon(&self) {
        let _guard = self.apply_lock.lock().expect("progress apply lock poisoned");
        self.finish_current(ResetKind::Abandon);
        self.global.abandon();
    }

    fn finish_current(&self, kind: ResetKind) {
        if let Some(p) = self.phase
            .lock()
            .expect("progress phase lock poisoned")
            .take()
        {
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

    fn should_show_threads(&self) -> bool {
        self.progress_mode() == ProgressMode::PerThread
            && self.layout.mode() != LayoutMode::Percent
    }

    fn attach_bar(&self, bar: &ProgressBar) {
        let phase = self.phase.lock().expect("progress phase lock poisoned").clone();
        if let Some(p) = phase {
            self.mp.insert_before(&p, bar.0.clone());
        } else if *self.global_registered.lock().expect("global-registered lock poisoned") {
            self.mp.insert_before(&self.global, bar.0.clone());
        }
    }

    fn select_mode(&self, cols: usize) -> LayoutMode {
        if self.progress_mode() == ProgressMode::None {
            return LayoutMode::Percent;
        }
        let (mut normal, mut compact) = self.global.required_widths();
        if self.progress_mode() == ProgressMode::PerThread {
            if let Some(p) = self.phase.lock().expect("progress phase lock poisoned").clone() {
                let (n, c) = p.required_widths();
                normal = normal.max(n);
                compact = compact.max(c);
            }
            for s in self.subs.lock().expect("progress subs lock poisoned").clone() {
                let (n, c) = s.required_widths();
                normal = normal.max(n);
                compact = compact.max(c);
            }
            let tb = self.thread_bars.lock().expect("progress thread-bars lock poisoned");
            if tb.count > 0 {
                if let Some(kind) = tb.kind {
                    let prefix = thread_prefix(0, tb.count);
                    let meta = BarMeta {
                        role: BarRole::Thread,
                        kind: Mutex::new(kind),
                        raw: Mutex::new(String::new()),
                        prefix,
                        layout: self.layout.clone(),
                    };
                    let (n, c) = meta.fixed_widths(0, None);
                    normal = normal.max(n);
                    compact = compact.max(c);
                }
            }
        }
        if cols >= normal + MIN_MSG_NORMAL {
            LayoutMode::Normal
        } else if cols >= compact + MIN_MSG_COMPACT {
            LayoutMode::Compact
        } else {
            LayoutMode::Percent
        }
    }

    fn apply_layout(&self) {
        let _guard = self.apply_lock.lock().expect("progress apply lock poisoned");
        if self.progress_mode() == ProgressMode::None {
            return;
        }
        let mode = self.select_mode(self.layout.width());
        self.layout.set_mode(mode);
        self.global.sync_style();
        if let Some(p) = self.phase.lock().expect("progress phase lock poisoned").clone() {
            p.sync_style();
            p.refit();
        }
        for s in self.subs.lock().expect("progress subs lock poisoned").clone() {
            s.sync_style();
            s.refit();
        }
        let mut tb = self.thread_bars.lock().expect("progress thread-bars lock poisoned");
        for b in tb.bars.iter().flatten() {
            b.sync_style();
            b.refit();
        }
        let show = self.progress_mode() == ProgressMode::PerThread
            && mode != LayoutMode::Percent;
        if show && !tb.attached {
            for b in tb.bars.iter().flatten() {
                self.attach_bar(b);
            }
            tb.attached = true;
        } else if !show && tb.attached {
            for b in tb.bars.iter().flatten() {
                self.mp.remove(b);
            }
            tb.attached = false;
        }
    }
}

fn start_width_watcher(inner: &Arc<Inner>, stop: &Arc<AtomicBool>) -> Option<thread::JoinHandle<()>> {
    if cfg!(test) || inner.progress_mode() == ProgressMode::None || !io::stderr().is_terminal() {
        return None;
    }
    let weak = Arc::downgrade(inner);
    let stop = stop.clone();
    Some(thread::Builder::new()
        .name("tar-dedup-term-width".into())
        .spawn(move || {
            loop {
                thread::park_timeout(Duration::from_millis(250));
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Some(inner) = weak.upgrade() else { break };
                inner.layout.width.store(terminal_cols(), Ordering::Relaxed);
                inner.apply_layout();
            }
        })
        .expect("spawn term-width watcher"))
}

/// Scoped phase override: while alive, phase-targeting operations drive the
/// override bar and the global stays put. Restores the previous override and
/// finishes + removes the override bar on drop.
pub struct PhaseRedirect<'a> {
    set: &'a Inner,
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
        self.watcher_stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.watcher.lock().expect("watcher lock poisoned").take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

fn make_global_bar(layout: Arc<LayoutState>) -> ProgressBar {
    let meta = Arc::new(BarMeta {
        role: BarRole::Global,
        kind: Mutex::new(BarKind::Count),
        raw: Mutex::new("tar-dedup".to_string()),
        prefix: String::new(),
        layout,
    });
    let bar = ProgressBar(indicatif::ProgressBar::new(0), Some(meta));
    bar.sync_style();
    bar.0.set_message("tar-dedup");
    bar.0.enable_steady_tick(Duration::from_millis(100));
    bar
}

fn make_bar(
    kind: BarKind,
    role: BarRole,
    prefix: &str,
    msg: &str,
    layout: Arc<LayoutState>,
) -> ProgressBar {
    let inner = match kind {
        BarKind::Counter => indicatif::ProgressBar::new_spinner(),
        BarKind::Count | BarKind::Bytes | BarKind::BytesRate => indicatif::ProgressBar::new(0),
    };
    let meta = Arc::new(BarMeta {
        role,
        kind: Mutex::new(kind),
        raw: Mutex::new(String::new()),
        prefix: prefix.to_string(),
        layout,
    });
    let bar = ProgressBar(inner, Some(meta));
    bar.sync_style();
    bar.set_message(msg);
    bar.0.enable_steady_tick(Duration::from_millis(100));
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
    fn bytes_rate_bar_is_a_bar_kind_and_finishes() {
        // The rate is a plain `BarKind` variant: normal inc drives it, and the
        // style's `{rate}` tracker reads position/elapsed on each draw. Nothing
        // perpendicular to the wrapper is touched.
        let set = ProgressBarSet::new(EXTRACT_MULTIPLIER);
        set.begin_phase(0, "archive", BarKind::BytesRate);
        set.set_phase_total(100);
        set.inc_phase(25);
        set.inc_phase(5);
        assert_eq!(set.phase_position(), 30);
        set.finish();
        assert_eq!(set.phase_position(), 0);
    }

    #[test]
    fn avg_rate_uses_consumed_bytes_and_elapsed_time() {
        assert_eq!(format_avg_rate(0, Duration::from_secs(1)), "@ 0B/s (0:01)");
        assert_eq!(
            format_avg_rate(2 * 1024 * 1024, Duration::from_secs(2)),
            "@ 1.00MiB/s (0:02)"
        );
        // Same position, shorter elapsed interval -> the reset-aware rate rises.
        assert_eq!(
            format_avg_rate(2 * 1024 * 1024, Duration::from_secs(1)),
            "@ 2.00MiB/s (0:01)"
        );
    }

    #[test]
    fn fit_helpers_pad_or_truncate_to_exact_width() {
        let long = "Hashing /mnt/Aljoscha-Storage/03_scratch/research/Sotoudeh-IMac/file.txt";
        for budget in [0, 1, 5, 20, 60, 160] {
            let middle = pad_to(fit_middle(long, budget), budget);
            let tail = pad_to(fit_tail(long, budget), budget);
            assert_eq!(measure_text_width(&middle), budget, "middle {budget}");
            assert_eq!(measure_text_width(&tail), budget, "tail {budget}");
        }
        let unicode = "Hashing /Bilder Beide NICHT LÖSCHEN/写真.jpg";
        for budget in [10, 17, 31] {
            let middle = pad_to(fit_middle(unicode, budget), budget);
            assert_eq!(measure_text_width(&middle), budget, "unicode {budget}");
        }
    }

    #[test]
    fn thread_prefix_scales_with_worker_count() {
        assert_eq!(thread_prefix(0, 1), "T1");
        assert_eq!(thread_prefix(7, 8), "T8");
        assert_eq!(thread_prefix(0, 10), "T01");
        assert_eq!(thread_prefix(15, 16), "T16");
        assert_eq!(thread_prefix(0, 128), "T001");
        assert_eq!(thread_prefix(127, 128), "T128");
    }

    #[test]
    fn per_thread_layout_switches_with_width() {
        let set = ProgressBarSet::new(EXTRACT_MULTIPLIER);
        set.begin_phase(0, "hash", BarKind::BytesRate);
        set.set_phase_total(1_000);

        set.set_width_for_test(120);
        assert_eq!(set.inner.layout.mode(), LayoutMode::Normal);
        set.set_width_for_test(60);
        assert_eq!(set.inner.layout.mode(), LayoutMode::Compact);
        set.set_width_for_test(10);
        assert_eq!(set.inner.layout.mode(), LayoutMode::Percent);
    }

    #[test]
    fn thread_bars_detach_and_reattach_on_width() {
        let set = ProgressBarSet::new(EXTRACT_MULTIPLIER);
        set.begin_phase(0, "hash", BarKind::Bytes);
        set.set_phase_total(1_000);
        set.create_thread_bars(BarKind::Bytes, 2);
        let t0 = set.thread_bar(0);
        let t1 = set.thread_bar(1);
        assert!(set.inner.thread_bars.lock().expect("threads").attached);

        set.set_width_for_test(10);
        assert_eq!(set.inner.layout.mode(), LayoutMode::Percent);
        assert!(!set.inner.thread_bars.lock().expect("threads").attached,
                "thread bars detached, handles retained");

        set.set_width_for_test(120);
        assert_eq!(set.inner.layout.mode(), LayoutMode::Normal);
        assert!(set.inner.thread_bars.lock().expect("threads").attached,
                "same handles reattached");
        t0.inc(1);
        t1.inc(1);
        assert_eq!(t0.position(), 1);
        assert_eq!(t1.position(), 1);
    }

    #[test]
    fn global_and_none_modes_gate_bars() {
        let global = ProgressBarSet::with_mode(EXTRACT_MULTIPLIER, ProgressMode::Global);
        global.begin_phase(0, "hash", BarKind::Counter);
        assert!(global.inner.phase.lock().expect("phase").is_some());
        global.push_sub_bar("sub", BarKind::Counter);
        global.create_thread_bars(BarKind::Bytes, 1);
        global.thread_bar(0);
        assert!(!global.inner.thread_bars.lock().expect("threads").attached,
                "global mode hides thread bars");

        let none = ProgressBarSet::with_mode(EXTRACT_MULTIPLIER, ProgressMode::None);
        assert!(none.inner.mp.is_hidden());
        none.begin_phase(0, "hash", BarKind::Counter);
        assert!(none.inner.phase.lock().expect("phase").is_some());
        none.create_thread_bars(BarKind::Bytes, 1);
        none.thread_bar(0);
        assert!(!none.inner.thread_bars.lock().expect("threads").attached,
                "none mode hides thread bars");
    }

    #[test]
    fn raw_message_refits_from_the_original_on_width_change() {
        let layout = Arc::new(LayoutState::new(ProgressMode::PerThread));
        let meta = BarMeta {
            role: BarRole::Phase,
            kind: Mutex::new(BarKind::Counter),
            raw: Mutex::new(String::new()),
            prefix: String::new(),
            layout: layout.clone(),
        };
        let raw = "Hashing /mnt/very/long/path/that/must/not/wrap/file.txt";
        layout.width.store(140, Ordering::Relaxed);
        layout.set_mode(LayoutMode::Normal);
        let wide = meta.render(raw, 10, Some(20));
        layout.width.store(40, Ordering::Relaxed);
        layout.set_mode(LayoutMode::Compact);
        let narrow = meta.render(raw, 10, Some(20));
        assert_eq!(measure_text_width(&wide), 140 - meta.fixed_normal(10, Some(20)));
        assert_eq!(measure_text_width(&narrow), 40 - meta.fixed_compact(10, Some(20)));
        assert!(narrow.contains("file.txt"), "basename survives middle truncation");
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