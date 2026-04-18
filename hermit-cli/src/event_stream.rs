/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use reverie::Tid;
use reverie::syscalls::Displayable;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use crate::event::Event;

/// An event to help with debugging, but is not actually necessary for the
/// functionality of record/replay.
#[derive(Debug, Serialize, Deserialize)]
pub struct DebugEvent {
    /// The raw syscall.
    syscall: (Sysno, SyscallArgs),

    /// The pretty, displayable version of the syscall.
    pretty: String,
}

impl DebugEvent {
    /// Constructs a new `DebugEvent`.
    pub fn new<M: MemoryAccess>(syscall: Syscall, memory: &M) -> Self {
        Self {
            syscall: syscall.into_parts(),
            pretty: format!("{}", syscall.display(memory)),
        }
    }

    /// Returns the syscall associated with this debug event.
    pub fn syscall(&self) -> Syscall {
        Syscall::from_raw(self.syscall.0, self.syscall.1)
    }
}

impl fmt::Display for DebugEvent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.pretty)
    }
}

/// A stream of syscall events.
#[derive(Serialize, Deserialize)]
pub struct EventReader {
    // The file where events are stored.
    //
    // NOTE: This field isn't serializable/deserializable, so we have to skip it
    // for now. With an in-guest backend, we'd need to implement this manually
    // to support state migration.
    #[serde(skip, default = "default_reader")]
    reader: io::BufReader<fs::File>,

    // The file where raw syscalls are stored. This is used for detecting
    // desynchronization bugs. This is stored in a separate file so that we can
    // easily turn this on or off to shift the balance on debuggability and
    // performance.
    #[serde(skip, default = "default_reader")]
    debug_events: io::BufReader<fs::File>,

    // The number of events read so far. Useful for debugging purposes.
    pub count: u64,
}

fn default_reader() -> io::BufReader<fs::File> {
    unimplemented!("Serialization is not yet implemented")
}

impl EventReader {
    /// Opens an existing event stream.
    pub fn open(path: &Path, thread_id: Tid) -> io::Result<Self> {
        Ok(Self {
            reader: io::BufReader::new(fs::File::open(
                path.join("thread").join(thread_id.to_string()),
            )?),
            debug_events: io::BufReader::new(fs::File::open(
                path.join("thread").join(format!("{}.debug", thread_id)),
            )?),
            count: 0,
        })
    }

    /// Reads the next event from the stream. Returns an error if there are no
    /// more events to consume.
    pub fn next_event(&mut self) -> Result<Event, bincode::error::DecodeError> {
        bincode::serde::decode_from_std_read(&mut self.reader, bincode::config::legacy())
    }

    /// Reads the next syscall from the syscall stream.
    pub fn next_debug_event(&mut self) -> Result<DebugEvent, bincode::error::DecodeError> {
        let debug_event = bincode::serde::decode_from_std_read(
            &mut self.debug_events,
            bincode::config::legacy(),
        )?;
        self.count += 1;
        Ok(debug_event)
    }
}

impl Default for EventReader {
    fn default() -> Self {
        panic!("Thread state should be explicitly initialized in init_thread_state")
    }
}

/// A stream of syscall events.
#[derive(Serialize, Deserialize)]
pub struct EventWriter {
    // The file where events are stored.
    //
    // NOTE: This field isn't serializable/deserializable, so we have to skip it
    // for now. With an in-guest backend, we'd need to implement this manually
    // to support state migration.
    #[serde(skip, default = "default_writer")]
    writer: io::BufWriter<fs::File>,

    // The file where syscalls are stored. This is used for debugging purposes.
    #[serde(skip, default = "default_writer")]
    debug_events: io::BufWriter<fs::File>,
}

fn default_writer() -> io::BufWriter<fs::File> {
    unimplemented!("Serialization is not yet implemented")
}

impl EventWriter {
    /// Creates a new event stream.
    pub fn create(path: &Path, thread_id: Tid) -> io::Result<Self> {
        let path = path.join("thread");

        fs::create_dir_all(&path)?;

        Ok(Self {
            writer: io::BufWriter::new(fs::File::create(path.join(thread_id.to_string()))?),
            debug_events: io::BufWriter::new(fs::File::create(
                path.join(format!("{}.debug", thread_id)),
            )?),
        })
    }

    /// Writes an event to the end of the stream.
    pub fn push_event(&mut self, event: Event) -> Result<(), bincode::error::EncodeError> {
        bincode::serde::encode_into_std_write(&event, &mut self.writer, bincode::config::legacy())
            .map(|_| ())
    }

    /// Writes a debug event to the end of the stream.
    pub fn push_debug_event(
        &mut self,
        event: DebugEvent,
    ) -> Result<(), bincode::error::EncodeError> {
        bincode::serde::encode_into_std_write(
            &event,
            &mut self.debug_events,
            bincode::config::legacy(),
        )
        .map(|_| ())
    }
}

impl Default for EventWriter {
    fn default() -> Self {
        panic!("Thread state should be explicitly initialized in init_thread_state")
    }
}
