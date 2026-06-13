//! Crash diagnostics: a SIGSEGV/SIGBUS/SIGILL handler that prints a
//! symbolized Rust backtrace before the process dies, then re-raises the
//! signal with the default handler so a core dump is still produced.
//!
//! The systemd core dumps we captured only contained the null frame
//! (`ip 0` — a call through a null function pointer); this handler captures
//! the real Rust stack at the moment of the fault.

/// Install the crash handler (no-op on non-unix).
pub fn install() {
    #[cfg(unix)]
    unsafe {
        for sig in [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGABRT] {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler as *const () as usize;
            // SA_RESETHAND restores the default handler after this delivery,
            // so the re-raise below produces the usual core dump.
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESETHAND;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
    }
}

#[cfg(unix)]
extern "C" fn handler(sig: i32, _info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    // force_capture ignores RUST_BACKTRACE and always unwinds. Allocating in a
    // signal handler isn't async-signal-safe, but we're already crashing — a
    // best-effort backtrace is far more useful than the bare `ip 0` core.
    let bt = std::backtrace::Backtrace::force_capture();
    let name = match sig {
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGBUS => "SIGBUS",
        libc::SIGILL => "SIGILL",
        libc::SIGABRT => "SIGABRT",
        _ => "signal",
    };
    eprintln!("\n==== citrus crashed: {name} ({sig}) ====\n{bt}\n====");
    unsafe {
        libc::raise(sig);
    }
}
