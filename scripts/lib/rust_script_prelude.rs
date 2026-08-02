//! Process-wide conventions shared by Hermit's standalone Rust scripts.

/// Restore the Unix default for `SIGPIPE` before a script writes output.
///
/// Rust ignores `SIGPIPE`, which turns a downstream reader closing early into
/// an `EPIPE` error and makes `print!`/`println!` panic. Traditional CLI tools
/// instead terminate quietly when commands such as `head` close the pipe.
pub fn init() {
    #[cfg(unix)]
    {
        const SIGPIPE: i32 = 13;
        const SIG_DFL: usize = 0;

        unsafe extern "C" {
            fn signal(signal: i32, handler: usize) -> usize;
        }

        // SAFETY: SIG_DFL is the process-wide POSIX default disposition. This
        // runs once at startup, before the script creates threads or handlers.
        unsafe {
            signal(SIGPIPE, SIG_DFL);
        }
    }
}
