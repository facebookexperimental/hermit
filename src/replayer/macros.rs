// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

/// Gets the next event from the event stream.
///
/// # Example
///
/// ```
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
