//! Stop on SIGTERM or SIGINT, as `kill`, `systemctl stop`, and shutdown send,
//! by publishing and unmounting, instead of dying with up to a second of
//! changes unpublished and a mount that fails every request.

use std::io;

/// Block SIGTERM and SIGINT in this thread and every thread it starts, so
/// that only `watch` receives them. Call it before any thread starts.
pub fn block() -> io::Result<libc::sigset_t> {
    // SAFETY: `sigset_t` is plain data that `sigemptyset` initializes, and
    // the calls only read and write the set they are given.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        match libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) {
            0 => Ok(set),
            error => Err(io::Error::from_raw_os_error(error)),
        }
    }
}

/// Run `stop` on a thread of its own when the first blocked signal arrives.
pub fn watch(signals: libc::sigset_t, stop: impl FnOnce() + Send + 'static) -> io::Result<()> {
    std::thread::Builder::new()
        .name("carryfs-signal".into())
        .spawn(move || {
            let mut signal = 0;
            // SAFETY: `signals` is an initialized set and `signal` outlives the call.
            if unsafe { libc::sigwait(&signals, &mut signal) } != 0 {
                return;
            }
            let millis = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis());
            eprintln!("carryfs: signal {signal} at {millis} ms: publishing and unmounting");
            stop();
        })?;
    Ok(())
}
