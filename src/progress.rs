use std::time::{Duration, Instant};

use humansize::{format_size, BINARY};
use indicatif::{ProgressBar, ProgressStyle};

pub struct ByteProgress {
    bar: ProgressBar,
    total: u64,
    started: Instant,
    last_report: Instant,
}

impl ByteProgress {
    pub fn new(label: &str, total: Option<u64>) -> Self {
        let bar = ProgressBar::new(total.unwrap_or(0));
        bar.set_style(
            ProgressStyle::with_template(
                "{msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, eta {eta})",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        bar.set_message(label.to_string());
        Self {
            bar,
            total: total.unwrap_or(0),
            started: Instant::now(),
            last_report: Instant::now(),
        }
    }

    pub fn set_message(&self, msg: &str) {
        self.bar.set_message(msg.to_string());
    }

    pub fn on_bytes(&mut self, consumed: u64) {
        self.bar.set_position(consumed);
        if self.last_report.elapsed() >= Duration::from_secs(5) && consumed > 0 {
            let elapsed = self.started.elapsed().as_secs_f64();
            let rate = consumed as f64 / elapsed;
            if rate > 0.0 && self.total > consumed {
                let remaining = (self.total - consumed) as f64 / rate;
                self.bar.set_message(format!(
                    "{} | ~{} remaining",
                    self.bar.message(),
                    format_duration(remaining)
                ));
            }
            self.last_report = Instant::now();
        }
    }

    pub fn finish(&self) {
        self.bar.finish_with_message(format!(
            "done ({})",
            format_size(self.bar.position(), BINARY)
        ));
    }
}

fn format_duration(secs: f64) -> String {
    let s = secs as u64;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    format!("{h}h {m}m")
}
