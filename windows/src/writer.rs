//! A background thread that writes sent runs to the device, oldest first.
//!
//! Writing each batch on the request thread left the device idle while the
//! core prepared data and the core idle while the device wrote. It follows the
//! Linux and macOS adapters' queue in
//! `fuse/src/device.rs`: one thread, FIFO order, so overlapping runs reach the
//! device in the order sent; and the first failed write is kept and returned
//! by every later `send` and `drain`, which faults the volume before any root
//! is written. The queue is bounded twice: a request waits for room in
//! `MAX_QUEUED` before it takes the volume lock (`Backlog::wait`), and `send`
//! itself waits at `HARD_QUEUED`, with the lock held.
//! `Disk` keeps a run's blocks in its cache until the run is reported done, so
//! reads never miss bytes still queued.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::cache::MAX_DIRTY;

/// The most that write requests queue: two `cache::MAX_DIRTY` batches, so the
/// request thread fills the next batch while the device writes the last. A
/// request waits until one of the two is free.
pub const MAX_QUEUED: usize = 2 * MAX_DIRTY;
/// Bytes queued before `send` waits. Requests wait for room first, so only
/// one request that sends more than the difference gets here. Waiting in
/// `send` holds the volume lock: when every batch of a stream waited there, a
/// `stat` during a 1.5 GiB write took 13 ms against 0.2 ms idle.
const HARD_QUEUED: usize = 32 << 20;

pub struct Writer {
    queue: Arc<Queue>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Queue {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// Runs not yet written, oldest first; the front one may be in progress.
    runs: VecDeque<(u64, Vec<u8>)>,
    bytes: usize,
    /// Written runs, for the cache to release.
    done: Vec<(u64, usize)>,
    failure: Option<(io::ErrorKind, Option<i32>)>,
    closed: bool,
    /// Calls, bytes, and time spent writing.
    stats: (u64, u64, Duration),
}

impl Queue {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.changed
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner)
    }
}

fn failed((kind, code): (io::ErrorKind, Option<i32>)) -> io::Error {
    code.map_or_else(|| kind.into(), io::Error::from_raw_os_error)
}

impl Writer {
    /// Start the thread. `write` writes one whole run at an offset.
    pub fn spawn(
        mut write: impl FnMut(u64, &[u8]) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Self> {
        let queue = Arc::new(Queue::default());
        let worker = Arc::clone(&queue);
        let thread = std::thread::Builder::new()
            .name("carryfs-writer".into())
            .spawn(move || {
                let mut state = worker.lock();
                loop {
                    let Some((offset, data)) = state.runs.front() else {
                        if state.closed {
                            return;
                        }
                        state = worker.wait(state);
                        continue;
                    };
                    // The run stays queued while it is written, so `drain`
                    // waits for it; nothing else removes the front run.
                    let (offset, len) = (*offset, data.len());
                    let data = std::mem::take(&mut state.runs[0].1);
                    drop(state);
                    let start = Instant::now();
                    // A panic is a failed write. Unwinding out of this thread
                    // would leave the run queued, and `send` and `drain`
                    // waiting for it with the volume lock held.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        write(offset, &data)
                    }))
                    .unwrap_or_else(|_| Err(io::Error::other("device writer panicked")));
                    let elapsed = start.elapsed();
                    state = worker.lock();
                    state.runs.pop_front();
                    state.bytes -= len;
                    state.stats.0 += 1;
                    state.stats.1 += len as u64;
                    state.stats.2 += elapsed;
                    state.done.push((offset, len));
                    if let Err(error) = result {
                        state
                            .failure
                            .get_or_insert((error.kind(), error.raw_os_error()));
                        // Later runs are dropped; the volume faults on the next
                        // send or drain, and a fault discards unpublished work.
                        let dropped: Vec<_> = state.runs.drain(..).collect();
                        state
                            .done
                            .extend(dropped.iter().map(|(offset, data)| (*offset, data.len())));
                        state.bytes = 0;
                    }
                    worker.changed.notify_all();
                }
            })?;
        Ok(Self {
            queue,
            thread: Some(thread),
        })
    }

    /// Queue a run, waiting while `HARD_QUEUED` bytes are ahead of it.
    pub fn send(&self, offset: u64, data: Vec<u8>) -> io::Result<()> {
        let mut state = self.queue.lock();
        while state.failure.is_none()
            && !state.runs.is_empty()
            && state.bytes + data.len() > HARD_QUEUED
        {
            state = self.queue.wait(state);
        }
        if let Some(failure) = state.failure {
            return Err(failed(failure));
        }
        state.bytes += data.len();
        state.runs.push_back((offset, data));
        self.queue.changed.notify_all();
        Ok(())
    }

    /// Wait until every queued run is written, or report the first failure.
    pub fn drain(&self) -> io::Result<()> {
        let mut state = self.queue.lock();
        while state.failure.is_none() && !state.runs.is_empty() {
            state = self.queue.wait(state);
        }
        state.failure.map_or(Ok(()), |failure| Err(failed(failure)))
    }

    /// Runs written (or dropped after a failure) since the last call.
    pub fn take_done(&self) -> Vec<(u64, usize)> {
        std::mem::take(&mut self.queue.lock().done)
    }

    /// Calls, bytes, and time the thread spent writing.
    pub fn stats(&self) -> (u64, u64, Duration) {
        self.queue.lock().stats
    }

    /// The queue, for request threads to wait on outside the volume lock.
    pub fn backlog(&self) -> Backlog {
        Backlog(Arc::clone(&self.queue))
    }
}

/// A writer's queue as a request thread sees it before taking the volume lock.
#[derive(Clone)]
pub struct Backlog(Arc<Queue>);

impl Backlog {
    /// Wait until the queue has room for the batch this request may complete.
    /// A failed or stopped writer ends the wait; the request's own `send`
    /// reports it.
    pub fn wait(&self) {
        let mut state = self.0.lock();
        while state.failure.is_none() && !state.closed && state.bytes > MAX_QUEUED - MAX_DIRTY {
            state = self.0.wait(state);
        }
    }
}

impl Drop for Writer {
    /// Write what is queued, then stop the thread.
    fn drop(&mut self) {
        self.queue.lock().closed = true;
        self.queue.changed.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    type Log = Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

    fn recording() -> (Writer, Log) {
        let log: Log = Arc::default();
        let sink = Arc::clone(&log);
        let writer = Writer::spawn(move |offset, data| {
            sink.lock().unwrap().push((offset, data.to_vec()));
            Ok(())
        })
        .unwrap();
        (writer, log)
    }

    #[test]
    fn runs_are_written_in_the_order_sent() {
        let (writer, log) = recording();
        writer.send(0, vec![1; 4096]).unwrap();
        writer.send(0, vec![2; 4096]).unwrap();
        writer.send(8192, vec![3; 4096]).unwrap();
        writer.drain().unwrap();
        let log = log.lock().unwrap();
        let order: Vec<_> = log
            .iter()
            .map(|(offset, data)| (*offset, data[0]))
            .collect();
        assert_eq!(order, [(0, 1), (0, 2), (8192, 3)]);
        let mut done = writer.take_done();
        done.sort();
        assert_eq!(done, [(0, 4096), (0, 4096), (8192, 4096)]);
        assert!(writer.take_done().is_empty());
    }

    #[test]
    fn drain_waits_for_a_run_in_progress() {
        let (started, wait) = mpsc::channel();
        let (release, gate) = mpsc::channel::<()>();
        let writer = Writer::spawn(move |_, _| {
            started.send(()).unwrap();
            gate.recv().unwrap();
            Ok(())
        })
        .unwrap();
        writer.send(0, vec![0; 4096]).unwrap();
        wait.recv().unwrap();
        // The run is being written, so it is not done yet.
        assert!(writer.take_done().is_empty());
        release.send(()).unwrap();
        writer.drain().unwrap();
        assert_eq!(writer.take_done(), [(0, 4096)]);
    }

    #[test]
    fn a_failed_write_fails_every_later_send_and_drain() {
        // The first write waits until all three runs are queued; otherwise
        // the failure can already be recorded when the third is sent.
        let (release, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let writer = Writer::spawn(move |offset, _| {
            if offset == 0 {
                gate.lock().unwrap().recv().unwrap();
            }
            if offset == 4096 {
                Err(io::Error::other("medium error"))
            } else {
                Ok(())
            }
        })
        .unwrap();
        writer.send(0, vec![0; 4096]).unwrap();
        writer.send(4096, vec![0; 4096]).unwrap();
        writer.send(8192, vec![0; 4096]).unwrap();
        release.send(()).unwrap();
        assert!(writer.drain().is_err());
        assert!(writer.send(0, vec![0; 4096]).is_err());
        assert!(writer.drain().is_err());
        // Every run is released from the cache, written or dropped.
        assert_eq!(writer.take_done().len(), 3);
    }

    /// A panic in the write counts as a failed write. Otherwise the run it
    /// was writing stays queued and every `drain` waits for a thread that is
    /// gone, with the volume lock held.
    #[test]
    fn a_panicking_write_fails_drain_instead_of_hanging() {
        let writer = Arc::new(Writer::spawn(|_, _| panic!("injected")).unwrap());
        writer.send(0, vec![0; 4096]).unwrap();
        let drainer = Arc::clone(&writer);
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || done.send(drainer.drain().is_err()).unwrap());
        assert_eq!(finished.recv_timeout(Duration::from_secs(2)), Ok(true));
        assert!(writer.send(0, vec![0; 4096]).is_err());
    }

    #[test]
    fn a_request_waits_for_the_backlog_and_a_sender_does_not() {
        let (release, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let writer = Writer::spawn(move |_, _| {
            gate.lock().unwrap().recv().unwrap();
            Ok(())
        })
        .unwrap();
        let backlog = writer.backlog();
        backlog.wait();
        // One batch queued leaves room for the next.
        writer.send(0, vec![0; MAX_DIRTY]).unwrap();
        backlog.wait();
        // With both queued a request waits, and a send still returns at once.
        writer.send(0, vec![0; MAX_DIRTY]).unwrap();
        writer.send(0, vec![0; 4096]).unwrap();
        let (passed, finished) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            backlog.wait();
            passed.send(()).unwrap();
        });
        assert!(finished.recv_timeout(Duration::from_millis(100)).is_err());
        release.send(()).unwrap();
        release.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
        release.send(()).unwrap();
        thread.join().unwrap();
        writer.drain().unwrap();
    }

    #[test]
    fn a_failed_or_stopped_writer_ends_the_wait_for_the_backlog() {
        let (release, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let writer = Writer::spawn(move |_, _| {
            gate.lock().unwrap().recv().unwrap();
            Err(io::Error::other("medium error"))
        })
        .unwrap();
        let backlog = writer.backlog();
        writer.send(0, vec![0; MAX_QUEUED + 4096]).unwrap();
        let waiter = backlog.clone();
        let (passed, finished) = mpsc::channel();
        std::thread::spawn(move || {
            waiter.wait();
            passed.send(()).unwrap();
        });
        assert!(finished.recv_timeout(Duration::from_millis(100)).is_err());
        release.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(writer.drain().is_err());
        drop(writer);
        backlog.wait();
    }

    #[test]
    fn a_sender_waits_at_the_queue_bound() {
        let (release, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let writer = Arc::new(
            Writer::spawn(move |_, _| {
                gate.lock().unwrap().recv().unwrap();
                Ok(())
            })
            .unwrap(),
        );
        writer.send(0, vec![0; HARD_QUEUED]).unwrap();
        let sender = Arc::clone(&writer);
        let (sent, finished) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            sender.send(0, vec![0; 4096]).unwrap();
            sent.send(()).unwrap();
        });
        assert!(finished.recv_timeout(Duration::from_millis(100)).is_err());
        release.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
        release.send(()).unwrap();
        thread.join().unwrap();
        writer.drain().unwrap();
    }

    #[test]
    fn dropping_the_writer_writes_what_is_queued() {
        let (writer, log) = recording();
        for index in 0..10 {
            writer.send(index * 4096, vec![0; 4096]).unwrap();
        }
        drop(writer);
        assert_eq!(log.lock().unwrap().len(), 10);
    }
}
