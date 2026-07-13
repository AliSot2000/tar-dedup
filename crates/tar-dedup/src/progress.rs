use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

const IO_BUF_SIZE: usize = 1024 * 1024;
/// Tar read chunk size during archive (keep xz fed without huge resident buffers).
const ARCHIVE_IO_BUF_SIZE: usize = 4 * 1024 * 1024;

pub fn io_buffer() -> Vec<u8> {
    vec![0u8; IO_BUF_SIZE]
}

pub fn archive_io_buffer() -> Vec<u8> {
    vec![0u8; ARCHIVE_IO_BUF_SIZE]
}

pub struct ByteProgress {
    bar: ProgressBar,
}

impl ByteProgress {
    pub fn new(label: &str, total: u64) -> Self {
        let bar = ProgressBar::new(total);
        bar.set_style(
            ProgressStyle::with_template(
                "{spinner} {msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} @ {bytes_per_sec}",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        bar.set_message(label.to_string());
        bar.enable_steady_tick(Duration::from_millis(100));
        Self { bar }
    }

    pub fn set_position(&self, pos: u64) {
        self.bar.set_position(pos);
    }

    pub fn inc(&self, n: u64) {
        self.bar.inc(n);
    }

    pub fn set_file(&self, label: &str, file: impl AsRef<std::path::Path>) {
        let short = truncate_middle(&file.as_ref().to_string_lossy(), 56);
        self.bar.set_message(format!("{label} {short}"));
    }

    pub fn set_message(&self, msg: &str) {
        self.bar.set_message(msg.to_string());
    }

    pub fn finish(&self, label: &str) {
        self.bar
            .finish_with_message(format!("{label}: {}", self.bar.position()));
    }

    pub fn abandon(&self) {
        self.bar.abandon();
    }
}

pub struct CountProgress {
    bar: ProgressBar,
}

impl CountProgress {
    pub fn new(label: &str) -> Self {
        let bar = ProgressBar::new_spinner();
        bar.set_style(
            ProgressStyle::with_template("{spinner} {msg} {pos} items")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        bar.set_message(label.to_string());
        Self { bar }
    }

    pub fn inc(&self, n: u64) {
        self.bar.inc(n);
    }

    pub fn finish(&self, label: &str) {
        self.bar.finish_with_message(format!("{label}: {} items", self.bar.position()));
    }

    /// Bar with a known total (hash/dedup-style).
    pub fn with_total(label: &str, total: u64) -> Self {
        let bar = ProgressBar::new(total);
        bar.set_style(
            ProgressStyle::with_template("{spinner} {msg} [{bar:40.cyan/blue}] {pos}/{len}")
                .unwrap()
                .progress_chars("=>-"),
        );
        bar.set_message(label.to_string());
        bar.enable_steady_tick(Duration::from_millis(100));
        Self { bar }
    }

    pub fn set_position(&self, pos: u64) {
        self.bar.set_position(pos);
    }

    pub fn set_file(&self, label: &str, file: impl AsRef<std::path::Path>) {
        let short = truncate_middle(&file.as_ref().to_string_lossy(), 56);
        self.bar.set_message(format!("{label} {short}"));
    }

    pub fn abandon(&self) {
        self.bar.abandon();
    }
}

fn truncate_middle(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1) / 2;
    let prefix: String = s.chars().take(keep).collect();
    let suffix: String = s.chars().rev().take(keep).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{prefix}…{suffix}")
}
