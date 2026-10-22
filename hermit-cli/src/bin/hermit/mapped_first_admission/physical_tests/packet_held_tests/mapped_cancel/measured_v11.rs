//! Exact V11 retained-image numeric input. NOT execution/source qualification.
//! The same runtime image is sealed; only Initialize enters it and then cancels.
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;

use reverie_loader::artifact::TOOL_REGION;
use reverie_loader::handoff::Extent;
use sha2::Digest;
use sha2::Sha256;

use super::MappedCase;
use super::ObservedImage;
use super::PAGE;
use super::ProposedPlacement;
use super::mapped_packet_control;
use super::rpc;

const IMAGE_BYTES: usize = 31_315_160;
const IMAGE_SHA256: [u8; 32] = [
    0x24, 0xca, 0x0e, 0x02, 0xe8, 0x44, 0xe1, 0x21, 0x29, 0x8c, 0xfb, 0x0b, 0x33, 0x19, 0x03, 0x4e,
    0xdb, 0xc5, 0x4a, 0x03, 0x90, 0x7b, 0xdc, 0x9b, 0x65, 0xbe, 0x5c, 0xb8, 0x76, 0xe3, 0x6e, 0xea,
];

fn observed() -> ObservedImage {
    ObservedImage {
        image_sha256: IMAGE_SHA256,
        layout_address: 0x6000e17bb000,
        reference_address: 0x6000e17ba000,
        handler_address: 0x6000e1684ce0,
        handler_bytes: 2110,
        handler_sha256: [
            0xc5, 0x29, 0x2b, 0x8b, 0x5a, 0xb1, 0x7a, 0x88, 0xb6, 0xdf, 0x98, 0x56, 0x80, 0x55,
            0x39, 0xe1, 0x8b, 0xdb, 0xae, 0x50, 0x56, 0xab, 0x48, 0x49, 0xd8, 0xe3, 0x35, 0x1f,
            0x06, 0xa3, 0xb3, 0x8f,
        ],
        dev_null_address: 0x6000e18e8141,
        // Strictly sorted actual return PCs. Roles remain exact source pairs:
        // SET_FS, secure poll, secure /dev/null open, set_tid_address.
        sites: [
            [0x6000e1002666, 0x6000e1002668, 0x9e, 0x1],
            [0x6000e1797ddb, 0x6000e1797ddd, 0x7, 0x3],
            [0x6000e1797e31, 0x6000e1797e33, 0x2, 0x4],
            [0x6000e179807f, 0x6000e1798081, 0xda, 0x2],
        ],
        ordinary_post_ips: vec![0x6000e1002022],
    }
}

fn placement() -> io::Result<ProposedPlacement> {
    // Existing shared free band DATA_FIRST=1MiB/PAGE, TOOL_BAND.0=65535.
    // These are fallible proposed roles, not observed mappings or initialized
    // arena ownership. Actual copied-input and complete shared validation, then
    // the unchanged no-replace injector, check every role and planned map.
    let first = TOOL_REGION
        .start
        .checked_add(1024 * 1024)
        .ok_or_else(|| rpc("role start overflow"))?;
    let last = TOOL_REGION
        .start
        .checked_add(65535 * PAGE)
        .ok_or_else(|| rpc("free band overflow"))?;
    let mut cursor = first;
    let mut roles = [Extent { start: 0, end: 0 }; 5];
    // Stack, TLS, bootstrap signal stack, handoff, scalar state; guards included.
    for (role, pages) in roles.iter_mut().zip([256_u64, 64, 256, 256, 256]) {
        let bytes = pages
            .checked_mul(PAGE)
            .ok_or_else(|| rpc("role bytes overflow"))?;
        let end = cursor
            .checked_add(bytes)
            .ok_or_else(|| rpc("role end overflow"))?;
        if end > last {
            return Err(rpc("proposed roles exceed shared free band"));
        }
        *role = Extent { start: cursor, end };
        cursor = end;
    }
    let filter_scratch = roles[4]
        .start
        .checked_add(8 * PAGE)
        .ok_or_else(|| rpc("filter staging overflow"))?;
    Ok(ProposedPlacement {
        roles,
        free_band: Extent {
            start: first,
            end: last,
        },
        filter_scratch,
    })
}

fn file_state(file: &File) -> io::Result<[u64; 9]> {
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() != IMAGE_BYTES as u64 || meta.len() > 128 * 1024 * 1024 {
        return Err(rpc(
            "V11 retained image is not the exact bounded regular file",
        ));
    }
    Ok([
        meta.dev(),
        meta.ino(),
        u64::from(meta.mode()),
        meta.nlink(),
        meta.len(),
        meta.mtime() as u64,
        meta.mtime_nsec() as u64,
        meta.ctime() as u64,
        meta.ctime_nsec() as u64,
    ])
}
fn read_image(file: &File) -> io::Result<Vec<u8>> {
    file_state(file)?; // Bound before allocation/read, including on final close.
    let mut bytes = vec![0; IMAGE_BYTES];
    file.read_exact_at(&mut bytes, 0)?;
    if <[u8; 32]>::from(Sha256::digest(&bytes)) != IMAGE_SHA256 {
        return Err(rpc("V11 retained image full SHA256 differs"));
    }
    Ok(bytes)
}

pub(super) fn run(case: MappedCase) -> io::Result<()> {
    // Required explicit file supplied by the bounded runner; missing input is
    // a test error, never an ignored/empty/default-positive fixture.
    let path = std::env::var_os("HERMIT_MAPPED_PACKET_V11_IMAGE")
        .ok_or_else(|| rpc("HERMIT_MAPPED_PACKET_V11_IMAGE is required"))?;
    let file = File::open(path)?;
    let before = file_state(&file)?;
    let bytes = read_image(&file)?;
    let proposed = placement()?;
    let mut coordinates = observed();
    if case == MappedCase::Initialize {
        coordinates.ordinary_post_ips = TOOL_INIT_RETURN_IPS.to_vec();
    }
    // SAFETY: exclusively serial bounded fixture on the original ptracer OS
    // thread. Existing ExecStop creates/retains actual sole-leader births and
    // inherited FD leases. Initialize alone executes private Tool setup to its
    // observation cut; no Guest instruction runs. The runner must
    // retain the actual compatible CPUID/XCR0 domain and original signal/filter
    // lineage and whole-subtree containment required by mapped_packet_control.
    // The typed tuple above is FALLIBLE map-only data, no source authority.
    // Every FD/backing and genuine admission outlives actual retirement inside
    // that lexical root. A lost cleanup capability never returns here.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        mapped_packet_control(case, &bytes, &coordinates, &proposed)
    }));
    // A normal Result failure must close this same retained file as well. A
    // caught root panic reaches here only after its actual confirmed cleanup.
    let close = (|| -> io::Result<()> {
        let after = read_image(&file)?;
        if after != bytes || file_state(&file)? != before {
            return Err(rpc(
                "V11 retained image changed across real map/cancel control",
            ));
        }
        Ok(())
    })();
    match outcome {
        Err(payload) => {
            // Do not hide the original panic after its confirmed cleanup.
            // Any independent close error remains a failing test, never PASS.
            if let Err(error) = close
                && let Err(diagnostic) =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        eprintln!("independent retained-image close failure: {error}")
                    }))
            {
                std::mem::forget(diagnostic);
            }
            std::panic::resume_unwind(payload)
        }
        Ok(result) => match (result, close) {
            (result, Ok(())) => result,
            (result, Err(error)) => Err(rpc(format!(
                "retained-image close: {error}; control: {result:?}"
            ))),
        },
    }
}

// Exact full-decoder observations for IMAGE_SHA256; every preceding opcode is
// also checked against its real RX file bytes before mapping. Not a source proof.
const TOOL_INIT_RETURN_IPS: [u64; 212] = [
    0x6000e1002022,
    0x6000e1002073,
    0x6000e100246e,
    0x6000e1002507,
    0x6000e100253e,
    0x6000e1002668,
    0x6000e1002722,
    0x6000e100272c,
    0x6000e100fed0,
    0x6000e1010033,
    0x6000e1010085,
    0x6000e101009c,
    0x6000e101013c,
    0x6000e1010167,
    0x6000e101055c,
    0x6000e10106bf,
    0x6000e1010711,
    0x6000e1010728,
    0x6000e10107c8,
    0x6000e10107f3,
    0x6000e1010bdc,
    0x6000e1010d3f,
    0x6000e1010d91,
    0x6000e1010da8,
    0x6000e1010e48,
    0x6000e1010e73,
    0x6000e101c564,
    0x6000e1089a5a,
    0x6000e1089a9b,
    0x6000e1089db7,
    0x6000e1089f3b,
    0x6000e1089fec,
    0x6000e108a04d,
    0x6000e108a2d9,
    0x6000e108a3b8,
    0x6000e108a48c,
    0x6000e108a4cd,
    0x6000e108aaec,
    0x6000e108ac34,
    0x6000e108accd,
    0x6000e108ad1d,
    0x6000e108b15d,
    0x6000e108b24c,
    0x6000e108b2eb,
    0x6000e108b326,
    0x6000e108b470,
    0x6000e108b533,
    0x6000e108b621,
    0x6000e108b65b,
    0x6000e108ba3e,
    0x6000e108bb83,
    0x6000e124f88e,
    0x6000e134e5b3,
    0x6000e14d6f13,
    0x6000e150b9fe,
    0x6000e150bafc,
    0x6000e150bbdd,
    0x6000e150bd74,
    0x6000e150be02,
    0x6000e150be19,
    0x6000e150c298,
    0x6000e150c34e,
    0x6000e150c442,
    0x6000e150ee71,
    0x6000e15107a2,
    0x6000e1510930,
    0x6000e1510a7b,
    0x6000e1510c60,
    0x6000e15258bf,
    0x6000e15258de,
    0x6000e1564eaa,
    0x6000e1590ef4,
    0x6000e159105c,
    0x6000e15b93b0,
    0x6000e15b9b87,
    0x6000e15ba4c9,
    0x6000e15ba4e9,
    0x6000e15bc4df,
    0x6000e15bc502,
    0x6000e15bedad,
    0x6000e15beec5,
    0x6000e15f2434,
    0x6000e15f2584,
    0x6000e15fbda8,
    0x6000e15fbe3a,
    0x6000e15fccb2,
    0x6000e15fce18,
    0x6000e15fd1b7,
    0x6000e15fd26e,
    0x6000e15fd4b2,
    0x6000e15fd543,
    0x6000e15fd56b,
    0x6000e15fd7e4,
    0x6000e15fd854,
    0x6000e15fd9dd,
    0x6000e15fda00,
    0x6000e15fe130,
    0x6000e15fe2c6,
    0x6000e15fe659,
    0x6000e15fe675,
    0x6000e15fe75f,
    0x6000e15fe869,
    0x6000e15fe902,
    0x6000e15fe9bc,
    0x6000e15fea8d,
    0x6000e15fed1e,
    0x6000e15fed68,
    0x6000e15fee18,
    0x6000e15fee7b,
    0x6000e15ff207,
    0x6000e15ffaf6,
    0x6000e15ffc89,
    0x6000e160059c,
    0x6000e1635bfb,
    0x6000e1635c07,
    0x6000e1635c34,
    0x6000e1635c40,
    0x6000e1635c75,
    0x6000e1635c83,
    0x6000e16397a5,
    0x6000e1639d0b,
    0x6000e1644975,
    0x6000e1665b09,
    0x6000e1665c09,
    0x6000e1665ca8,
    0x6000e1665cc3,
    0x6000e1665cea,
    0x6000e1665e30,
    0x6000e1665ef7,
    0x6000e1665f12,
    0x6000e1676f51,
    0x6000e1676fd5,
    0x6000e16822b1,
    0x6000e16835d8,
    0x6000e16ccd82,
    0x6000e17976ba,
    0x6000e17976d4,
    0x6000e1797704,
    0x6000e1797720,
    0x6000e1797a20,
    0x6000e1797a38,
    0x6000e1797a5b,
    0x6000e1797a8e,
    0x6000e1797ab2,
    0x6000e1797acc,
    0x6000e1797ae5,
    0x6000e1797ddd,
    0x6000e1797e33,
    0x6000e1798081,
    0x6000e17981ef,
    0x6000e1798279,
    0x6000e1798383,
    0x6000e1798390,
    0x6000e1798426,
    0x6000e1798436,
    0x6000e179844b,
    0x6000e179845e,
    0x6000e179851c,
    0x6000e17985bc,
    0x6000e17989ae,
    0x6000e17989bc,
    0x6000e1798ace,
    0x6000e1798b84,
    0x6000e1798ba7,
    0x6000e1798cbf,
    0x6000e1798ffd,
    0x6000e179900d,
    0x6000e1799048,
    0x6000e179905e,
    0x6000e17990ee,
    0x6000e17990fc,
    0x6000e179912d,
    0x6000e179913c,
    0x6000e179a194,
    0x6000e179a1a1,
    0x6000e179a20e,
    0x6000e179a225,
    0x6000e179a23f,
    0x6000e179af42,
    0x6000e179af61,
    0x6000e179af79,
    0x6000e179afc6,
    0x6000e179afea,
    0x6000e179b001,
    0x6000e179b014,
    0x6000e179b030,
    0x6000e179c2e1,
    0x6000e179c3a3,
    0x6000e179c477,
    0x6000e179c4c2,
    0x6000e179c4ff,
    0x6000e179c538,
    0x6000e179c54c,
    0x6000e179c597,
    0x6000e179c5bc,
    0x6000e179cb0f,
    0x6000e179e1d3,
    0x6000e179e29e,
    0x6000e179e2e2,
    0x6000e179e34e,
    0x6000e179f3e8,
    0x6000e179f3f4,
    0x6000e179f499,
    0x6000e179f541,
    0x6000e179f6dd,
    0x6000e179f71b,
    0x6000e179f73e,
    0x6000e179f754,
    0x6000e179fa87,
    0x6000e179fbab,
    0x6000e17acbe8,
    0x6000e17ace06,
];
