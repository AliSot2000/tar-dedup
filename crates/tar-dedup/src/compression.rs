mod xz;

use crate::config::CompressionFormat;

pub use xz::{
    compress_footer_bytes, decompress_footer_bytes, InterruptibleXzEncoder, FOOTER_XZ_PRESET,
};

pub fn warn_on_start(format: CompressionFormat) {
    // TODO: Needs to be tracing
    if format.does_compress() {
        eprintln!(
            "warning: each pause finalizes a compression stream; repeated interrupts increase archive size."
        );
    }
}
