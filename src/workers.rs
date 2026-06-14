use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Condvar;
use parking_lot::Mutex;

use crate::error::Result;

#[derive(Clone)]
pub struct WorkerPool {
    shutdown: Arc<AtomicBool>,
    max_active: Arc<Mutex<usize>>,
    cv: Arc<Condvar>,
}

impl WorkerPool {
    pub fn new(max_active: usize) -> Self {
        Self {
            shutdown: Arc::new(AtomicBool::new(false)),
            max_active: Arc::new(Mutex::new(max_active)),
            cv: Arc::new(Condvar::new()),
        }
    }

    pub fn set_max_active(&self, n: usize) {
        *self.max_active.lock() = n;
        self.cv.notify_all();
    }

    pub fn max_active(&self) -> usize {
        *self.max_active.lock()
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.cv.notify_all();
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    pub fn acquire_slot(&self) -> Result<WorkerSlot<'_>> {
        if self.shutdown_requested() {
            return Err(crate::error::Error::Other(anyhow::anyhow!("shutdown requested")));
        }
        Ok(WorkerSlot { pool: self })
    }
}

pub struct WorkerSlot<'a> {
    pool: &'a WorkerPool,
}

impl Drop for WorkerSlot<'_> {
    fn drop(&mut self) {
        self.pool.cv.notify_one();
    }
}
