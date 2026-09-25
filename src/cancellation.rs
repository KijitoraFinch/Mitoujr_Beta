use anyhow::{Result, ensure};
use std::sync::atomic::{AtomicI32, Ordering};

static SIGNAL: AtomicI32 = AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn interrupted(signal: libc::c_int) {
    SIGNAL.store(signal, Ordering::Relaxed);
}

#[cfg(unix)]
pub fn install() -> Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the handler only stores a lock-free atomic and never touches
        // allocator, locks, or I/O. This is installed before worker threads start.
        let previous =
            unsafe { libc::signal(signal, interrupted as *const () as libc::sighandler_t) };
        ensure!(
            previous != libc::SIG_ERR,
            "install signal handler: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

pub fn check() -> Result<()> {
    let signal = SIGNAL.load(Ordering::Relaxed);
    ensure!(signal == 0, "operation interrupted by signal {signal}");
    Ok(())
}

pub fn exit_code() -> Option<u8> {
    let signal = SIGNAL.load(Ordering::Relaxed);
    if signal == 0 {
        None
    } else {
        Some(128 + signal as u8)
    }
}
