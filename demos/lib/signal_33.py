"""Give every `hermit` a QEMU demo starts the same signal 33 disposition.

This module runs on the host only. It is deliberately not part of
demo_common.py, because demo_common.py is one of the two sources copied into the
guest (GUEST_CONTROLLER_SOURCES): any change to that file's bytes changes what
the guest executes, and with it the snapshot and the Hermit log that demos 5 and
6 compare with a saved reference run.
"""

import threading


def settle_signal_33_disposition() -> None:
    """Make glibc's first-thread change to signal 33 happen before any `hermit`.

    Hermit passes the `SigIgn` field of the guest's `/proc/self/status` through
    from whatever started `hermit`
    (https://github.com/rrnewton/hermit/issues/3441). QEMU reads that file, so
    the field reaches the Hermit log that demos 5 and 6 compare.

    Signal 33 is one of glibc's two internal signals, and glibc refuses to let a
    program change it, so an inherited `SIG_IGN` passes through shells and
    interpreters unchanged. GNU make, for one, starts its recipes with signal 33
    ignored. The exception is glibc itself: when a process creates its first
    thread, glibc installs its own handler for signal 33, and every child
    started after that runs with the default disposition. Demos 5 and 6 start
    their first thread (the output copier) just after their first `hermit`. So
    when the script itself started with signal 33 ignored, the first boot or
    resume of an invocation saw it ignored and the second did not. On
    2026-09-30 that made the second boot and the second resume of a first
    invocation print PARTIAL, on exactly this bit.

    Starting and joining one thread before the first `hermit` makes glibc's
    change happen first. With glibc, every `hermit` this process starts then
    runs with the default disposition for signal 33, however the script was
    started.
    """
    thread = threading.Thread(target=lambda: None, name="settle-signal-33")
    thread.start()
    thread.join()
