use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

const IO_BUF_SIZE: usize = 1024 * 1024;

pub fn io_buffer() -> Vec<u8> {
    vec![0u8; IO_BUF_SIZE]
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

    pub fn set_file(&self, label: &str, file: &str) {
        let short = truncate_middle(file, 56);
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
}

fn truncate_middle(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1) / 2;
    format!("{}…{}", &s[..keep], &s[s.len() - keep..])
}
