//! Real private-runner entry integration, using the one shared installed owner.
//!
//! All unsafe authority originates in the covered native startup and actual
//! Initial/Successor admission. Packet/catalog values remain transport data.

use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::sync::Arc;

use detcore_model::liteinst_native_exec_catalog as catalog_wire;
use detcore_model::liteinst_native_exec_input as input;
use reverie_inguest::guest::exec_private::NativeControl;
use reverie_inguest::guest::exec_private::NativeInitializationIoGuard;
use reverie_inguest::guest::exec_private::NativeLoaderResume;
use reverie_inguest::guest::native_exec_arguments::ArgumentRange;
use reverie_inguest::guest::native_exec_arguments::ReadCapture;
use reverie_inguest::guest::native_exec_fds::NativeSocket;
use reverie_inguest::guest::native_exec_fds::reserve_socket;
use reverie_inguest::guest::native_exec_fds::socket_fd;
use reverie_inguest::guest::native_exec_mappings::ExecutableBacking;
use reverie_inguest::guest::native_exec_mappings::ExecutableExtent;
use reverie_inguest::guest::native_exec_mappings::ExecutableMappings;
use reverie_inguest::guest::native_exec_mappings::ExecutableObject;
use reverie_inguest::guest::rpc::GuestRpcStream;
use reverie_rpc_transport::BlockingRpcClient;
use reverie_rpc_transport::InitialImageClient;
use reverie_rpc_transport::SuccessorClient;

use super::catalog;
use super::consumer;
use super::driver;
use super::provider;

fn refusal(message: &'static str) -> io::Error {
    io::Error::other(message)
}

#[expect(
    clippy::large_enum_variant,
    reason = "Initial/successor preparation owns distinct fixed controllers on the native private initializer stack; preserve their by-value lifecycle."
)]
pub(crate) enum Prepared {
    Initial {
        image: reverie_liteinst::PreparedInitialImage<detcore::Detcore>,
        metadata: Metadata,
    },
    Successor {
        image: reverie_liteinst::PreparedSuccessorImage<detcore::Detcore>,
        metadata: Metadata,
    },
}

pub(crate) struct Metadata {
    source: [u8; 32],
    schema: [u8; 16],
    config: [u8; 32],
    bootstrap: [u8; 32],
    generation: u64,
    forward_policy: String,
    detlog_object: (u64, u64),
    observer_object: (u64, u64),
    resume: NativeLoaderResume,
    protected: Box<[ArgumentRange]>,
    catalog: catalog_wire::Catalog,
    backing_objects: Vec<ExecutableObject>,
}

#[expect(
    clippy::large_enum_variant,
    reason = "Completion retains the successor controller and resume by value until finish; preserve the one-use completion path."
)]
pub(crate) enum Installed {
    Initial(reverie_liteinst::InstalledInitialImage),
    Successor {
        image: reverie_liteinst::InstalledSuccessorImage<detcore::Detcore>,
        resume: NativeLoaderResume,
    },
}

pub(crate) fn qualify_config(config: &detcore::Config) -> io::Result<()> {
    detcore::native_exec::validate_native_exec_guest_configuration(config)
        .map_err(|_| refusal("native guest Config is outside the admitted domain"))?;
    if !config.in_guest_site_patching_off {
        return Err(refusal(
            "native first domain requires genuine patching-off trap Config",
        ));
    }
    Ok(())
}

fn check_config(config: &detcore::Config, packet: input::Packet<'_>) -> io::Result<()> {
    qualify_config(config)?;
    if detcore::native_exec::native_exec_config_digest(config).ok() != Some(*packet.config_digest)
        || config.in_guest_detlog_forward_policy.as_deref() != Some(packet.forward_policy)
    {
        return Err(refusal(
            "native admitted Config or forwarding policy differs",
        ));
    }
    Ok(())
}

/// # Safety
/// The actual native runner supplies this real private FS/stack/control, exact
/// inherited gate/filter and protected packet. Host admission must establish
/// the same watched birth, sealed artifact/root graph and actual kernel image
/// before redeeming. No loader/Guest work may occur during this function.
pub(crate) unsafe fn prepare(
    native: *mut NativeControl,
    bytes: &[u8],
) -> io::Result<(Prepared, OsString, OsString, OsString)> {
    let packet = input::decode(bytes).map_err(|_| refusal("native input packet is malformed"))?;
    let source = detcore_model::liteinst_native_exec_sources::digest(
        reverie_inguest::native_exec_sources::digest(),
    );
    if packet.native_source_digest != &source
        || packet.config_schema.as_slice() != detcore::config_wire_fingerprint().as_bytes()
    {
        return Err(refusal("native compiled source or Config schema differs"));
    }
    let tid = unsafe { reverie_inguest::guest::exec_private::native_tid(native) }?;
    let socket = OsString::from_vec(packet.coordinator.to_vec());
    let path = std::path::Path::new(&socket);
    // These successful exchanges are the real one-use host admission on the
    // same owned stream. No additional client is created by installation.
    #[expect(
        clippy::large_enum_variant,
        reason = "Retain the actual admitted client and neutral-cut consumer by value until preparation; this is bounded private initializer state."
    )]
    enum Admitted {
        // Fatal mechanism ownership drops before Config/client teardown.
        Initial(
            NativeInitializationIoGuard,
            InitialImageClient<detcore::GlobalState, GuestRpcStream>,
        ),
        Successor(
            NativeInitializationIoGuard,
            SuccessorClient<detcore::GlobalState, GuestRpcStream>,
            consumer::Consumer,
            reverie_inguest::guest::exec_private::SuccessorBinding,
        ),
    }
    // This owns only physical socket I/O through the qualified inherited gate.
    // It publishes neither Tool control nor admission: the shared exchange below
    // must still obtain the real watched-image ACK and independent digests.
    let stream = unsafe { GuestRpcStream::connect_native(path, native) }?;
    let admitted = match packet.image {
        input::Image::Initial { nonce } => {
            let client = BlockingRpcClient::initial_image_from_connected_stream(
                stream,
                reverie::Pid::from_raw(tid),
                nonce,
                *packet.config_digest,
            )
            .map_err(|_| refusal("native Initial admission failed"))?;
            let fatal = unsafe { NativeInitializationIoGuard::arm(native) };
            check_config(client.config(), packet)?;
            Admitted::Initial(fatal, client)
        }
        input::Image::Successor {
            ticket,
            composite_digest,
        } => {
            let client = BlockingRpcClient::successor_from_connected_stream(
                stream,
                reverie::Pid::from_raw(tid),
                reverie_rpc_transport::successor::SuccessorRequest {
                    ticket,
                    expected: reverie_rpc_transport::successor::ExpectedSuccessor {
                        config: *packet.config_digest,
                        state: *composite_digest,
                    },
                },
            )
            .map_err(|_| refusal("native Successor admission failed"))?;
            let fatal = unsafe { NativeInitializationIoGuard::arm(native) };
            check_config(client.config(), packet)?;
            let (consumer, binding) = unsafe {
                consumer::Consumer::new(
                    detcore::native_exec_control::ExecTicket(*ticket),
                    client.state(),
                    tid,
                )
            }?;
            Admitted::Successor(fatal, client, consumer, binding)
        }
    };
    let expected = catalog_wire::Bindings {
        source,
        schema: *packet.config_schema,
        bootstrap: *packet.bootstrap_digest,
    };
    // The admitted actual stream is a retained owner while the closed-root
    // data file is read. Root qualification precedes that stream's admission.
    let held = unsafe { catalog::capture(expected, &admitted) }.map_err(refusal)?;
    if !held
        .data
        .targets
        .iter()
        .any(|target| held.data.objects[target.image as usize].digest == *packet.target_digest)
    {
        return Err(refusal(
            "native admitted target is absent from the closed catalog",
        ));
    }
    let (catalog, backing_objects) = held.into_parts();
    let resume = unsafe { NativeLoaderResume::capture(native) }?;
    let protected =
        unsafe { reverie_inguest::guest::exec_private::native_protected_mappings(native) }?
            .into_iter()
            .map(|(start, end)| {
                Ok(ArgumentRange {
                    start: usize::try_from(start).map_err(|_| {
                        refusal("native protected mapping start exceeds address size")
                    })?,
                    end: usize::try_from(end).map_err(|_| {
                        refusal("native protected mapping end exceeds address size")
                    })?,
                })
            })
            .collect::<io::Result<Vec<_>>>()?
            .into_boxed_slice();
    let (observer, identity) =
        detcore::detlog::parse_tool_output_env_value(packet.observer_identity)
            .ok_or_else(|| refusal("native observer identity is unreadable"))?;
    let (detlog, detlog_identity) =
        detcore::detlog::parse_tool_output_env_value(packet.detlog_identity)
            .ok_or_else(|| refusal("native DETLOG identity is unreadable"))?;
    if observer == detlog
        || identity == detlog_identity
        || detcore::detlog::socket_identity(observer) != Some(identity)
        || detcore::detlog::socket_identity(detlog) != Some(detlog_identity)
    {
        return Err(refusal(
            "native observer and DETLOG actual socket roles differ",
        ));
    }
    unsafe { reserve_socket(NativeSocket::Observer, observer) }
        .map_err(|error| refusal(error.0))?;
    crate::exec_initial::retain_native_observer_identity(identity).map_err(refusal)?;
    let provider_fd = provider::create_socket()?;
    unsafe { reserve_socket(NativeSocket::Provider, provider_fd) }
        .map_err(|error| refusal(error.0))?;
    // The catalog and packet have been authenticated/read. End the regular
    // bootstrap lease before any actual lifecycle or Guest publication.
    unsafe { reverie_inguest::guest::exec_private::retire_native_input(native) }?;
    let generation = match &admitted {
        Admitted::Initial(_, _) => 0,
        Admitted::Successor(_, _, _, binding) => binding.generation,
    };
    let metadata = Metadata {
        source,
        schema: *packet.config_schema,
        config: *packet.config_digest,
        bootstrap: *packet.bootstrap_digest,
        generation,
        forward_policy: packet.forward_policy.to_owned(),
        detlog_object: detlog_identity,
        observer_object: identity,
        resume,
        protected,
        catalog,
        backing_objects,
    };
    let prepared = match admitted {
        Admitted::Initial(fatal, client) => {
            let image = unsafe { reverie_liteinst::prepare_tool_initial_image(client, native) }?;
            // The genuine prepared owner has armed its admitted fatal guard
            // and published the same control. Only now retire physical abort.
            unsafe { fatal.handoff_to_prepared_owner() };
            Prepared::Initial { image, metadata }
        }
        Admitted::Successor(fatal, client, consumer, binding) => {
            let image = unsafe {
                reverie_liteinst::prepare_tool_successor_image(
                    client,
                    native,
                    binding,
                    Box::new(consumer),
                )
            }?;
            unsafe { fatal.handoff_to_prepared_owner() };
            Prepared::Successor { image, metadata }
        }
    };
    Ok((
        prepared,
        socket,
        OsString::from(packet.detlog_identity),
        OsString::from(packet.forward_policy),
    ))
}

fn object(value: &catalog_wire::Object) -> provider::Object {
    provider::Object {
        device: value.device,
        inode: value.inode,
        size: value.size,
        digest: value.digest,
    }
}

/// # Safety
/// Same authenticated pre-loader interval and actual prepared client/controller
/// lineage as prepare. Common DETLOG setup has reserved its real socket first.
pub(crate) unsafe fn install(
    mut prepared: Prepared,
    coordinator: &std::path::Path,
) -> io::Result<Installed> {
    let metadata = match &mut prepared {
        Prepared::Initial { metadata, .. } | Prepared::Successor { metadata, .. } => metadata,
    };
    let targets = metadata
        .catalog
        .targets
        .iter()
        .map(|target| driver::Target {
            path: target.path.clone().into_boxed_slice(),
            digest: metadata.catalog.objects[target.image as usize].digest,
            bootstrap: metadata.bootstrap,
            objects: [
                object(&metadata.catalog.objects[target.stock_loader as usize]),
                object(&metadata.catalog.objects[target.runner as usize]),
                object(&metadata.catalog.objects[target.native_config as usize]),
            ],
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let mut objects = metadata
        .catalog
        .objects
        .iter()
        .filter(|object| !object.executable.is_empty())
        .map(|object| ExecutableObject {
            device: object.device,
            inode: object.inode,
            size: object.size,
            backing: ExecutableBacking::Sealed,
            extents: object
                .executable
                .iter()
                .map(|extent| ExecutableExtent {
                    start: extent.start,
                    end: extent.end,
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        })
        .collect::<Vec<_>>();
    // Physical backing observations are derived under the same actual
    // Initial/Successor root admission; provider rows retain sealed identities.
    // Move them with that owned prepared image, never recapture after Guest work.
    let backing_objects = core::mem::take(&mut metadata.backing_objects);
    objects.extend(backing_objects);
    let objects = objects.into_boxed_slice();
    let protected = metadata.protected.to_vec().into_boxed_slice();
    let mappings = unsafe { ExecutableMappings::new(objects, protected) }.map_err(refusal)?;
    let detlog = reverie_liteinst::tool_output_fd()
        .ok_or_else(|| refusal("native reserved DETLOG absent"))?;
    let observer = socket_fd(NativeSocket::Observer)
        .ok_or_else(|| refusal("native reserved observer absent"))?;
    let provider_fd = socket_fd(NativeSocket::Provider)
        .ok_or_else(|| refusal("native reserved provider absent"))?;
    if detcore::detlog::socket_identity(detlog) != Some(metadata.detlog_object)
        || detcore::detlog::socket_identity(observer) != Some(metadata.observer_object)
    {
        return Err(refusal(
            "native reserved socket objects differ from admission",
        ));
    }
    let authority = driver::Authority {
        config: metadata.config,
        source: metadata.source,
        schema: metadata.schema,
        targets,
        protected: metadata.protected.to_vec().into_boxed_slice(),
        image_epoch: metadata.generation,
        argument_frontend: ReadCapture::SavedPkru,
        detlog_object: metadata.detlog_object,
        observer_object: metadata.observer_object,
        forward_policy: metadata.forward_policy.clone(),
        provider_fd,
        executable_mappings: mappings,
    };
    // Each role expectation comes from the authenticated sealed catalog,
    // independently retained before any later provider transfer. The closed
    // root/watch owner, not these data values, supplies Driver::new authority.
    let driver = Arc::new(unsafe { driver::Driver::new(authority) });
    match prepared {
        Prepared::Initial { image, .. } => Ok(Installed::Initial(unsafe {
            reverie_liteinst::install_tool_initial_image_with_native_exec(
                coordinator,
                image,
                driver,
                qualify_config,
            )
        }?)),
        Prepared::Successor { image, metadata } => Ok(Installed::Successor {
            image: unsafe {
                reverie_liteinst::install_tool_successor_image(coordinator, image, driver)
            }?,
            resume: metadata.resume,
        }),
    }
}

impl Installed {
    pub(crate) unsafe fn finish(self) -> io::Result<()> {
        crate::exec_initial::install_dispatch_observer();
        match self {
            Self::Initial(image) => {
                if let Err(error) = reverie_liteinst::initialize_initial_image_lifecycle(&image) {
                    // Keep the actual image/fatal guard owned through reporting;
                    // returning this same error still drops the armed guard.
                    unsafe {
                        reverie_inguest::guest::exec_private::report_initial_refusal(
                            reverie_inguest::guest::exec_private::InitialRefusalStage::Lifecycle,
                            &error,
                        )
                    };
                    return Err(error);
                }
                image.finish()
            }
            Self::Successor { image, resume } => {
                let mut entry = unsafe { resume.image_entry() };
                image.run_post_exec_before_loader(&mut entry)?;
                unsafe { image.finish() }
            }
        }
    }
}
