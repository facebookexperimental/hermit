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
    ($guest:expr, $event:ident) => {
        $guest
            .thread_state_mut()
            .next_event()
            .unwrap()
            .event
            .map(|e| match e {
                $crate::event::SyscallEvent::$event(event) => event,
                event => panic!("Got unexpected event: {:?}", event),
            })
    };
}
