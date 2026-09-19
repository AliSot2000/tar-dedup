use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
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

/// One shared bar set: a fixed bottom global bar (percent only) plus the
/// current phase bar(s) above it. All bars draw to stdout; `MultiProgress`
/// auto-hides when stdout is not a terminal — `suspend`-printed logs still emit.
pub struct ProgressBarSet {
    mp: Arc<MultiProgress>,
    global: ProgressBar,
    phase: Mutex<Option<ProgressBar>>,
    subs: Mutex<Vec<ProgressBar>>,
    table_size: Mutex<u64>,
    multiplier: u64,
}

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
            table_size: Mutex::new(0),
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

    /// Advance the phase bar and the global by `n`.
    pub fn inc_both(&self, n: u64) {
        if let Some(p) = self.phase.lock().expect("progress phase lock poisoned").as_ref() {
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
        if let Some(p) = self.phase.lock().expect("progress phase lock poisoned").as_ref() {
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
        if let Some(p) = self.phase.lock().expect("progress phase lock poisoned").take() {
            reset_bar(&p, kind);
            self.mp.remove(&p);
        }
        let subs = std::mem::take(&mut *self.subs.lock().expect("progress subs lock poisoned"));
        for s in subs {
            reset_bar(&s, kind);
            self.mp.remove(&s);
        }
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
        .with_ansi(io::stdout().is_terminal())
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
        let mpb = LOG_MPB.lock().expect("LOG_MPB lock poisoned").clone();
        match mpb {
            Some(mp) => mp.suspend(|| write_plain(buf)),
            None => write_plain(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn write_plain(buf: &[u8]) -> io::Result<usize> {
    let mut out = io::stdout();
    out.write_all(buf)?;
    out.flush()?;
    Ok(buf.len())
}

/// Register the bar set with the log writer; called when a run starts.
pub fn register_mpb(mp: Arc<MultiProgress>) {
    *LOG_MPB.lock().expect("LOG_MPB lock poisoned") = Some(mp);
}

pub fn unregister_mpb() {
    *LOG_MPB.lock().expect("LOG_MPB lock poisoned") = None;
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