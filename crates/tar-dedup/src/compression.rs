mod xz;

use crate::config::CompressionFormat;

pub use xz::InterruptibleXzEncoder;

pub fn warn_on_start(format: CompressionFormat) {
    // TODO: Needs to be tracing
    if format.does_compress() {
        eprintln!(
            "warning: each pause finalizes a compression stream; repeated interrupts increase archive size."
        );
    }
}
