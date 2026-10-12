//! What a WinFsp request waits for outside the volume lock: room in the
//! background writer's queue, and a read-ahead window the request continues
//! into. The device (`device.rs`) fills them; `fs.rs` waits on them. Platform
//! independent, so the operations compile and test on any host.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::writer::Backlog;

/// A window being read ahead, for the request that will continue into it.
/// Set once its thread has finished, whatever the result.
#[derive(Clone)]
pub struct Window(pub(crate) Arc<AtomicBool>);

impl Window {
    pub fn arrived(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// What request threads share with the device outside the volume lock.
#[derive(Default)]
pub(crate) struct Shared {
    /// Held to mark a window arrived and to wait for one.
    pub(crate) arriving: Mutex<()>,
    pub(crate) arrived: Condvar,
    /// The window a read continuing the last data read would wait for.
    pub(crate) next: Mutex<Option<Window>>,
}

/// What a request waits for before it takes the volume lock. The core waits
/// for the device inside its calls, with the lock held and every other request
/// behind it: during a 1.5 GiB write a `stat` took 13 ms against 0.2 ms idle,
/// and 3.7 ms during a read.
#[derive(Clone)]
pub struct Waits {
    pub(crate) backlog: Option<Backlog>,
    pub(crate) shared: Arc<Shared>,
}

impl Waits {
    /// Before a write: until the background writer has room in its queue.
    pub fn before_write(&self) {
        if let Some(backlog) = &self.backlog {
            backlog.wait();
        }
    }

    /// The window that a read continuing the last data read would wait for,
    /// if the stream had reached the end of its current one. Taken, so it
    /// describes one read; call it under the volume lock.
    pub fn take_next(&self) -> Option<Window> {
        self.shared
            .next
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Before the read that continues into `window`: until it has been read.
    pub fn arrival(&self, window: &Window) {
        let mut held = self
            .shared
            .arriving
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while !window.arrived() {
            held = self
                .shared
                .arrived
                .wait(held)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_request_waits_for_its_window_to_arrive() {
        let waits = Waits {
            backlog: None,
            shared: Arc::default(),
        };
        let window = Window(Arc::new(AtomicBool::new(false)));
        *waits.shared.next.lock().unwrap() = Some(window.clone());
        assert!(waits.take_next().is_some());
        assert!(waits.take_next().is_none());
        let (waiter, awaited) = (waits.clone(), window.clone());
        let (passed, finished) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            waiter.arrival(&awaited);
            passed.send(()).unwrap();
        });
        assert!(finished.recv_timeout(Duration::from_millis(100)).is_err());
        {
            let _held = waits.shared.arriving.lock().unwrap();
            window.0.store(true, Ordering::Release);
            waits.shared.arrived.notify_all();
        }
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
        thread.join().unwrap();
        waits.arrival(&window);
        waits.before_write();
    }
}
