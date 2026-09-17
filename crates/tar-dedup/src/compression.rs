mod xz;

use crate::config::CompressionFormat;

pub use xz::{
    FOOTER_XZ_PRESET, InterruptibleXzEncoder, compress_footer_bytes, decompress_footer_bytes,
};

pub fn warn_on_start(format: CompressionFormat) {
    if format.does_compress() {
        tracing::warn!(
            "warning: each pause finalizes a compression stream; repeated interrupts increase \
            archive size."
        );
    }
}
