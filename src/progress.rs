use indicatif::{ProgressBar, ProgressStyle};

const IO_BUF_SIZE: usize = 1024 * 1024;

pub fn io_buffer() -> Vec<u8> {
    vec![0u8; IO_BUF_SIZE]
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
