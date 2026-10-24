/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The canonical models of open file descriptions that several guest
//! processes share, and the leases through which a process reaches them.
//!
//! A backend that runs Detcore inside each guest process cannot share one
//! `Arc` between processes, so a description that a fork shares lives here,
//! in the global state, and each process's handle is `Shared` (see
//! `crate::fd::OpenFileState`). A `DetFd` method on a shared handle takes the
//! model, runs its synchronous body on it, and publishes it back, inside one
//! method call.
//!
//! - Only the thread that holds the scheduler's serial grant may take, so
//!   access is exclusive and happens in canonical order.
//! - The global state keeps the canonical model while a lease is out and sends
//!   a copy. A publish replaces the model only when it names the current lease,
//!   so a late, duplicate or foreign publish is refused. If the holder's
//!   process dies mid-lease, the lease ends and its unpublished change is
//!   discarded.
//! - Models move in chunks of at most [`CHUNK_LEN`] bytes, because a transport
//!   frame is bounded and a model (a directory or procfs snapshot) is not.
//!
//! Production promotion currently selects random-device descriptions in shared
//! Detcore; the model and protocol remain generic. Committed models live until
//! GlobalState teardown, including after their last guest descriptor closes.
//! Memory therefore scales with distinct promoted OFDs and their model sizes,
//! not merely with currently open descriptors.
//!
//! Every message here is control traffic: it changes no clock, request slot,
//! scheduler membership or turn.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;

use crate::fd::OpenFileModel;
use crate::fd::SharedOpenFileChannel;
use crate::fd::SharedOpenFileError;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::OpenFileId;

/// The largest model chunk one message carries. Transport frames are limited
/// to 16 MiB. The chunk is much smaller because a guest process transfers it
/// inside the runtime's fixed callback heap, where the transport's own frame
/// buffers coexist with it.
pub const CHUNK_LEN: usize = 1 << 20;

/// One control request about a shared open file description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenFileControl {
    /// Start a lease on `id` and return the model's first chunk.
    Take {
        /// The shared open file description.
        id: OpenFileId,
    },
    /// Return the chunk of the leased model that starts at `offset`.
    TakeChunk {
        /// The shared open file description.
        id: OpenFileId,
        /// The lease, as [`OpenFileControlReply::Taken`] named it.
        sequence: u64,
        /// Where the chunk starts in the encoded model.
        offset: usize,
    },
    /// Append `bytes` at `offset` to the model being published under the
    /// lease. The last chunk commits the model and ends the lease.
    PublishChunk {
        /// The shared open file description.
        id: OpenFileId,
        /// The lease being published.
        sequence: u64,
        /// Where `bytes` start in the encoded model.
        offset: usize,
        /// This chunk of the encoded model.
        bytes: Vec<u8>,
        /// Whether this is the last chunk.
        last: bool,
    },
    /// Reserve a new identity for promotion. The holder publishes its local
    /// model through the ordinary chunked lease before exposing shared handles.
    Promote {
        /// The local description being promoted.
        id: OpenFileId,
    },
}

/// The answer to an [`OpenFileControl`] request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenFileControlReply {
    /// A lease started: its sequence, the encoded model's length, and its
    /// first chunk.
    Taken {
        /// Names the lease in every later message about it.
        sequence: u64,
        /// The encoded model's length in bytes.
        length: usize,
        /// The encoded model's first bytes, at most `CHUNK_LEN` (1 MiB).
        chunk: Vec<u8>,
    },
    /// A further chunk of the leased model.
    Chunk(Vec<u8>),
    /// A non-final publish chunk was accepted.
    Accepted,
    /// The model was committed and the lease ended.
    Published,
    /// A promotion lease started. Its initial model is not yet available.
    Promoting {
        /// Names the lease in every publish chunk.
        sequence: u64,
    },
}

fn encode_model(model: &OpenFileModel) -> Result<Vec<u8>, SharedOpenFileError> {
    bincode::serde::encode_to_vec(model, bincode::config::legacy())
        .map_err(|error| SharedOpenFileError(format!("encoding an open file model: {error}")))
}

fn decode_model(bytes: &[u8]) -> Result<OpenFileModel, SharedOpenFileError> {
    let (model, used) =
        bincode::serde::decode_from_slice::<OpenFileModel, _>(bytes, bincode::config::legacy())
            .map_err(|error| {
                SharedOpenFileError(format!("decoding an open file model: {error}"))
            })?;
    if used != bytes.len() {
        return Err(SharedOpenFileError(format!(
            "an open file model left {} undecoded bytes",
            bytes.len() - used
        )));
    }
    Ok(model)
}

fn chunk_at(bytes: &[u8], offset: usize) -> Result<Vec<u8>, SharedOpenFileError> {
    if offset > bytes.len() {
        return Err(SharedOpenFileError(format!(
            "chunk offset {offset} is past the model's {} bytes",
            bytes.len()
        )));
    }
    let end = bytes.len().min(offset + CHUNK_LEN);
    Ok(bytes[offset..end].to_vec())
}

/// Who holds a lease: the process, for settlement at its death, and the
/// thread that took it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Holder {
    process: DetPid,
    thread: DetTid,
}

#[derive(Debug)]
struct Lease {
    holder: Holder,
    sequence: u64,
    /// The encoded copy being taken.
    outgoing: Vec<u8>,
    /// The encoded model being published, chunk by chunk.
    incoming: Vec<u8>,
}

#[derive(Debug)]
struct Canonical {
    /// Absent only while the initial promotion is being published.
    model: Option<OpenFileModel>,
    lease: Option<Lease>,
}

/// The global store of shared open file descriptions.
#[derive(Debug, Default)]
pub struct SharedOpenFileStore {
    entries: BTreeMap<OpenFileId, Canonical>,
    next_sequence: u64,
}

impl SharedOpenFileStore {
    /// Makes `model` the canonical model of its open file description. The
    /// share transaction at a fork is the caller.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn insert(&mut self, model: OpenFileModel) -> Result<(), SharedOpenFileError> {
        let id = model.id();
        if self.entries.contains_key(&id) {
            return Err(SharedOpenFileError(format!("{id:?} is already shared")));
        }
        self.entries.insert(
            id,
            Canonical {
                model: Some(model),
                lease: None,
            },
        );
        Ok(())
    }

    /// Answers one control request from `thread` of `process`. The caller has
    /// already checked that `thread` holds the scheduler's serial grant.
    pub(crate) fn handle(
        &mut self,
        process: DetPid,
        thread: DetTid,
        request: OpenFileControl,
    ) -> Result<OpenFileControlReply, SharedOpenFileError> {
        let holder = Holder { process, thread };
        match request {
            OpenFileControl::Promote { id } => {
                if self.entries.contains_key(&id) {
                    return Err(SharedOpenFileError(format!("{id:?} is already shared")));
                }
                self.next_sequence += 1;
                let sequence = self.next_sequence;
                self.entries.insert(
                    id,
                    Canonical {
                        model: None,
                        lease: Some(Lease {
                            holder,
                            sequence,
                            outgoing: Vec::new(),
                            incoming: Vec::new(),
                        }),
                    },
                );
                Ok(OpenFileControlReply::Promoting { sequence })
            }
            OpenFileControl::Take { id } => {
                let sequence = self.next_sequence + 1;
                let canonical = self.entry(id)?;
                if let Some(lease) = &canonical.lease {
                    return Err(SharedOpenFileError(format!(
                        "{id:?} is already leased to thread {} (lease {})",
                        lease.holder.thread, lease.sequence
                    )));
                }
                let model = canonical.model.as_ref().ok_or_else(|| {
                    SharedOpenFileError(format!("{id:?} promotion is incomplete"))
                })?;
                let outgoing = encode_model(model)?;
                let length = outgoing.len();
                let chunk = chunk_at(&outgoing, 0)?;
                canonical.lease = Some(Lease {
                    holder,
                    sequence,
                    outgoing,
                    incoming: Vec::new(),
                });
                self.next_sequence = sequence;
                Ok(OpenFileControlReply::Taken {
                    sequence,
                    length,
                    chunk,
                })
            }
            OpenFileControl::TakeChunk {
                id,
                sequence,
                offset,
            } => {
                let canonical = self.entry(id)?;
                if canonical.model.is_none() {
                    return Err(SharedOpenFileError(format!(
                        "{id:?} promotion is incomplete"
                    )));
                }
                let lease = Self::current_lease(canonical, id, holder, sequence)?;
                Ok(OpenFileControlReply::Chunk(chunk_at(
                    &lease.outgoing,
                    offset,
                )?))
            }
            OpenFileControl::PublishChunk {
                id,
                sequence,
                offset,
                bytes,
                last,
            } => {
                if bytes.len() > CHUNK_LEN {
                    return Err(SharedOpenFileError(format!(
                        "{id:?}: oversized publish chunk"
                    )));
                }
                let canonical = self.entry(id)?;
                let lease = Self::current_lease(canonical, id, holder, sequence)?;
                if offset != lease.incoming.len() {
                    return Err(SharedOpenFileError(format!(
                        "{id:?}: publish chunk at {offset}, expected {}",
                        lease.incoming.len()
                    )));
                }
                lease.incoming.extend_from_slice(&bytes);
                if !last {
                    return Ok(OpenFileControlReply::Accepted);
                }
                let model = decode_model(&lease.incoming)?;
                if model.id() != id {
                    return Err(SharedOpenFileError(format!(
                        "{id:?}: published the model of {:?}",
                        model.id()
                    )));
                }
                canonical.model = Some(model);
                canonical.lease = None;
                Ok(OpenFileControlReply::Published)
            }
        }
    }

    /// Ends every lease held by `process`, discarding their unpublished
    /// changes. Called when the process is retired.
    pub(crate) fn end_leases_of(&mut self, process: DetPid) -> usize {
        let mut ended = 0;
        for canonical in self.entries.values_mut() {
            if canonical
                .lease
                .as_ref()
                .is_some_and(|lease| lease.holder.process == process)
            {
                canonical.lease = None;
                ended += 1;
            }
        }
        self.entries
            .retain(|_, canonical| canonical.model.is_some() || canonical.lease.is_some());
        ended
    }

    fn entry(&mut self, id: OpenFileId) -> Result<&mut Canonical, SharedOpenFileError> {
        self.entries
            .get_mut(&id)
            .ok_or_else(|| SharedOpenFileError(format!("{id:?} is not shared")))
    }

    fn current_lease(
        canonical: &mut Canonical,
        id: OpenFileId,
        holder: Holder,
        sequence: u64,
    ) -> Result<&mut Lease, SharedOpenFileError> {
        match &mut canonical.lease {
            Some(lease) if lease.sequence == sequence && lease.holder == holder => Ok(lease),
            _ => Err(SharedOpenFileError(format!(
                "{id:?}: lease {sequence} of thread {} is not the current lease",
                holder.thread
            ))),
        }
    }

    #[cfg(test)]
    fn model(&self, id: OpenFileId) -> &OpenFileModel {
        self.entries[&id].model.as_ref().expect("committed model")
    }

    #[cfg(test)]
    pub(crate) fn is_leased(&self, id: OpenFileId) -> bool {
        self.entries[&id].lease.is_some()
    }
}

/// Carries one [`OpenFileControl`] request to the global state and returns
/// its answer, synchronously. A backend implements this over its existing
/// coordinator connection.
pub trait OpenFileControlTransport: Send + Sync {
    /// Sends `request` and waits for the answer.
    fn call(&self, request: OpenFileControl) -> Result<OpenFileControlReply, SharedOpenFileError>;
}

/// A [`SharedOpenFileChannel`] that moves models in chunks over an
/// [`OpenFileControlTransport`].
///
/// It streams both ways: a take decodes the model while it fetches the
/// chunks, and a publish sends each chunk as soon as the encoder fills it. So
/// the process never holds more than one chunk of the encoding, beside the
/// model itself and the transport's frame for that chunk. That matters inside a guest process, where the runtime's
/// callback heap is small and fixed: a model that fits there as a local
/// description must also fit while it is taken or published.
pub struct ChunkedOpenFileChannel<T> {
    transport: T,
    /// The most encoded bytes a take or publish has held at once.
    peak_buffered: AtomicUsize,
}

impl<T: OpenFileControlTransport> ChunkedOpenFileChannel<T> {
    /// Wraps `transport`.
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            peak_buffered: AtomicUsize::new(0),
        }
    }

    fn note_buffered(&self, bytes: usize) {
        self.peak_buffered.fetch_max(bytes, Ordering::Relaxed);
    }
}

fn unexpected(reply: OpenFileControlReply) -> SharedOpenFileError {
    SharedOpenFileError(format!("unexpected open file control reply {reply:?}"))
}

/// Reads a leased model's encoding one chunk at a time, fetching the next
/// chunk only when the decoder has consumed the current one.
struct TakeReader<'a, T> {
    channel: &'a ChunkedOpenFileChannel<T>,
    id: OpenFileId,
    sequence: u64,
    length: usize,
    chunk: Vec<u8>,
    position: usize,
    /// Encoded bytes received so far, the current chunk included.
    received: usize,
    /// The transport's own error, kept so the caller reports it rather than
    /// the decoder's.
    failure: Option<SharedOpenFileError>,
}

impl<T: OpenFileControlTransport> TakeReader<'_, T> {
    fn fail(&mut self, error: SharedOpenFileError) -> std::io::Error {
        let io = std::io::Error::other(error.0.clone());
        self.failure = Some(error);
        io
    }

    fn fetch(&mut self) -> std::io::Result<()> {
        // The consumed chunk is released before the next one arrives, so the
        // two never coexist.
        self.chunk = Vec::new();
        self.position = 0;
        let reply = self.channel.transport.call(OpenFileControl::TakeChunk {
            id: self.id,
            sequence: self.sequence,
            offset: self.received,
        });
        match reply {
            Ok(OpenFileControlReply::Chunk(chunk))
                if !chunk.is_empty()
                    && chunk.len() <= CHUNK_LEN
                    && self.received + chunk.len() <= self.length =>
            {
                self.received += chunk.len();
                self.chunk = chunk;
                self.position = 0;
                self.channel.note_buffered(self.chunk.len());
                Ok(())
            }
            Ok(other) => Err(self.fail(unexpected(other))),
            Err(error) => Err(self.fail(error)),
        }
    }

    /// Whether the decoder consumed exactly the whole encoding.
    fn consumed_everything(&self) -> bool {
        self.received == self.length && self.position == self.chunk.len()
    }
}

impl<T: OpenFileControlTransport> std::io::Read for TakeReader<'_, T> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.position == self.chunk.len() {
            if self.received == self.length {
                return Ok(0);
            }
            self.fetch()?;
        }
        let count = out.len().min(self.chunk.len() - self.position);
        out[..count].copy_from_slice(&self.chunk[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

/// Collects a model's encoding and sends each full chunk as it fills.
struct PublishWriter<'a, T> {
    channel: &'a ChunkedOpenFileChannel<T>,
    id: OpenFileId,
    sequence: u64,
    /// Encoded bytes already sent.
    offset: usize,
    buffer: Vec<u8>,
    failure: Option<SharedOpenFileError>,
}

impl<T: OpenFileControlTransport> PublishWriter<'_, T> {
    fn send(&mut self, last: bool) -> Result<(), SharedOpenFileError> {
        let bytes = std::mem::take(&mut self.buffer);
        let length = bytes.len();
        let reply = self.channel.transport.call(OpenFileControl::PublishChunk {
            id: self.id,
            sequence: self.sequence,
            offset: self.offset,
            bytes,
            last,
        })?;
        match (reply, last) {
            (OpenFileControlReply::Published, true) | (OpenFileControlReply::Accepted, false) => {
                self.offset += length;
                Ok(())
            }
            (other, _) => Err(unexpected(other)),
        }
    }
}

impl<T: OpenFileControlTransport> std::io::Write for PublishWriter<'_, T> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.buffer.len() == CHUNK_LEN
            && let Err(error) = self.send(false)
        {
            let io = std::io::Error::other(error.0.clone());
            self.failure = Some(error);
            return Err(io);
        }
        let count = bytes.len().min(CHUNK_LEN - self.buffer.len());
        self.buffer.extend_from_slice(&bytes[..count]);
        self.channel.note_buffered(self.buffer.len());
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<T: OpenFileControlTransport> SharedOpenFileChannel for ChunkedOpenFileChannel<T> {
    fn promote(&self, model: &OpenFileModel) -> Result<(), SharedOpenFileError> {
        let sequence = match self
            .transport
            .call(OpenFileControl::Promote { id: model.id() })?
        {
            OpenFileControlReply::Promoting { sequence } => sequence,
            other => return Err(unexpected(other)),
        };
        self.publish_model(model, sequence)
    }

    fn take(&self, id: OpenFileId) -> Result<crate::fd::OpenFileLease, SharedOpenFileError> {
        let (sequence, length, chunk) = match self.transport.call(OpenFileControl::Take { id })? {
            OpenFileControlReply::Taken {
                sequence,
                length,
                chunk,
            } if chunk.len() <= CHUNK_LEN.min(length) => (sequence, length, chunk),
            other => return Err(unexpected(other)),
        };
        self.note_buffered(chunk.len());
        let mut reader = TakeReader {
            channel: self,
            id,
            sequence,
            length,
            received: chunk.len(),
            chunk,
            position: 0,
            failure: None,
        };
        let decoded = bincode::serde::decode_from_std_read::<OpenFileModel, _, _>(
            &mut reader,
            bincode::config::legacy(),
        );
        if let Some(error) = reader.failure.take() {
            return Err(error);
        }
        let model = decoded.map_err(|error| {
            SharedOpenFileError(format!("decoding an open file model: {error}"))
        })?;
        if !reader.consumed_everything() {
            return Err(SharedOpenFileError(format!(
                "{id:?}: the model did not use exactly its {length} encoded bytes"
            )));
        }
        Ok(crate::fd::OpenFileLease {
            id,
            sequence,
            model,
        })
    }

    fn publish(&self, lease: crate::fd::OpenFileLease) -> Result<(), SharedOpenFileError> {
        self.publish_model(&lease.model, lease.sequence)
    }
}

impl<T: OpenFileControlTransport> ChunkedOpenFileChannel<T> {
    fn publish_model(
        &self,
        model: &OpenFileModel,
        sequence: u64,
    ) -> Result<(), SharedOpenFileError> {
        let mut writer = PublishWriter {
            channel: self,
            id: model.id(),
            sequence,
            offset: 0,
            buffer: Vec::new(),
            failure: None,
        };
        let encoded =
            bincode::serde::encode_into_std_write(model, &mut writer, bincode::config::legacy());
        if let Some(error) = writer.failure.take() {
            return Err(error);
        }
        encoded.map_err(|error| {
            SharedOpenFileError(format!("encoding an open file model: {error}"))
        })?;
        // The last chunk, possibly empty, commits the model.
        writer.send(true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use nix::fcntl::OFlag;

    use super::*;
    use crate::fd::DetFd;
    use crate::fd::FdType;

    const PROCESS: DetPid = DetPid::from_raw(40);
    const THREAD: DetTid = DetTid::from_raw(40);

    fn model(id: OpenFileId, path: &str) -> OpenFileModel {
        DetFd::new(3, OFlag::O_RDWR, FdType::Regular, id)
            .with_path(path)
            .model_for_test()
    }

    /// Calls the store directly, as the global state does after its grant
    /// check.
    struct Direct<'a>(&'a Mutex<SharedOpenFileStore>);

    impl OpenFileControlTransport for Direct<'_> {
        fn call(
            &self,
            request: OpenFileControl,
        ) -> Result<OpenFileControlReply, SharedOpenFileError> {
            self.0.lock().unwrap().handle(PROCESS, THREAD, request)
        }
    }

    #[test]
    fn take_and_publish_replace_the_canonical_model() {
        let id = OpenFileId::new(THREAD, 0);
        let store = Mutex::new(SharedOpenFileStore::default());
        store.lock().unwrap().insert(model(id, "/old")).unwrap();
        let channel = ChunkedOpenFileChannel::new(Direct(&store));

        let mut lease = channel.take(id).unwrap();
        assert_eq!(lease.model.path_for_test(), Some("/old".into()));
        assert!(store.lock().unwrap().is_leased(id));
        lease.model = model(id, "/new");
        channel.publish(lease).unwrap();

        let store = store.lock().unwrap();
        assert!(!store.is_leased(id));
        assert_eq!(store.model(id).path_for_test(), Some("/new".into()));
    }

    #[test]
    fn promotion_commits_once_before_another_holder_can_take() {
        let id = OpenFileId::new(THREAD, 800);
        let mut store = SharedOpenFileStore::default();
        let OpenFileControlReply::Promoting { sequence } = store
            .handle(PROCESS, THREAD, OpenFileControl::Promote { id })
            .unwrap()
        else {
            panic!("promotion reply")
        };
        assert!(
            store
                .handle(PROCESS, THREAD, OpenFileControl::Promote { id })
                .is_err()
        );
        assert!(
            store
                .handle(PROCESS, THREAD, OpenFileControl::Take { id })
                .is_err()
        );
        assert!(
            store
                .handle(
                    PROCESS,
                    THREAD,
                    OpenFileControl::TakeChunk {
                        id,
                        sequence,
                        offset: 0
                    }
                )
                .is_err()
        );
        let child = DetTid::from_raw(41);
        let publish = OpenFileControl::PublishChunk {
            id,
            sequence,
            offset: 0,
            bytes: encode_model(&model(id, "/promoted")).unwrap(),
            last: true,
        };
        assert!(store.handle(child, child, publish.clone()).is_err());
        store.handle(PROCESS, THREAD, publish.clone()).unwrap();
        assert!(store.handle(PROCESS, THREAD, publish).is_err());
        assert_eq!(store.model(id).path_for_test(), Some("/promoted".into()));
        assert!(matches!(
            store
                .handle(child, child, OpenFileControl::Take { id })
                .unwrap(),
            OpenFileControlReply::Taken { .. }
        ));
        assert_eq!(store.end_leases_of(child), 1);
        assert_eq!(store.model(id).path_for_test(), Some("/promoted".into()));

        let incomplete = OpenFileId::new(THREAD, 801);
        store
            .handle(PROCESS, THREAD, OpenFileControl::Promote { id: incomplete })
            .unwrap();
        assert_eq!(store.end_leases_of(PROCESS), 1);
        assert!(!store.entries.contains_key(&incomplete));
    }

    #[test]
    fn promotion_streams_the_existing_model_without_a_second_codec() {
        let id = OpenFileId::new(THREAD, 802);
        let store = Mutex::new(SharedOpenFileStore::default());
        let channel = ChunkedOpenFileChannel::new(Direct(&store));
        let path = format!("/{}", "p".repeat(CHUNK_LEN + 17));
        channel.promote(&model(id, &path)).unwrap();
        assert!(channel.promote(&model(id, "/replacement")).is_err());
        let lease = channel.take(id).unwrap();
        assert_eq!(lease.model.path_for_test(), Some(path.into()));
        channel.publish(lease).unwrap();
        assert!(channel.peak_buffered.load(Ordering::Relaxed) <= CHUNK_LEN);
    }

    /// Calls the store directly and records the largest model chunk any one
    /// message carried.
    struct Recording<'a> {
        store: &'a Mutex<SharedOpenFileStore>,
        largest: Mutex<usize>,
        messages: Mutex<usize>,
    }

    impl OpenFileControlTransport for Recording<'_> {
        fn call(
            &self,
            request: OpenFileControl,
        ) -> Result<OpenFileControlReply, SharedOpenFileError> {
            if let OpenFileControl::PublishChunk { bytes, .. } = &request {
                let mut largest = self.largest.lock().unwrap();
                *largest = (*largest).max(bytes.len());
            }
            let reply = self.store.lock().unwrap().handle(PROCESS, THREAD, request);
            if let Ok(
                OpenFileControlReply::Taken { chunk, .. } | OpenFileControlReply::Chunk(chunk),
            ) = &reply
            {
                let mut largest = self.largest.lock().unwrap();
                *largest = (*largest).max(chunk.len());
            }
            *self.messages.lock().unwrap() += 1;
            reply
        }
    }

    /// A model larger than the 16 MiB transport frame moves in chunks of at
    /// most `CHUNK_LEN`, both ways, and the channel never holds more than one
    /// chunk of its encoding: it decodes while it fetches and sends while it
    /// encodes.
    #[test]
    fn a_model_larger_than_a_frame_moves_in_bounded_chunks() {
        let id = OpenFileId::new(THREAD, 1);
        let long = format!("/{}", "x".repeat((16 << 20) + 17));
        let store = Mutex::new(SharedOpenFileStore::default());
        store.lock().unwrap().insert(model(id, &long)).unwrap();
        let channel = ChunkedOpenFileChannel::new(Recording {
            store: &store,
            largest: Mutex::new(0),
            messages: Mutex::new(0),
        });

        let lease = channel.take(id).unwrap();
        assert_eq!(lease.model.path_for_test(), Some(long.clone().into()));
        channel.publish(lease).unwrap();
        assert_eq!(
            store.lock().unwrap().model(id).path_for_test(),
            Some(long.into())
        );
        let transport = &channel.transport;
        assert!(*transport.largest.lock().unwrap() <= CHUNK_LEN);
        let peak = channel.peak_buffered.load(Ordering::Relaxed);
        assert!(
            peak > 0 && peak <= CHUNK_LEN,
            "held {peak} encoded bytes at once"
        );
        // At least seventeen chunks each way for a model over 16 MiB.
        assert!(*transport.messages.lock().unwrap() >= 34);
    }

    /// Until the last chunk of a publish arrives, the old model stays
    /// canonical; a chunk at the wrong offset, or a model of another
    /// description, is refused and commits nothing.
    #[test]
    fn a_partial_or_wrong_publish_commits_nothing() {
        let id = OpenFileId::new(THREAD, 4);
        let mut store = SharedOpenFileStore::default();
        store.insert(model(id, "/old")).unwrap();
        let take = |store: &mut SharedOpenFileStore| match store
            .handle(PROCESS, THREAD, OpenFileControl::Take { id })
            .unwrap()
        {
            OpenFileControlReply::Taken { sequence, .. } => sequence,
            other => panic!("expected a lease, got {other:?}"),
        };
        let publish = |store: &mut SharedOpenFileStore, sequence, offset, bytes: &[u8], last| {
            store.handle(
                PROCESS,
                THREAD,
                OpenFileControl::PublishChunk {
                    id,
                    sequence,
                    offset,
                    bytes: bytes.to_vec(),
                    last,
                },
            )
        };
        let new = encode_model(&model(id, "/new")).unwrap();
        let (head, tail) = new.split_at(new.len() / 2);

        let sequence = take(&mut store);
        assert_eq!(
            publish(&mut store, sequence, 0, head, false),
            Ok(OpenFileControlReply::Accepted)
        );
        assert_eq!(store.model(id).path_for_test(), Some("/old".into()));
        assert!(store.is_leased(id));
        // A gap or an overlap is refused.
        assert!(publish(&mut store, sequence, head.len() + 1, tail, true).is_err());
        assert!(publish(&mut store, sequence, head.len() - 1, tail, true).is_err());
        assert_eq!(store.model(id).path_for_test(), Some("/old".into()));
        // The holder dies before the last chunk: its partial publish is gone.
        assert_eq!(store.end_leases_of(PROCESS), 1);
        assert_eq!(store.model(id).path_for_test(), Some("/old".into()));
        assert!(publish(&mut store, sequence, head.len(), tail, true).is_err());

        // A complete publish of another description's model is refused.
        let other = encode_model(&model(OpenFileId::new(THREAD, 5), "/other")).unwrap();
        let sequence = take(&mut store);
        assert!(publish(&mut store, sequence, 0, &other, true).is_err());
        assert_eq!(store.model(id).path_for_test(), Some("/old".into()));
    }

    #[test]
    fn a_second_take_and_a_stale_publish_are_refused() {
        let id = OpenFileId::new(THREAD, 2);
        let mut store = SharedOpenFileStore::default();
        store.insert(model(id, "/a")).unwrap();

        let OpenFileControlReply::Taken { sequence, .. } = store
            .handle(PROCESS, THREAD, OpenFileControl::Take { id })
            .unwrap()
        else {
            panic!("expected a lease");
        };
        assert!(
            store
                .handle(PROCESS, THREAD, OpenFileControl::Take { id })
                .is_err()
        );
        let bytes = encode_model(&model(id, "/b")).unwrap();
        // A publish under another sequence, or from another thread, is refused
        // and leaves the lease in place.
        for (thread, sequence) in [(THREAD, sequence + 1), (DetTid::from_raw(41), sequence)] {
            assert!(
                store
                    .handle(
                        PROCESS,
                        thread,
                        OpenFileControl::PublishChunk {
                            id,
                            sequence,
                            offset: 0,
                            bytes: bytes.clone(),
                            last: true,
                        },
                    )
                    .is_err()
            );
            assert!(store.is_leased(id));
        }
        assert_eq!(store.model(id).path_for_test(), Some("/a".into()));
    }

    #[test]
    fn the_death_of_the_holder_ends_its_lease_and_keeps_the_model() {
        let id = OpenFileId::new(THREAD, 3);
        let mut store = SharedOpenFileStore::default();
        store.insert(model(id, "/kept")).unwrap();
        let OpenFileControlReply::Taken { sequence, .. } = store
            .handle(PROCESS, THREAD, OpenFileControl::Take { id })
            .unwrap()
        else {
            panic!("expected a lease");
        };

        assert_eq!(store.end_leases_of(DetPid::from_raw(99)), 0);
        assert_eq!(store.end_leases_of(PROCESS), 1);
        assert!(!store.is_leased(id));
        assert_eq!(store.model(id).path_for_test(), Some("/kept".into()));
        // The dead holder's late publish names no current lease.
        assert!(
            store
                .handle(
                    PROCESS,
                    THREAD,
                    OpenFileControl::PublishChunk {
                        id,
                        sequence,
                        offset: 0,
                        bytes: encode_model(&model(id, "/late")).unwrap(),
                        last: true,
                    },
                )
                .is_err()
        );
        assert_eq!(store.model(id).path_for_test(), Some("/kept".into()));
    }
}
