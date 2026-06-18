use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
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
                        eprintln!(
                            "Gracefully shutdown. Finishing in-flight files (2 more signals to abort now)"
                        );
                    }
                    2 => {
                        eprintln!(
                            "Gracefully shutdown. Finishing in-flight files (one more signal to abort now)"
                        );
                    }
                    _ => {
                        mode_for_handler.store(MODE_FORCE, Ordering::SeqCst);
                        eprintln!("Aborting now; in-flight progress is discarded.");
                    }
                }
            }
        });

        Ok(Self { mode })
    }

    pub fn is_force(&self) -> bool {
        self.mode.load(Ordering::SeqCst) == MODE_FORCE
    }

    /// Stop before starting a new unit of work (file, group, tar entry, …).
    pub fn check_between_files(&self) -> Result<()> {
        match self.mode.load(Ordering::SeqCst) {
            MODE_RUNNING => Ok(()),
            MODE_GRACEFUL | MODE_FORCE | _ => Err(Error::Interrupted),
        }
    }

    /// Abort long-running work immediately (force only).
    pub fn check_in_flight(&self) -> Result<()> {
        if self.is_force() {
            Err(Error::Interrupted)
        } else {
            Ok(())
        }
    }
}
