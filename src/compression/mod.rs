mod xz;

use crate::config::CompressionFormat;

pub use xz::{InterruptibleXzEncoder, PRESET as XZ_PRESET};

pub fn warn_on_start(format: CompressionFormat) {
    if format.allows_resume() {
        eprintln!(
            "warning: each pause finalizes a compression stream; repeated interrupts increase archive size."
        );
    }
}
