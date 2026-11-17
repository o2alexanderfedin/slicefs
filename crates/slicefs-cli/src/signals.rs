//! SIGTERM, SIGINT and SIGHUP end a mount cleanly (FUSET-05).
//!
//! The handler only writes the signal number to a pipe (async-signal-safe).
//! A thread reads the pipe and calls `on_stop`, which unmounts the mount
//! point the same way as `slicefs unmount`. The FUSE session then ends
//! normally, so `destroy()` does the final commit and seals the pack. A
//! second signal ends the process at once.
//!
//! This uses a handler and not a blocked signal mask: child processes keep
//! a mask (the FUSE-T server must still stop on SIGTERM), but exec resets a
//! handler. A signal that is ignored at start (`nohup`, or SIGINT in a
//! background job) stays ignored.

use std::io;
use std::sync::atomic::{AtomicI32, Ordering};

/// The signals that end a mount.
pub const SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// Write end of the pipe, read by the handler.
static PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = PIPE_WRITE.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = sig as u8;
        // SAFETY: write(2) is async-signal-safe; `b` lives for the call.
        unsafe { libc::write(fd, &b as *const u8 as *const libc::c_void, 1) };
    }
}

/// Install the handlers and start the thread that serves them.
///
/// `on_stop(sig)` runs on the first signal and returns `true` if the unmount
/// started. On the next signal, the process exits with `128 + sig` and no
/// final commit. Call this only once in a process.
pub fn install<F>(mut on_stop: F) -> io::Result<()>
where
    F: FnMut(libc::c_int) -> bool + Send + 'static,
{
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in fds {
        // SAFETY: `fd` is a descriptor that pipe(2) just opened.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    PIPE_WRITE.store(fds[1], Ordering::SeqCst);

    for sig in SIGNALS {
        // SAFETY: a zeroed sigaction is a valid value; the pointers are valid
        // for each call.
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, std::ptr::null(), &mut old) != 0 {
                return Err(io::Error::last_os_error());
            }
            if old.sa_sigaction == libc::SIG_IGN {
                continue;
            }
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }

    let read_fd = fds[0];
    std::thread::Builder::new().name("signals".into()).spawn(move || {
        let mut stopping = false;
        loop {
            let mut b = 0u8;
            // SAFETY: reads one byte into `b`.
            let n = unsafe { libc::read(read_fd, &mut b as *mut u8 as *mut libc::c_void, 1) };
            if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if n <= 0 {
                return;
            }
            let sig = libc::c_int::from(b);
            if stopping {
                eprintln!("Signal {sig} again: exit now, without a final commit.");
                std::process::exit(128 + sig);
            }
            stopping = true;
            if on_stop(sig) {
                eprintln!("Signal {sig}: unmount, then a final commit.");
            } else {
                eprintln!(
                    "Signal {sig}: the unmount failed (busy?). Close the open files, \
                     or send the signal again to exit without a final commit."
                );
            }
        }
    })?;
    Ok(())
}
