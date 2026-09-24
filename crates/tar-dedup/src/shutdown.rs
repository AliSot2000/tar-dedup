use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread;

use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::error::{Error, Result};

const MODE_RUNNING: u8 = 0;
const MODE_GRACEFUL: u8 = 1;
const MODE_FORCE: u8 = 2;

#[derive(Clone)]
pub struct Shutdown {
    mode: Arc<AtomicU8>,
}

impl Shutdown {
    pub fn install() -> Result<Self> {
        let mode = Arc::new(AtomicU8::new(MODE_RUNNING));
        let mode_for_handler = mode.clone();

        let mut signals = Signals::new([SIGINT, SIGTERM])?;
        thread::spawn(move || {
            let mut count = 0u32;
            for _ in signals.forever() {
                count += 1;
                match count {
                    1 => {
                        mode_for_handler.store(MODE_GRACEFUL, Ordering::SeqCst);
                        tracing::info!("
                            Gracefully shutdown. Finishing in-flight files (2 more signals to \
                            abort now)"
                        );
                    }
                    2 => {
                        tracing::info!("
                            Gracefully shutdown. Finishing in-flight files (one more signal to \
                            abort now)"
                        );
                    }
                    _ => {
                        mode_for_handler.store(MODE_FORCE, Ordering::SeqCst);
                        if count == 3 {
                            tracing::info!("\
                            Aborting now; in-flight progress is discarded."
                            );
                        }
                    }
                }
            }
        });

        Ok(Self { mode })
    }

    /// Handle without signal handlers, for embedding and tests.
    pub fn detached() -> Self {
        Self {
            mode: Arc::new(AtomicU8::new(MODE_RUNNING)),
        }
    }

    /// Request a graceful stop (finish in-flight work, stop between files).
    pub fn request_graceful(&self) {
        self.mode.store(MODE_GRACEFUL, Ordering::SeqCst);
    }

    /// Request an immediate abort (discard in-flight progress; see `check_in_flight`).
    pub fn request_force(&self) {
        self.mode.store(MODE_FORCE, Ordering::SeqCst);
    }

    pub fn is_force(&self) -> bool {
        self.mode.load(Ordering::SeqCst) == MODE_FORCE
    }
    
    pub fn is_graceful(&self) -> bool { self.mode.load(Ordering::SeqCst) == MODE_GRACEFUL }

    pub fn is_interrupted(&self) -> bool {
        let mode = self.mode.load(Ordering::SeqCst);
        mode == MODE_GRACEFUL || mode == MODE_FORCE
    }


    /// Stop before starting a new unit of work (file, group, tar entry, …).
    pub fn check_between_files(&self) -> Result<()> {
        match self.mode.load(Ordering::SeqCst) {
            MODE_RUNNING => Ok(()),
            MODE_GRACEFUL | MODE_FORCE => Err(Error::Interrupted),
            v => panic!("Got unexpected Shutdown Value of {v}"),
        }
    }

    /// Abort long-running work immediately (force only). Only raises variant Interrupted.
    pub fn check_in_flight(&self) -> Result<()> {
        if self.is_force() {
            Err(Error::Interrupted)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graceful_request_stops_between_units_not_in_flight() {
        let s = Shutdown::detached();
        assert!(!s.is_interrupted());
        assert!(s.check_between_files().is_ok());
        assert!(s.check_in_flight().is_ok());

        s.request_graceful();
        assert!(s.is_graceful());
        assert!(s.is_interrupted());
        assert!(s.check_between_files().is_err());
        assert!(s.check_in_flight().is_ok());
    }

    #[test]
    fn force_request_aborts_everywhere() {
        let s = Shutdown::detached();

        s.request_force();
        assert!(s.is_force());
        assert!(s.is_interrupted());
        assert!(s.check_between_files().is_err());
        assert!(s.check_in_flight().is_err());
    }
}
