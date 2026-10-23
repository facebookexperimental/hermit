/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/// Gets the next event from the event stream.
///
/// # Example
///
/// ```ignore
/// let event = next_event!(guest, Read);
/// ```
///
/// The return type in this example will be `Result<ReadEvent, Errno>`.
macro_rules! next_event {
    ($guest:expr, $event:ident) => {{
        let thread = $guest.tid();
        let recorded = $guest
            .thread_state_mut()
            .next_event()
            .unwrap_or_else(|error| {
                panic!(
                    "Replay event stream ended unexpectedly on thread {} while expecting {} after syscall event {}: {}",
                    thread,
                    stringify!($event),
                    $guest.thread_state().count,
                    error,
                )
            });
        let syscall_count = $guest.thread_state().count;
        recorded.event.map(|event| match event {
            $crate::event::SyscallEvent::$event(event) => event,
            event => panic!(
                "Replay event mismatch on thread {} at syscall event {}: expected {}, found {:?}. The recording and replay handlers may be out of sync",
                thread,
                syscall_count,
                stringify!($event),
                event,
            ),
        })
    }};
}
