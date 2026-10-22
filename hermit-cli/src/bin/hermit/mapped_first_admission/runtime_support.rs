//! Shared launch mechanics used by the opt-in route and unchanged physical controls.
//! No source qualification, Ready, policy implementation or second reaper.
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::sync::Arc;
use std::time::Duration;

use reverie_inguest::launcher::exec_stop::ChildOwner;
use reverie_inguest::launcher::exec_stop::CleanupReceipt;
use reverie_inguest::launcher::exec_stop::CopiedInitialData;
use reverie_inguest::launcher::exec_stop::InitialExecStop;
use reverie_inguest::launcher::exec_stop::LaunchFailure;
use reverie_inguest::launcher::exec_stop::physical::CancelledPreparation;
use reverie_inguest::launcher::exec_stop::physical::Preparation;
use reverie_inguest::launcher::exec_stop::physical::PreparationFailure;
use reverie_inguest::launcher::exec_stop::physical::PreparedMapping;
use reverie_loader::artifact::RUNTIME_WINDOW;
use reverie_loader::artifact::TOOL_REGION;
use reverie_loader::handoff::Extent;
use reverie_loader::handoff::{self};
use reverie_loader::image::Image;
use reverie_loader::image::Policy;
use reverie_loader::image::ValidatedImage;
use reverie_rpc_transport::RpcServer;
use sha2::Digest;
use sha2::Sha256;

use super::MappedFirstAdmission;
use super::packet_lease::SealedPacketLease;
pub(crate) const WAIT: Duration = Duration::from_secs(10);
pub(crate) fn need(condition: bool, message: &str) -> io::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message.to_owned()))
    }
}
pub(crate) fn rpc(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
pub(crate) async fn bounded<T>(
    future: impl std::future::Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::time::timeout(WAIT, future).await.map_err(rpc)?
}
pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}

// H owns no kernel wait mechanism: each slot retains the actual existing R
// capability. Place this value outside ALL block_on/catch_unwind expressions.
pub(crate) struct RetainedPhysical<'fd> {
    pub(crate) initial: Option<InitialExecStop<'fd>>,
    pub(crate) mapped: Option<PreparedMapping<'fd>>,
    pub(crate) preparation_failure: Option<Box<PreparationFailure<'fd>>>,
    pub(crate) unconfirmed_initial: Option<Box<ChildOwner<'fd>>>,
    pub(crate) launch_failure: Option<LaunchFailure<'fd>>,
    pub(crate) receipt: CleanupReceipt,
    pub(crate) cancelled: Option<CancelledPreparation>,
}
impl<'fd> RetainedPhysical<'fd> {
    // Construct this BEFORE spawn; store either returned capability immediately.
    pub(crate) fn empty(receipt: CleanupReceipt) -> Self {
        Self {
            initial: None,
            mapped: None,
            preparation_failure: None,
            unconfirmed_initial: None,
            launch_failure: None,
            receipt,
            cancelled: None,
        }
    }
    pub(crate) fn retain_spawn(
        &mut self,
        result: std::result::Result<InitialExecStop<'fd>, LaunchFailure<'fd>>,
    ) {
        match result {
            Ok(initial) => self.initial = Some(initial),
            Err(failure) => self.launch_failure = Some(failure),
        }
    }
    pub(crate) fn has_cleanup_capability(&self) -> bool {
        self.initial.is_some()
            || self.mapped.is_some()
            || self.preparation_failure.is_some()
            || self.unconfirmed_initial.is_some()
            || self.launch_failure.is_some()
    }
    pub(crate) fn initial(&self) -> io::Result<&InitialExecStop<'fd>> {
        self.initial
            .as_ref()
            .ok_or_else(|| match self.launch_failure.as_ref() {
                Some(failure) => io::Error::other(format!("actual launch refused: {failure:?}")),
                None => invalid("actual initial stop absent"),
            })
    }
    pub(crate) fn mapped(&self) -> io::Result<&PreparedMapping<'fd>> {
        self.mapped
            .as_ref()
            .ok_or_else(|| invalid("actual prepared mapping absent"))
    }
    /// Safety: exact existing Preparation contract, including original-thread
    /// affinity, inherited leases, real context/PKRU and external containment.
    /// Image execution qualification is NOT asserted by this map-only adapter.
    pub(crate) unsafe fn prepare(&mut self, inputs: Preparation<'fd>) -> io::Result<()> {
        let initial = self
            .initial
            .take()
            .ok_or_else(|| invalid("actual initial stop absent"))?;
        // No await or fallible diagnostic between consuming and retaining owner.
        match unsafe { initial.prepare_mapping(inputs) } {
            Ok(mapped) => {
                self.mapped = Some(mapped);
                Ok(())
            }
            Err(failure) => {
                self.preparation_failure = Some(failure);
                match self.preparation_failure.as_ref() {
                    Some(retained) => Err(io::Error::other(format!(
                        "actual mapping refused: {}",
                        retained.error
                    ))),
                    None => Err(invalid("retained preparation failure absent")),
                }
            }
        }
    }
    pub(crate) fn retire_once(&mut self) {
        if let Some(initial) = self.initial.take()
            && let Err(owner) = initial.cancel()
        {
            self.unconfirmed_initial = Some(owner);
        }
        if let Some(mapped) = self.mapped.take() {
            match mapped.cancel() {
                Ok(cancelled) => self.cancelled = Some(cancelled),
                Err(mapped) => self.mapped = Some(*mapped),
            }
        }
        if let Some(failure) = self.preparation_failure.as_mut() {
            let _ = failure.retry_cleanup();
        }
        if let Some(failure) = self.launch_failure.as_mut() {
            let _ = failure.retry_cleanup();
        }
        if let Some(owner) = self.unconfirmed_initial.take()
            && let Err(owner) = (*owner).cancel()
        {
            self.unconfirmed_initial = Some(owner);
        }
    }
}
// No Drop success receipt, new reaper, raw task escape, wait loop, entry API,
// generic proof constructor or test suppression is added. The enclosing root
// MUST retain this component and all its leases until actual confirmed cleanup.

pub(crate) type NumericResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub(crate) const PAGE: u64 = 4096;
pub(crate) fn require(ok: bool, why: &str) -> NumericResult<()> {
    if ok { Ok(()) } else { Err(why.into()) }
}

/// Exact data coordinates supplied from independently retained output, then
/// checked against the SAME full image bytes below. These are not source proof.
pub(crate) struct ObservedImage {
    pub(crate) image_sha256: [u8; 32],
    pub(crate) layout_address: u64,
    pub(crate) reference_address: u64,
    pub(crate) handler_address: u64,
    pub(crate) handler_bytes: usize,
    pub(crate) handler_sha256: [u8; 32],
    pub(crate) dev_null_address: u64,
    pub(crate) sites: [[u64; 4]; 4],
    pub(crate) ordinary_post_ips: Vec<u64>,
}
pub(crate) struct ProposedPlacement {
    pub(crate) roles: [Extent; 5],
    pub(crate) free_band: Extent,
    pub(crate) filter_scratch: u64,
}
pub(crate) struct PacketMappingWire {
    pub(crate) bytes: Vec<u8>,
    pub(crate) address: u64,
}
pub(crate) struct Context {
    pub(crate) pid: u64,
    pub(crate) regs: [u64; 27],
    pub(crate) mask: u64,
    pub(crate) input_fd: u64,
}
pub(crate) struct WireInputs<'a, 'packet> {
    pub(crate) runtime: &'a [u8],
    pub(crate) observed: &'a ObservedImage,
    pub(crate) roles: &'a [Extent; 5],
    pub(crate) descriptor: &'a [u64; 12],
    pub(crate) reference: u64,
    pub(crate) copied: &'a CopiedInitialData,
    pub(crate) pkru: u32,
    pub(crate) free_band: Extent,
    pub(crate) packet: &'a SealedPacketLease<'packet>,
}

pub(crate) fn checked_image<'a>(
    runtime: &'a [u8],
    observed: &ObservedImage,
) -> NumericResult<(ValidatedImage<'a>, [u64; 12])> {
    require(
        !runtime.is_empty() && runtime.len() <= 512 * 1024 * 1024,
        "retained image size",
    )?;
    require(
        <[u8; 32]>::from(Sha256::digest(runtime)) == observed.image_sha256,
        "retained image SHA256",
    )?;
    let image = Image::parse(runtime)
        .map_err(|e| format!("ELF: {e:?}"))?
        .validate(&Policy {
            window: RUNTIME_WINDOW,
            allow_tls: true,
        })
        .map_err(|e| format!("ELF policy: {e:?}"))?;
    image
        .private_runtime_metadata()
        .map_err(|e| format!("private metadata: {e:?}"))?;
    let raw = exact_ro(&image, runtime, observed.layout_address, 96)?;
    let mut descriptor = [0; 12];
    for (out, row) in descriptor.iter_mut().zip(raw.as_chunks::<8>().0) {
        *out = u64::from_le_bytes(*row);
    }
    require(
        descriptor[..6]
            == [
                u64::from_le_bytes(*b"HLMLAY01"),
                1,
                96,
                handoff::VERSION,
                handoff::HEADER_BYTES as u64,
                handoff::MAX_BYTES as u64,
            ]
            && descriptor[6] > 0
            && descriptor[8] > 0
            && descriptor[7].is_power_of_two()
            && descriptor[9].is_power_of_two()
            && descriptor[10] == RUNTIME_WINDOW.start + PAGE
            && descriptor[11] == descriptor[10] + 0x7c,
        "actual layout descriptor",
    )?;
    exact_ro(&image, runtime, observed.reference_address, PAGE as usize)?;
    Ok((image, descriptor))
}
pub(crate) fn exact_ro(
    image: &ValidatedImage<'_>,
    _bytes: &[u8],
    address: u64,
    len: usize,
) -> NumericResult<Vec<u8>> {
    let end = address
        .checked_add(u64::try_from(len)?)
        .ok_or("RO span overflow")?;
    for segment in image.segments() {
        let initialized = image
            .mapped_file_bytes(segment)
            .map_err(|e| format!("RO load: {e:?}"))?;
        let initialized_end = segment
            .memory
            .start
            .checked_add(u64::try_from(initialized.len())?)
            .ok_or("RO initialized end overflow")?;
        if segment.permissions.read
            && !segment.permissions.write
            && !segment.permissions.execute
            && segment.memory.start <= address
            && end <= initialized_end
        {
            let at = usize::try_from(address - segment.memory.start)?;
            return initialized
                .get(at..at.checked_add(len).ok_or("RO offset overflow")?)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| "RO initialized bytes".into());
        }
    }
    Err("descriptor/reference must come from actual file-backed R-only segment".into())
}

pub(crate) fn exact_rx<'a>(
    image: &ValidatedImage<'a>,
    address: u64,
    length: usize,
) -> NumericResult<&'a [u8]> {
    let end = address
        .checked_add(length as u64)
        .ok_or("RX span overflow")?;
    for load in image.segments() {
        let initialized = image
            .mapped_file_bytes(load)
            .map_err(|e| format!("RX load: {e:?}"))?;
        if load.permissions.read
            && load.permissions.execute
            && !load.permissions.write
            && load.memory.start <= address
            && end
                <= load
                    .memory
                    .start
                    .checked_add(
                        load.file
                            .end
                            .checked_sub(load.file.start)
                            .ok_or("RX file size")?,
                    )
                    .ok_or("RX end overflow")?
        {
            let at = usize::try_from(address - load.memory.start)?;
            return initialized
                .get(at..at.checked_add(length).ok_or("RX offset overflow")?)
                .ok_or_else(|| "RX file bounds".into());
        }
    }
    Err("mapping-only observation outside initialized RX bytes".into())
}
pub(crate) fn aligned(value: u64, alignment: u64) -> NumericResult<u64> {
    require(alignment.is_power_of_two(), "numeric alignment")?;
    Ok(value
        .checked_add(alignment - 1)
        .ok_or("alignment overflow")?
        & !(alignment - 1))
}
pub(crate) fn payload_words(
    data: &CopiedInitialData,
    address: u64,
    count: u64,
) -> NumericResult<Vec<u64>> {
    let start = usize::try_from(address.checked_sub(data.address()).ok_or("copied offset")?)?;
    let length = usize::try_from(count.checked_mul(8).ok_or("copied words overflow")?)?;
    let end = start.checked_add(length).ok_or("copied end overflow")?;
    Ok(data
        .bytes()
        .get(start..end)
        .ok_or("copied words bounds")?
        .as_chunks::<8>()
        .0
        .iter()
        .map(|row| u64::from_le_bytes(*row))
        .collect())
}
pub(crate) fn payload_bytes(
    data: &CopiedInitialData,
    address: u64,
    length: u64,
) -> NumericResult<&[u8]> {
    let start = usize::try_from(address.checked_sub(data.address()).ok_or("copied offset")?)?;
    let end = start
        .checked_add(usize::try_from(length)?)
        .ok_or("copied end overflow")?;
    data.bytes()
        .get(start..end)
        .ok_or_else(|| "copied bytes bounds".into())
}
impl PacketMappingWire {
    pub(crate) fn build(
        image: &ValidatedImage<'_>,
        inputs: WireInputs<'_, '_>,
        context: Context,
    ) -> NumericResult<Self> {
        let WireInputs {
            runtime,
            observed,
            roles,
            descriptor,
            reference,
            copied: data,
            pkru,
            free_band,
            packet,
        } = inputs;
        let address = roles[3].start.checked_add(PAGE).ok_or("handoff overflow")?;
        let length = usize::try_from(
            roles[3]
                .end
                .checked_sub(address)
                .and_then(|end| end.checked_sub(PAGE))
                .ok_or("handoff guards")?,
        )?;
        require(
            length <= handoff::MAX_BYTES && length >= 3 * PAGE as usize,
            "complete numeric fixture handoff needs header/table/payload space",
        )?;
        let mut bytes = vec![0; length];
        let at = usize::try_from(
            data.address()
                .checked_sub(address)
                .ok_or("payload offset")?,
        )?;
        let end = at
            .checked_add(data.bytes().len())
            .ok_or("payload overflow")?;
        bytes
            .get_mut(at..end)
            .ok_or("payload bounds")?
            .copy_from_slice(data.bytes());
        let metadata = data.image_metadata();
        let handler = observed.handler_address;
        let handler_len = observed.handler_bytes;
        require(
            handler_len > 0 && handler_len <= 64 * 1024,
            "handler observation bounds",
        )?;
        require(
            <[u8; 32]>::from(Sha256::digest(exact_rx(image, handler, handler_len)?))
                == observed.handler_sha256,
            "actual handler bytes",
        )?;
        let dev_null = observed.dev_null_address;
        require(
            exact_ro(image, runtime, dev_null, 10)? == b"/dev/null\0",
            "actual ELF dev-null literal",
        )?;
        let mut code = Vec::new();
        for load in image.segments() {
            if load.permissions.read && load.permissions.execute && !load.permissions.write {
                let end = load
                    .memory
                    .start
                    .checked_add(load.file.end - load.file.start)
                    .ok_or("code end")?;
                code.push(handoff::Code {
                    start: load.memory.start,
                    end,
                });
            }
        }
        let mut sites = Vec::new();
        let mut seen = [false; 4];
        for values in observed.sites {
            let [ip, post, number, operation] = values;
            let slot = match (operation, number) {
                (1, 158) => 0,
                (2, 218) => 1,
                (3, 7) => 2,
                (4, 2) => 3,
                _ => return Err("measured operation/number".into()),
            };
            require(!seen[slot], "duplicate measured role")?;
            seen[slot] = true;
            require(
                ip.checked_add(2) == Some(post) && exact_rx(image, ip, 2)? == [0x0f, 0x05],
                "actual syscall/post instruction bytes",
            )?;
            sites.push(handoff::SiteRecord {
                syscall_ip: ip,
                return_ip: post,
                number,
                operation,
            });
        }
        sites.sort_unstable_by_key(|site| site.return_ip);
        let tool_init = &observed.ordinary_post_ips;
        require(
            !tool_init.is_empty() && tool_init.len() <= 4096,
            "finite actual ToolInit sites",
        )?;
        require(
            tool_init.windows(2).all(|pair| pair[0] < pair[1]),
            "unique sorted ToolInit sites",
        )?;
        for &post in tool_init {
            require(
                exact_rx(image, post.checked_sub(2).ok_or("tool post")?, 2)? == [0x0f, 0x05],
                "actual ordinary syscall bytes, not ToolInit authorization",
            )?;
        }
        let maps = data.planned_maps();
        let locations = data.locations();
        let env = payload_words(data, locations.envp, locations.envp_words)?;
        let aux = payload_words(data, locations.raw_aux, locations.raw_aux_words)?;
        let name = payload_bytes(data, locations.program_name, locations.program_name_bytes)?;
        let control = roles[4].start.checked_add(PAGE).ok_or("control overflow")?;
        let relay = aligned(
            control.checked_add(descriptor[6]).ok_or("relay overflow")?,
            descriptor[9],
        )?;
        let tls = &metadata.tls;
        let backing = data.tls_geometry().backing();
        let mut header = handoff::Header {
            prefix: [
                handoff::MAGIC,
                handoff::VERSION,
                length as u64,
                handoff::HEADER_BYTES as u64,
                0,
                0,
                0,
                0,
            ],
            binding: [[0; 4]; 5], // Explicit numeric fixture; no digest is an authority claim.
            roles: *roles,
            registers: context.regs,
            scalars: [
                context.pid,
                control,
                data.control_bytes(),
                relay,
                data.relay_bytes(),
                context.input_fd,
                u64::from(pkru),
                context.mask,
                data.tls_geometry().pthread().start as u64,
                handler,
                reference,
                PAGE,
                metadata.gnu_stack_bytes,
                dev_null,
                0,
                0,
            ],
            tables: std::array::from_fn(|_| handoff::Table {
                offset: 0,
                count: 0,
                stride: 0,
            }),
            tls: [
                metadata.program_headers.start,
                56,
                u64::from(metadata.program_header_count),
                tls.address,
                tls.file_bytes,
                tls.memory_bytes,
                tls.alignment,
                backing.start as u64,
                backing.bytes as u64,
                tls.load.memory.start,
                tls.load.memory.end - tls.load.memory.start,
                tls.load.file.end - tls.load.file.start,
            ],
        };
        // Reserve complete metadata tables below the already placed copied payload.
        let mut cursor = handoff::HEADER_BYTES as u64;
        for (index, count, stride) in [
            (0, maps.len(), 32),
            (1, code.len(), 16),
            (2, sites.len(), 32),
            (3, packet.bytes().len(), 1),
            (7, 10, 1),
            (8, tool_init.len(), 8),
        ] {
            cursor = aligned(cursor, 8)?;
            header.tables[index] = handoff::Table {
                offset: cursor,
                count: count as u64,
                stride,
            };
            cursor = cursor
                .checked_add((count as u64).checked_mul(stride).ok_or("table size")?)
                .ok_or("table end")?;
        }
        require(cursor <= at as u64, "metadata would overlap copied payload")?;
        for (index, location, count, stride) in [
            (4, locations.envp, locations.envp_words, 8),
            (5, locations.raw_aux, locations.raw_aux_words, 8),
            (6, locations.program_name, locations.program_name_bytes, 1),
        ] {
            header.tables[index] = handoff::Table {
                offset: location.checked_sub(address).ok_or("table offset")?,
                count,
                stride,
            };
        }
        let tables = handoff::TableData {
            maps,
            code: &code,
            sites: &sites,
            packet: packet.bytes(),
            env: &env,
            aux: &aux,
            program_name: name,
            dev_null: b"/dev/null\0",
            tool_init,
        };
        handoff::serialize(&header, &tables, address, &mut bytes)
            .map_err(|e| format!("numeric fixture serialize: {e:?}"))?;
        let expected = reverie_loader::mapped_inputs::ConsumerLayout {
            free_band,
            region: Extent {
                start: TOOL_REGION.start,
                end: TOOL_REGION.end,
            },
            control_bytes: descriptor[6],
            relay_bytes: descriptor[8],
            control_align: descriptor[7],
            relay_align: descriptor[9],
        };
        // Run the unchanged complete production predicate before mapping.
        // The readable-union correction is an explicit source prerequisite.
        reverie_loader::mapped_inputs::ProductionMappedInputs::validate_at(
            &bytes, address, expected,
        )
        .map_err(|e| format!("complete numeric baseline refused: {e:?}"))?;
        let view = handoff::View::parse(&bytes, address)
            .map_err(|e| format!("packet mapping parse: {e:?}"))?;
        require(
            view.table(3)
                .map_err(|e| format!("packet table: {e:?}"))?
                .bytes()
                == packet.bytes(),
            "serialized table3 lost genuine packet",
        )?;
        Ok(Self { bytes, address })
    }
}

pub(crate) fn memfd_bytes(bytes: &[u8], seal: bool) -> io::Result<File> {
    let file = File::from(
        crate::verify_inputs::new_unsealed_memfd(c"mapped-packet-control").map_err(rpc)?,
    );
    file.write_all_at(bytes, 0)?;
    file.set_len(bytes.len() as u64)?;
    if seal {
        seal_memfd(&file)?;
    }
    Ok(file)
}
pub(crate) fn seal_memfd(file: &File) -> io::Result<()> {
    // SAFETY: this root owns the retained, already-filled memfd. No guest or
    // concurrent writer runs. Positional writes never advanced its shared offset.
    if unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
// A caught payload may have a panicking destructor. Retain the first payload
// (including an earlier setup/observation panic); never drop a secondary payload
// while actual child retirement is unconfirmed. This exceptional leak grants no
// cleanup credit. The containing fixture remains externally bounded.
pub(crate) type PanicPayload = Box<dyn std::any::Any + Send + 'static>;
pub(crate) fn retain_unwind<T>(
    first: &mut Option<PanicPayload>,
    operation: impl FnOnce() -> T,
) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(value) => Some(value),
        Err(payload) => {
            if first.is_none() {
                *first = Some(payload);
            } else {
                std::mem::forget(payload);
            }
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetirementStep {
    Complete,
    RetryRetained,
    RetainFrame,
}

// This is host-side ownership control, not a terminal receipt constructor.
// The physical caller supplies the existing receipt snapshot and capability
// presence AFTER the caught attempt. Generic parameters permit data-only unwind
// tests to exercise this exact transition without manufacturing a ChildOwner.
pub(crate) fn guarded_retirement<T>(
    owner: &mut T,
    first: &mut Option<PanicPayload>,
    retire: impl FnOnce(&mut T),
    snapshot: impl FnOnce(&T) -> (bool, bool, bool),
) -> RetirementStep {
    let _ = retain_unwind(first, || retire(owner));
    let Some((created, confirmed, retained)) = retain_unwind(first, || snapshot(owner)) else {
        // An unreadable receipt provides no permission to release the frame.
        return RetirementStep::RetainFrame;
    };
    if !created || confirmed {
        RetirementStep::Complete
    } else if retained {
        RetirementStep::RetryRetained
    } else {
        RetirementStep::RetainFrame
    }
}

pub(crate) fn retain_unconfirmed_frame() -> ! {
    // Failure-only last resort: no diagnostics, TLS-dependent park, allocation,
    // callback, return, panic or resource destruction. Keep every outer backing,
    // descriptor/admission owner and caught payload live until the existing
    // whole-subtree containment terminates this FAILED/INCOMPLETE fixture. CPU
    // spin is deliberately limited to this lost-capability path, not retries.
    loop {
        std::hint::spin_loop();
    }
}

pub(crate) fn cleanup_all(
    a: &mut RetainedPhysical<'_>,
    b: &mut RetainedPhysical<'_>,
    first: &mut Option<PanicPayload>,
) {
    // No second wait/reaper, future, detach, entry or timeout release. Existing
    // R cancellation does its bounded original-owner wait. External containment
    // bounds this entire synchronous root if actual retirement never confirms.
    loop {
        let sa = guarded_retirement(
            a,
            first,
            |root| root.retire_once(),
            |root| {
                let state = root.receipt.snapshot();
                (
                    state.child_created,
                    state.confirmed,
                    root.has_cleanup_capability(),
                )
            },
        );
        let sb = guarded_retirement(
            b,
            first,
            |root| root.retire_once(),
            |root| {
                let state = root.receipt.snapshot();
                (
                    state.child_created,
                    state.confirmed,
                    root.has_cleanup_capability(),
                )
            },
        );
        if sa == RetirementStep::Complete && sb == RetirementStep::Complete {
            return;
        }
        if sa == RetirementStep::RetainFrame || sb == RetirementStep::RetainFrame {
            retain_unconfirmed_frame();
        }
        // Only actual retained capabilities are retried by retire_once. A lost
        // capability is never reconstructed from a PID, receipt or kill request.
    }
}

/// This fixture only invokes the real server's existing cooperative serve_until;
/// no replacement coordinator or GlobalTool is defined.
pub(crate) struct Serving {
    pub(crate) task: Option<tokio::task::JoinHandle<Result<(), reverie_rpc_transport::RpcError>>>,
    pub(crate) shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    pub(crate) connections: reverie_rpc_transport::ConnectionMonitor,
    pub(crate) issues: reverie_rpc_transport::RpcIssueMonitor,
    pub(crate) owner: Arc<MappedFirstAdmission>,
}
impl Drop for Serving {
    fn drop(&mut self) {
        // Emergency path retains failure and stops owned tasks; Child Drop still
        // kills/reaps. A successful test MUST take explicit joined cleanup below.
        if self.task.is_some() {
            self.owner
                .fail(&"physical admission test aborted before joined cleanup");
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            if let Some(task) = self.task.take() {
                task.abort();
            }
        }
    }
}
impl Serving {
    pub(crate) async fn close(&mut self) -> io::Result<()> {
        // Children have already been killed/reaped or exited. Require actual
        // physical watcher drain and actual per-connection drain before shutdown.
        self.owner
            .exit_admission()
            .drain(WAIT)
            .await
            .map_err(|error| rpc(format!("physical watch drain failed: {error:?}")))?;
        bounded(async {
            self.connections.wait_for_idle().await;
            Ok(())
        })
        .await?;
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let mut task = self
            .task
            .take()
            .ok_or_else(|| io::Error::other("server already closed"))?;
        let joined = tokio::time::timeout(WAIT, &mut task).await;
        match joined {
            Ok(result) => result.map_err(rpc)?.map_err(rpc),
            Err(error) => {
                task.abort();
                let _ = task.await;
                Err(rpc(error))
            }
        }
    }
}

/// The future (and its RpcServer pathname cleanup) drops before the retained
/// directory FD, including cancellation before the first poll. This prevents
/// an aborted server from unlinking through a later reuse of that descriptor.
pub(crate) struct DirectoryBoundServer<F> {
    pub(crate) server: std::pin::Pin<Box<F>>,
    pub(crate) _directory_fd: std::fs::File,
}
impl<F: std::future::Future> std::future::Future for DirectoryBoundServer<F> {
    type Output = F::Output;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.get_mut().server.as_mut().poll(context)
    }
}

pub(crate) struct Admission {
    pub(crate) owner: Arc<MappedFirstAdmission>,
    pub(crate) server: Option<RpcServer<detcore::GlobalState>>,
    pub(crate) issues: Option<reverie_rpc_transport::RpcIssueMonitor>,
    pub(crate) drained: bool,
}
impl Drop for Admission {
    fn drop(&mut self) {
        // Normal explicit drain runs first. Abort/unwind must at least poison
        // the actual watch; the original held ptracer still owns terminal wait.
        if !self.drained {
            self.owner
                .fail(&"held packet control ended before explicit watch drain");
        }
    }
}
#[cfg(test)]
pub(crate) async fn admit(
    admissions: &mut Vec<Admission>,
    global: &Arc<detcore::GlobalState>,
    config: &detcore::Config,
    socket: &std::path::Path,
    stop: &InitialExecStop<'_>,
    detlog: &UnixStream,
) -> io::Result<Arc<MappedFirstAdmission>> {
    let server = RpcServer::bind_exclusive(socket, global.clone(), config.clone()).map_err(rpc)?;
    let owner = MappedFirstAdmission::prepare(
        global.clone(),
        stop.duplicate_birth_pidfd().map_err(rpc)?,
        socket.as_os_str().as_bytes(),
        detlog.as_fd(),
        detlog.as_raw_fd(),
    )
    .await?;
    retain_admission(admissions, server, owner)
}

pub(crate) async fn admit_with_observer(
    admissions: &mut Vec<Admission>,
    global: &Arc<detcore::GlobalState>,
    config: &detcore::Config,
    socket: &std::path::Path,
    stop: &InitialExecStop<'_>,
    detlog: &UnixStream,
    observer: &UnixStream,
) -> io::Result<Arc<MappedFirstAdmission>> {
    let server = RpcServer::bind_exclusive(socket, global.clone(), config.clone()).map_err(rpc)?;
    let owner = MappedFirstAdmission::prepare_with_observer(
        global.clone(),
        stop.duplicate_birth_pidfd().map_err(rpc)?,
        socket.as_os_str().as_bytes(),
        detlog.as_fd(),
        observer.as_fd(),
    )
    .await?;
    retain_admission(admissions, server, owner)
}

fn retain_admission(
    admissions: &mut Vec<Admission>,
    server: RpcServer<detcore::GlobalState>,
    owner: Arc<MappedFirstAdmission>,
) -> io::Result<Arc<MappedFirstAdmission>> {
    // Retain real watch/server before any further fallible configuration.
    admissions.push(Admission {
        owner: owner.clone(),
        server: Some(server),
        issues: None,
        drained: false,
    });
    let retained = admissions
        .last_mut()
        .ok_or_else(|| rpc("admission slot absent"))?;
    retained.issues = Some(
        owner.configure_server(
            retained
                .server
                .as_mut()
                .ok_or_else(|| rpc("actual RPC server absent"))?,
        )?,
    );
    Ok(owner)
}

pub(crate) fn exact_storage(sealed: &SealedPacketLease<'_>, expected: &[u8]) -> io::Result<()> {
    need(
        sealed.bytes() == expected,
        "lease differs from actual initial_packet",
    )?;
    // Duplicate is a real shared-OFD alias for observation; positional IO leaves
    // its original shared offset unchanged. No path or replacement is used.
    let file = File::from(sealed.fd().try_clone_to_owned()?);
    need(
        file.metadata()?.len() == expected.len() as u64,
        "packet file length differs",
    )?;
    let mut bytes = vec![0; expected.len()];
    file.read_exact_at(&mut bytes, 0)?;
    need(bytes == expected, "sealed inherited OFD content differs")?;
    let mut tail = [0];
    need(
        file.read_at(&mut tail, expected.len() as u64)? == 0,
        "packet EOF differs",
    )?;
    // SAFETY: read-only fcntl/offset queries on the retained shared OFD.
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    let offset = unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_CUR) };
    need(
        seals == (libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL),
        "packet immutable seal set differs",
    )?;
    need(offset == 0, "packet inherited shared offset differs")
}

pub(crate) mod init_trial;
