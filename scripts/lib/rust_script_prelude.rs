//! Process-wide conventions shared by Hermit's standalone Rust scripts.

/// Make a standalone script tolerate a downstream reader closing the pipe early
/// (`prog | head`, `prog | grep -q`, …) by terminating cleanly on `SIGPIPE`.
///
/// Rust installs `SIG_IGN` for `SIGPIPE` at startup, which turns a closed reader
/// into an `EPIPE` error and makes `print!`/`println!` panic with a backtrace.
/// Restoring `SIG_DFL` avoids the panic, but the kernel then terminates the
/// process *by the signal*, which the shell reports as exit status 141
/// (`128 + SIGPIPE`). Under `set -o pipefail` a status of 141 fails the whole
/// pipeline, so a routine `manifest-cli list | head` looks like an error.
///
/// Instead we install a tiny handler that exits with status 0: a consumer that
/// stops reading early is normal, expected termination, not a failure of the
/// producer. This mirrors the intent of traditional CLI tools while staying
/// friendly to `pipefail` pipelines.
pub fn init() {
    #[cfg(unix)]
    {
        const SIGPIPE: i32 = 13;

        unsafe extern "C" {
            fn signal(signal: i32, handler: usize) -> usize;
            fn _exit(code: i32) -> !;
        }

        // Signal handler: a downstream consumer closed the pipe. Treat it as a
        // clean, expected end of output.
        //
        // SAFETY: `_exit` is async-signal-safe (it does not flush stdio or run
        // atexit handlers), which is exactly what a signal handler is allowed to
        // call. We deliberately skip stdio flushing: the pipe is already gone.
        extern "C" fn on_sigpipe(_signum: i32) {
            unsafe { _exit(0) }
        }

        // SAFETY: installs a process-wide disposition once at startup, before
        // the script creates threads or produces output.
        unsafe {
            signal(SIGPIPE, on_sigpipe as *const () as usize);
        }
    }
}
