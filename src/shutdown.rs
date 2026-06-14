use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::error::Result;

pub struct Shutdown {
    flag: Arc<AtomicBool>,
}

impl Shutdown {
    pub fn install() -> Result<Self> {
        let flag = Arc::new(AtomicBool::new(false));
        let int_flag = flag.clone();
        let term_flag = flag.clone();
        signal_hook::flag::register(signal_hook::consts::SIGINT, int_flag)?;
        signal_hook::flag::register(signal_hook::consts::SIGTERM, term_flag)?;
        Ok(Self { flag })
    }

    pub fn requested(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}
