//! PID 1 duties for containers started without `--init`.
//!
//! As PID 1, PHPHub inherits every orphaned process in the container (for
//! example `timeout`, when the shell that started it from PHP's `exec()`
//! exits first). If nothing reaps them, they stay as `<defunct>` entries.
//!
//! Reaping with `waitpid(-1)` in the server itself would race with PHP:
//! `proc_close()`, `exec()` and `system()` wait for their own child by pid,
//! and an orphan cannot be told apart from such a child. So we do what tini
//! does. PID 1 forks once, before any thread exists. The child runs the
//! server (or the passthrough command) as usual. The parent only forwards
//! signals to that child and reaps whatever exits. PHP's children stay
//! children of the server process and keep their exit codes.

use std::io::Error;

/// Signals that must stay deliverable to the thread that caused them, or
/// that a background process group needs (as in tini).
const SYNC: [libc::c_int; 9] = [
    libc::SIGFPE,
    libc::SIGILL,
    libc::SIGSEGV,
    libc::SIGBUS,
    libc::SIGABRT,
    libc::SIGTRAP,
    libc::SIGSYS,
    libc::SIGTTIN,
    libc::SIGTTOU,
];

/// Returns at once unless this process is PID 1. As PID 1 it returns only
/// in the forked child; the parent stays here until that child exits and
/// then exits with the child's status.
pub fn become_init_if_pid1(quiet: bool) {
    if unsafe { libc::getpid() } != 1 {
        return;
    }
    unsafe {
        // Block the signals before forking so none sent in between is lost.
        let mut set: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut set);
        for s in SYNC {
            libc::sigdelset(&mut set, s);
        }
        libc::sigprocmask(libc::SIG_SETMASK, &set, &mut old);

        match libc::fork() {
            -1 => {
                let err = Error::last_os_error();
                libc::sigprocmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
                crate::logln!("warning: running as PID 1 but cannot fork a reaper: {err}; exited child processes will not be reaped");
            }
            0 => {
                libc::sigprocmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
                take_terminal();
            }
            child => supervise(child, &set, quiet),
        }
    }
}

/// In the child: own process group, and the terminal's foreground group if
/// there is one. Ctrl-C then reaches the server once, from the terminal,
/// instead of a second time through the parent.
unsafe fn take_terminal() {
    unsafe {
        libc::setpgid(0, 0);
        if libc::isatty(libc::STDIN_FILENO) == 1 {
            let prev = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp());
            libc::signal(libc::SIGTTOU, prev);
        }
    }
}

/// In PID 1: forward signals to `child`, reap every exited process, and
/// exit with the child's status once it is gone.
unsafe fn supervise(child: libc::pid_t, set: &libc::sigset_t, quiet: bool) -> ! {
    if !quiet {
        crate::logln!("running as PID 1: forwarding signals to pid {child} and reaping exited processes");
    }
    loop {
        let mut sig: libc::c_int = 0;
        if unsafe { libc::sigwait(set, &mut sig) } != 0 {
            continue;
        }
        if sig != libc::SIGCHLD {
            // ESRCH (child already gone) is fine: SIGCHLD comes next.
            unsafe { libc::kill(child, sig) };
            continue;
        }
        // One SIGCHLD can stand for several exits.
        loop {
            let mut status: libc::c_int = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            if pid == child {
                let code = if libc::WIFEXITED(status) {
                    libc::WEXITSTATUS(status)
                } else if libc::WIFSIGNALED(status) {
                    128 + libc::WTERMSIG(status)
                } else {
                    continue;
                };
                std::process::exit(code);
            }
        }
    }
}
