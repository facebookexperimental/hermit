/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in LICENSE.
 */

//! Ordinary HLIINIT1 admission for an actual already-owned first-image child.
//!
//! This is not a launcher, image qualifier, replacement coordinator, or Ready
//! producer. The caller keeps the original stopped-generation cleanup owner,
//! prepared image, socket/DETLOG leases and physical bootstrap transaction.
//! An actual pidfd authenticates this admission's watched birth, never those
//! independent obligations. Nothing here releases the child or the Guest.

use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use detcore::detlog::NativePhysicalWatchIdentity;
use detcore_model::liteinst_initial_image as packet;
use hermit::in_guest_exits::InGuestExitAdmission;
use hermit::in_guest_exits::NativeWatchedProcess;
use reverie_rpc_transport::Admitted;
use reverie_rpc_transport::ConnectionAdmission;
use reverie_rpc_transport::RpcIssueMonitor;
use reverie_rpc_transport::RpcServer;
use reverie_rpc_transport::initial_image::InitialImageAdmission;
use reverie_rpc_transport::initial_image::config_digest;

use super::initial_image_admission::InitialRedemption;
use super::initial_image_admission::NativeReporter;
use super::initial_image_admission::consume_initial_redemption;
use super::initial_image_admission::entropy;
use super::initial_image_admission::inherited_socket_identity;
use super::initial_image_admission::require_watched_peer;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn error(error: impl std::fmt::Display) -> io::Error {
    invalid(error.to_string())
}

/// The one real watch and one-use Config redemption for the mapped first image.
/// No native exec artifact catalog, root classifier, successor registry, native
/// observer, or native Setup is created or impersonated by this owner.
pub(super) struct MappedFirstAdmission {
    global: Arc<detcore::GlobalState>,
    config: detcore::Config,
    exits: Arc<InGuestExitAdmission>,
    watch: Arc<NativeWatchedProcess>,
    nonce: [u8; 32],
    digest: [u8; 32],
    schema: [u8; 16],
    coordinator: Vec<u8>,
    detlog_identity: String,
    observer_identity: Option<String>,
    initial_used: AtomicBool,
    failed: AtomicBool,
    server_configured: AtomicBool,
}

impl MappedFirstAdmission {
    /// Bind the actual held child before it may enter the runtime or connect.
    ///
    /// `launch_pidfd` must be duplicated from the same retained stopped-child
    /// generation by the real physical producer. This function can authenticate
    /// a kernel pidfd and its watch, but cannot prove mapping or ptrace ownership;
    /// its safe return deliberately conveys only RPC admission, not permission
    /// to execute an image. No PID argument or PID-to-pidfd reconstruction exists.
    ///
    /// `coordinator` names the actual prebound listener in the child's mount
    /// namespace. The producer retains the actual DETLOG sender, arranges its
    /// declared child destination and drops the parent sender only after that
    /// real inheritance. The borrowed descriptor observes that same socket; it
    /// is not a transferred FD, detached socket identity, or EOF receipt.
    ///
    /// This runs on the already-existing coordinator Tokio runtime. On error it
    /// poisons and drains any new watch; the original child cleanup authority
    /// remains with the caller even if the drain cannot be confirmed.
    #[cfg(test)]
    pub(super) async fn prepare(
        global: Arc<detcore::GlobalState>,
        launch_pidfd: OwnedFd,
        coordinator: &[u8],
        detlog_sender: BorrowedFd<'_>,
        detlog_child_fd: i32,
    ) -> io::Result<Arc<Self>> {
        Self::prepare_inner(
            global,
            launch_pidfd,
            coordinator,
            detlog_sender,
            detlog_child_fd,
            None,
        )
        .await
    }

    /// Bind an independent inherited observer to the same actual first-image watch.
    pub(super) async fn prepare_with_observer(
        global: Arc<detcore::GlobalState>,
        launch_pidfd: OwnedFd,
        coordinator: &[u8],
        detlog_sender: BorrowedFd<'_>,
        observer_sender: BorrowedFd<'_>,
    ) -> io::Result<Arc<Self>> {
        Self::prepare_inner(
            global,
            launch_pidfd,
            coordinator,
            detlog_sender,
            detlog_sender.as_raw_fd(),
            Some(observer_sender),
        )
        .await
    }

    async fn prepare_inner(
        global: Arc<detcore::GlobalState>,
        launch_pidfd: OwnedFd,
        coordinator: &[u8],
        detlog_sender: BorrowedFd<'_>,
        detlog_child_fd: i32,
        observer_sender: Option<BorrowedFd<'_>>,
    ) -> io::Result<Arc<Self>> {
        // This accessor retains host-only sinks omitted from Config's wire.
        // Never accept a caller's merely digest-equivalent replacement Config.
        let config = global.native_exec_host_config().clone();
        if global.native_exec_registry().is_some() {
            return Err(invalid(
                "mapped first image cannot share a native successor registry",
            ));
        }
        if let Some(reason) = packet::config_violation(&config) {
            return Err(invalid(reason));
        }
        if detlog_child_fd < 0 {
            return Err(invalid("mapped DETLOG child descriptor is negative"));
        }
        let nonce = entropy()?;
        let digest = config_digest(&reverie_rpc_transport::codec::encode(&config).map_err(error)?);
        let schema = detcore::config_wire_fingerprint()
            .as_bytes()
            .try_into()
            .map_err(|_| invalid("mapped InitialImage Config schema width"))?;
        let detlog_identity = inherited_socket_identity(detlog_sender, detlog_child_fd)?;
        let observer_identity = observer_sender
            .map(|observer| observer_binding(observer, detlog_child_fd, &detlog_identity))
            .transpose()?;
        // Validate the complete actual packet before starting a watch task.
        encode_observed_packet(
            &config,
            &nonce,
            &digest,
            &schema,
            coordinator,
            &detlog_identity,
            observer_identity.as_deref(),
        )?;
        let exits = Arc::new(InGuestExitAdmission::default());
        exits.attach_exit_reporter(Arc::new(NativeReporter {
            global: Arc::downgrade(&global),
        }));
        let bound = (|| {
            let admitted = exits.admit(launch_pidfd.try_clone()?)?;
            let watch = exits.retain_native_process(launch_pidfd)?;
            if admitted.process_id != watch.process_id() {
                return Err(invalid("mapped watch returned a different actual leader"));
            }
            Ok(watch)
        })();
        let watch = match bound {
            Ok(watch) => watch,
            Err(error) => {
                exits.fail_launch(&error);
                return match exits.drain(Duration::from_secs(10)).await {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(invalid(format!(
                        "mapped admission refused: {error}; watch drain unconfirmed: {cleanup:?}"
                    ))),
                };
            }
        };
        Ok(Arc::new(Self {
            global,
            config,
            exits,
            watch: Arc::new(watch),
            nonce,
            digest,
            schema,
            coordinator: coordinator.to_vec(),
            detlog_identity,
            observer_identity,
            initial_used: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            server_configured: AtomicBool::new(false),
        }))
    }

    /// Complete ordinary HLNI bytes for the actual sealed input producer.
    /// Encoding or sealing them grants no physical launch or Ready permission.
    pub(super) fn initial_packet(&self) -> io::Result<Vec<u8>> {
        if self.failed.load(Ordering::Acquire) || !self.watch.watch_is_healthy() {
            return Err(invalid(
                "mapped InitialImage watch is failed or no longer live",
            ));
        }
        encode_observed_packet(
            &self.config,
            &self.nonce,
            &self.digest,
            &self.schema,
            &self.coordinator,
            &self.detlog_identity,
            self.observer_identity.as_deref(),
        )
    }

    /// Configure only ordinary InitialImage admission on the same real global.
    /// Native successor dispatch and config-first fallback remain disabled.
    /// The caller serves this same server concurrently with the ptracer pump.
    /// It must race the returned issue monitor against that completion and call
    /// fail on every retained issue, including malformed request/EOF failures
    /// before the redemption hook can be called. Never discard this monitor.
    pub(super) fn configure_server(
        self: &Arc<Self>,
        server: &mut RpcServer<detcore::GlobalState>,
    ) -> io::Result<RpcIssueMonitor> {
        let result = (|| {
            if self.failed.load(Ordering::Acquire)
                || !self.watch.watch_is_healthy()
                || !Arc::ptr_eq(&self.global, &server.global())
                || self.server_configured.swap(true, Ordering::AcqRel)
            {
                return Err(invalid(
                    "mapped InitialImage server/global/watch binding refused",
                ));
            }
            let issues = server.retain_connection_issues();
            server.set_connection_admission(self.clone());
            server
                .set_initial_image_admission(self.clone())
                .map_err(error)?;
            Ok(issues)
        })();
        if let Err(error) = &result {
            self.fail(error);
        }
        result
    }

    /// Borrow the actual watch for the future physical completion transaction.
    /// The child cleanup owner remains external and must stay alive through
    /// terminal wait; this watch is not that ptracer or a replacement wait owner.
    pub(super) fn watched_process(&self) -> &NativeWatchedProcess {
        &self.watch
    }

    pub(super) fn observer_watch(&self) -> Arc<NativeWatchedProcess> {
        self.watch.clone()
    }

    pub(super) async fn observer_completion(
        &self,
    ) -> io::Result<hermit::in_guest_exits::NativePhysicalCompletion> {
        self.exits
            .drain_native_completion(
                Duration::from_secs(10),
                self.watch.retained_pidfd().try_clone_to_owned()?,
                self.watch.process_id(),
                self.watch.birth_identity(),
            )
            .await
    }

    /// Retain this same admission for real exit draining and watchdog reporting.
    /// Replacing its reporter invalidates the retained watched-birth proof.
    pub(super) fn exit_admission(&self) -> &Arc<InGuestExitAdmission> {
        &self.exits
    }

    /// Permanently poison admission and fail the same watch/global when the
    /// actual coordinator/issue race fails; this is not physical retirement.
    pub(super) fn fail(&self, error: &dyn std::fmt::Display) {
        self.failed.store(true, Ordering::Release);
        self.exits.fail_launch(error);
    }
}

fn encode_packet(
    config: &detcore::Config,
    nonce: &[u8; 32],
    digest: &[u8; 32],
    schema: &[u8; 16],
    coordinator: &[u8],
    detlog_identity: &str,
) -> io::Result<Vec<u8>> {
    let policy = config
        .in_guest_detlog_forward_policy
        .as_deref()
        .ok_or_else(|| invalid("mapped InitialImage lacks genuine DETLOG forwarding policy"))?;
    packet::encode(packet::Packet {
        nonce,
        config_digest: digest,
        schema_fingerprint: schema,
        coordinator,
        detlog_identity,
        forward_policy: policy,
    })
    .map_err(error)
}

fn observer_binding(
    observer: BorrowedFd<'_>,
    detlog_child_fd: i32,
    detlog_identity: &str,
) -> io::Result<String> {
    let encoded = inherited_socket_identity(observer, observer.as_raw_fd())?;
    let (_, object) = detcore::detlog::parse_tool_output_env_value(&encoded)
        .ok_or_else(|| invalid("mapped observer identity unreadable"))?;
    let (_, detlog_object) = detcore::detlog::parse_tool_output_env_value(detlog_identity)
        .ok_or_else(|| invalid("mapped DETLOG identity unreadable"))?;
    if observer.as_raw_fd() == detlog_child_fd || object == detlog_object {
        return Err(invalid("mapped observer must be distinct from DETLOG"));
    }
    let mut kind = 0i32;
    let mut size = std::mem::size_of_val(&kind) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            observer.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&raw mut kind).cast(),
            &mut size,
        )
    } != 0
        || size as usize != std::mem::size_of_val(&kind)
        || kind != libc::SOCK_SEQPACKET
    {
        return Err(invalid("mapped observer requires sequenced packets"));
    }
    Ok(encoded)
}

fn encode_observed_packet(
    config: &detcore::Config,
    nonce: &[u8; 32],
    digest: &[u8; 32],
    schema: &[u8; 16],
    coordinator: &[u8],
    detlog: &str,
    observer: Option<&str>,
) -> io::Result<Vec<u8>> {
    let old = encode_packet(config, nonce, digest, schema, coordinator, detlog)?;
    match observer {
        None => Ok(old),
        Some(identity) => {
            packet::encode_with_observer(packet::decode(&old).map_err(error)?, identity)
                .map_err(error)
        }
    }
}

impl ConnectionAdmission for MappedFirstAdmission {
    fn admit(&self, peer: OwnedFd) -> io::Result<Admitted> {
        let result = (|| {
            require_watched_peer(&self.watch, &peer, &self.failed)?;
            let admitted = self.exits.admit(peer)?;
            if admitted.process_id != self.watch.process_id() {
                return Err(invalid("mapped RPC watch returned another leader"));
            }
            Ok(admitted)
        })();
        if let Err(error) = &result {
            self.fail(error);
        }
        result
    }

    fn admission_failed(&self, error: &io::Error) {
        self.fail(error);
    }
}

impl InitialImageAdmission for MappedFirstAdmission {
    fn redeem(
        &self,
        peer: OwnedFd,
        admitted: Admitted,
        nonce: &[u8],
        digest: [u8; 32],
    ) -> io::Result<()> {
        let result = (|| {
            require_watched_peer(&self.watch, &peer, &self.failed)?;
            if let Some(reason) = packet::config_violation(&self.config) {
                return Err(invalid(reason));
            }
            consume_initial_redemption(
                InitialRedemption {
                    process: self.watch.process_id(),
                    nonce: &self.nonce,
                    digest: self.digest,
                },
                InitialRedemption {
                    process: admitted.process_id,
                    nonce,
                    digest,
                },
                &self.initial_used,
            )
        })();
        if let Err(error) = &result {
            // Permanent poison includes mismatches that did not consume the
            // one-use bit. Wrong nonce or wrong Config never retry. Earlier
            // framing/EOF errors reach the coordinator's retained issue monitor.
            self.fail(error);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_packet_uses_actual_codec_without_changing_bound_fields() {
        let config = detcore::Config {
            in_guest_detlog_forward_policy: Some("INFO".to_owned()),
            ..Default::default()
        };
        let bytes = encode_packet(
            &config,
            &[1; 32],
            &[2; 32],
            b"0123456789abcdef",
            b"/run/rpc-\xff.sock",
            "207:0000000000000007:0000000000000009",
        )
        .unwrap();
        assert_eq!(&bytes[..8], b"HLIINIT1");
        let decoded = packet::decode(&bytes).unwrap();
        assert_eq!(decoded.nonce, &[1; 32]);
        assert_eq!(decoded.config_digest, &[2; 32]);
        assert_eq!(decoded.schema_fingerprint, b"0123456789abcdef");
        assert_eq!(decoded.coordinator, b"/run/rpc-\xff.sock");
        assert_eq!(
            decoded.detlog_identity,
            "207:0000000000000007:0000000000000009"
        );
        assert_eq!(decoded.forward_policy, "INFO");
        // No semantic owner or physical positive is asserted by this codec case.
    }

    #[cfg(feature = "native-exec")]
    #[test]
    fn real_native_initial_and_successor_packets_are_not_ordinary_initial_packets() {
        use detcore_model::liteinst_native_exec_input as native;
        for image in [
            native::Image::Initial { nonce: &[1; 32] },
            native::Image::Successor {
                ticket: &[1; 32],
                composite_digest: &[3; 32],
            },
        ] {
            let bytes = native::encode(native::Packet {
                config_digest: &[2; 32],
                config_schema: b"0123456789abcdef",
                native_source_digest: &[4; 32],
                target_digest: &[5; 32],
                bootstrap_digest: &[6; 32],
                image,
                coordinator: b"/run/rpc.sock",
                detlog_identity: "204:0000000000000007:0000000000000009",
                forward_policy: "INFO",
                observer_identity: "203:0000000000000007:000000000000000a",
            })
            .unwrap();
            assert!(native::decode(&bytes).is_ok());
            assert_eq!(packet::decode(&bytes), Err(packet::PacketError::Magic));
        }
    }
}

// Real admission-only child/socket/pidfd controls; no runtime image executes.
#[cfg(test)]
mod physical_tests;

// Requires the production exec-stop inherited-descriptor bridge and private runtime feature.
mod packet_lease;

mod mapped_run;
mod runtime_binding;
mod runtime_support;
pub(crate) use mapped_run::run as run_mapped;
pub(crate) use runtime_binding::RetainedImage;
pub(crate) use runtime_binding::declared as packaged_image_declared;

#[cfg(test)]
mod observer_binding_tests {
    use std::os::fd::AsFd;
    use std::os::fd::FromRawFd;

    use super::*;
    fn pair() -> [OwnedFd; 2] {
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        fds.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
    }
    #[test]
    fn actual_observer_requires_distinct_descriptor_and_socket_object() {
        let [detlog, _peer] = pair();
        let [observer, _observer_peer] = pair();
        let identity = inherited_socket_identity(detlog.as_fd(), detlog.as_raw_fd()).unwrap();
        assert!(observer_binding(observer.as_fd(), detlog.as_raw_fd(), &identity).is_ok());
        assert!(
            observer_binding(detlog.as_fd(), detlog.as_raw_fd(), &identity)
                .unwrap_err()
                .to_string()
                .contains("distinct")
        );
        let duplicate = detlog.try_clone().unwrap();
        assert!(
            observer_binding(duplicate.as_fd(), detlog.as_raw_fd(), &identity)
                .unwrap_err()
                .to_string()
                .contains("distinct")
        );
        assert!(observer_binding(observer.as_fd(), detlog.as_raw_fd(), "malformed").is_err());
    }
    #[test]
    fn actual_stream_socket_is_not_an_observer_datagram_channel() {
        let [detlog, _peer] = pair();
        let identity = inherited_socket_identity(detlog.as_fd(), detlog.as_raw_fd()).unwrap();
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(observer_binding(stream.as_fd(), detlog.as_raw_fd(), &identity).is_err());
    }
}

#[cfg(all(test, feature = "mm-cancellation-test-support"))]
pub(crate) use mapped_run::mm_cancel::cancel_at as cancel_mm_in_container;
