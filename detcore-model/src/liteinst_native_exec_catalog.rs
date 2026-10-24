//! Bounded sealed native target catalog data shared by the host and runtime.
//!
//! Decoding verifies shape and internal references only. Neither a digest nor
//! this file grants namespace, artifact, process or native-exec authority. The
//! actual initializer must first hold the authenticated immutable root and
//! compare independently compiled bindings before using any decoded row.

use std::fmt;

/// Fixed path in the independently admitted common root.
pub const PATH: &[u8] = b"/run/hermit/native/catalog";
/// Separate file limit; the existing control packet remains at 4096 bytes.
pub const MAX_BYTES: usize = 1024 * 1024;
/// Maximum original target paths, including explicitly declared aliases.
pub const MAX_TARGETS: usize = 128;
/// Maximum distinct sealed backing objects.
pub const MAX_OBJECTS: usize = 512;
/// Maximum executable PT_LOAD extents in one object.
pub const MAX_EXTENTS: usize = 64;
/// Maximum raw absolute path bytes, without a terminating NUL.
pub const MAX_PATH: usize = 4095;
/// Maximum individual sealed object bytes.
pub const MAX_OBJECT_BYTES: u64 = 256 * 1024 * 1024;
/// Catalog wire magic, independent from HLNEXEC1.
pub const MAGIC: [u8; 8] = *b"HLNCAT01";
/// Current exact version. Version one had no authenticated pathname backing.
pub const VERSION: u16 = 2;
/// Fixed header size.
pub const HEADER_BYTES: usize = 104;

/// Independently held expected values supplied by the authenticated initializer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bindings {
    /// Actual combined compiled native input digest.
    pub source: [u8; 32],
    /// Entire sixteen-character ASCII hex Config schema.
    pub schema: [u8; 16],
    /// Independently retained qualified bootstrap digest.
    pub bootstrap: [u8; 32],
}
/// Page-rounded file-offset interval of an actual executable PT_LOAD.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Extent {
    /// Inclusive first byte.
    pub start: u64,
    /// Exclusive final byte.
    pub end: u64,
}
/// One distinct actual sealed backing object; paths never replace its identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Object {
    /// Kernel device of the retained sealed file.
    pub device: u64,
    /// Kernel inode of the retained sealed file.
    pub inode: u64,
    /// Exact file size, excluding page rounding.
    pub size: u64,
    /// SHA256 of all actual file bytes.
    pub digest: [u8; 32],
    /// Primary resolved placement in the admitted immutable root, if present.
    /// This is a source-to-backing join, never a replacement for source identity.
    pub backing_path: Option<Vec<u8>>,
    /// Executable file extents. Non-executable config objects have none.
    pub executable: Vec<Extent>,
}
/// One generic caller-declared dynamic target and its retained role indices.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    /// Exact original exec path, including an explicitly declared alias.
    pub path: Vec<u8>,
    /// Index of the target object.
    pub image: u16,
    /// Index of its independently retained original stock interpreter.
    pub stock_loader: u16,
    /// Index of the qualified private runner.
    pub runner: u16,
    /// Index of the sealed native config bytes.
    pub native_config: u16,
}
/// Owned bounded data. Construction and decoding confer no admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Catalog {
    /// Complete expected data bindings.
    pub bindings: Bindings,
    /// Unique actual backing objects.
    pub objects: Vec<Object>,
    /// Unique exact original target paths.
    pub targets: Vec<Target>,
}
/// Shape or independent binding disagreement, never a Linux exec errno.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Wrong complete file bounds.
    Size,
    /// Wrong magic.
    Magic,
    /// Unsupported version.
    Version,
    /// Nonzero reserved bytes.
    Reserved,
    /// Truncation, count or trailing-byte mismatch.
    Length,
    /// Invalid complete Config schema or zero binding.
    Binding,
    /// Invalid, duplicate or escaping raw absolute target path.
    Path,
    /// Invalid, duplicate or unsupported sealed object identity.
    Object,
    /// Invalid or overlapping executable file intervals.
    Extent,
    /// Missing, aliased or wrong-kind role index.
    Role,
    /// Actual bindings disagree with independent expectations.
    Mismatch,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "native catalog {self:?}")
    }
}
impl std::error::Error for Error {}

fn path_valid(path: &[u8]) -> bool {
    path.len() > 1
        && path.len() <= MAX_PATH
        && path[0] == b'/'
        && !path.contains(&0)
        && path[1..]
            .split(|b| *b == b'/')
            .all(|part| !part.is_empty() && part != b"." && part != b"..")
}
impl Catalog {
    /// Validate all counts, raw paths, extents and role references.
    pub fn validate(&self) -> Result<(), Error> {
        let binding = self.bindings;
        if binding.source == [0; 32]
            || binding.bootstrap == [0; 32]
            || !binding.schema.iter().all(u8::is_ascii_hexdigit)
        {
            return Err(Error::Binding);
        }
        if self.targets.is_empty()
            || self.targets.len() > MAX_TARGETS
            || self.objects.is_empty()
            || self.objects.len() > MAX_OBJECTS
        {
            return Err(Error::Length);
        }
        for (i, object) in self.objects.iter().enumerate() {
            if object.inode == 0
                || object.size == 0
                || object.size > MAX_OBJECT_BYTES
                || object.digest == [0; 32]
                || self.objects[..i]
                    .iter()
                    .any(|other| other.device == object.device && other.inode == object.inode)
            {
                return Err(Error::Object);
            }
            if let Some(path) = &object.backing_path
                && (!path_valid(path)
                    || path == PATH
                    || self.objects[..i]
                        .iter()
                        .any(|other| other.backing_path.as_ref() == Some(path)))
            {
                return Err(Error::Path);
            }
            if object.executable.len() > MAX_EXTENTS {
                return Err(Error::Extent);
            }
            let limit = object.size.checked_add(4095).ok_or(Error::Extent)? & !4095;
            let mut previous = 0;
            for extent in &object.executable {
                if extent.start % 4096 != 0
                    || extent.end % 4096 != 0
                    || extent.start < previous
                    || extent.start >= extent.end
                    || extent.end > limit
                {
                    return Err(Error::Extent);
                }
                previous = extent.end;
            }
        }
        for (i, target) in self.targets.iter().enumerate() {
            if !path_valid(&target.path) || self.targets[..i].iter().any(|t| t.path == target.path)
            {
                return Err(Error::Path);
            }
            let indices = [
                target.image,
                target.stock_loader,
                target.runner,
                target.native_config,
            ];
            for (i, index) in indices.iter().enumerate() {
                if usize::from(*index) >= self.objects.len() || indices[..i].contains(index) {
                    return Err(Error::Role);
                }
            }
            if [target.image, target.stock_loader, target.runner]
                .iter()
                .any(|index| self.objects[usize::from(*index)].executable.is_empty())
                || !self.objects[usize::from(target.native_config)]
                    .executable
                    .is_empty()
            {
                return Err(Error::Role);
            }
            if self.objects[usize::from(target.image)]
                .backing_path
                .is_none()
            {
                return Err(Error::Role);
            }
        }
        Ok(())
    }
    /// Compare the entire bindings against independent authenticated expectations.
    pub fn check_bindings(&self, expected: Bindings) -> Result<(), Error> {
        if self.bindings != expected {
            return Err(Error::Mismatch);
        }
        Ok(())
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}
impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let end = self.position.checked_add(N).ok_or(Error::Length)?;
        let value = self.bytes.get(self.position..end).ok_or(Error::Length)?;
        self.position = end;
        value.try_into().map_err(|_| Error::Length)
    }
    fn word(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.take()?))
    }
    fn wide(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.take()?))
    }
    fn reserved<const N: usize>(&mut self) -> Result<(), Error> {
        if self.take::<N>()? != [0; N] {
            return Err(Error::Reserved);
        }
        Ok(())
    }
}
/// Decode exact complete bounded data, with no v0 or old packet fallback.
pub fn decode(bytes: &[u8]) -> Result<Catalog, Error> {
    if !(HEADER_BYTES..=MAX_BYTES).contains(&bytes.len()) {
        return Err(Error::Size);
    }
    let mut reader = Reader { bytes, position: 0 };
    if reader.take::<8>()? != MAGIC {
        return Err(Error::Magic);
    }
    if reader.word()? != VERSION {
        return Err(Error::Version);
    }
    reader.reserved::<2>()?;
    if u32::from_le_bytes(reader.take()?) as usize != bytes.len() {
        return Err(Error::Size);
    }
    let bindings = Bindings {
        source: reader.take()?,
        schema: reader.take()?,
        bootstrap: reader.take()?,
    };
    let targets = usize::from(reader.word()?);
    let objects = usize::from(reader.word()?);
    reader.reserved::<4>()?;
    if targets == 0 || targets > MAX_TARGETS || objects == 0 || objects > MAX_OBJECTS {
        return Err(Error::Length);
    }
    let mut catalog = Catalog {
        bindings,
        objects: Vec::with_capacity(objects),
        targets: Vec::with_capacity(targets),
    };
    for _ in 0..objects {
        let (device, inode, size, digest) = (
            reader.wide()?,
            reader.wide()?,
            reader.wide()?,
            reader.take()?,
        );
        let count = usize::from(reader.word()?);
        let path_length = usize::from(reader.word()?);
        reader.reserved::<4>()?;
        if count > MAX_EXTENTS {
            return Err(Error::Extent);
        }
        if path_length > MAX_PATH {
            return Err(Error::Path);
        }
        let mut executable = Vec::with_capacity(count);
        for _ in 0..count {
            executable.push(Extent {
                start: reader.wide()?,
                end: reader.wide()?,
            });
        }
        let backing_path = if path_length == 0 {
            None
        } else {
            let end = reader
                .position
                .checked_add(path_length)
                .ok_or(Error::Length)?;
            let path = bytes
                .get(reader.position..end)
                .ok_or(Error::Length)?
                .to_vec();
            reader.position = end;
            Some(path)
        };
        catalog.objects.push(Object {
            device,
            inode,
            size,
            digest,
            backing_path,
            executable,
        });
    }
    for _ in 0..targets {
        let length = usize::from(reader.word()?);
        let (image, stock_loader, runner, native_config) = (
            reader.word()?,
            reader.word()?,
            reader.word()?,
            reader.word()?,
        );
        reader.reserved::<6>()?;
        if length == 0 || length > MAX_PATH {
            return Err(Error::Path);
        }
        let end = reader.position.checked_add(length).ok_or(Error::Length)?;
        let path = bytes
            .get(reader.position..end)
            .ok_or(Error::Length)?
            .to_vec();
        reader.position = end;
        catalog.targets.push(Target {
            path,
            image,
            stock_loader,
            runner,
            native_config,
        });
    }
    if reader.position != bytes.len() {
        return Err(Error::Length);
    }
    catalog.validate()?;
    Ok(catalog)
}
/// Encode canonical bytes after validation. No packet bound is widened.
pub fn encode(catalog: &Catalog) -> Result<Vec<u8>, Error> {
    catalog.validate()?;
    let mut length = HEADER_BYTES;
    for object in &catalog.objects {
        length = length
            .checked_add(
                64 + object.executable.len() * 16
                    + object.backing_path.as_ref().map_or(0, Vec::len),
            )
            .ok_or(Error::Size)?;
    }
    for target in &catalog.targets {
        length = length
            .checked_add(16 + target.path.len())
            .ok_or(Error::Size)?;
    }
    if length > MAX_BYTES {
        return Err(Error::Size);
    }
    let mut bytes = Vec::with_capacity(length);
    bytes.extend(MAGIC);
    bytes.extend(VERSION.to_le_bytes());
    bytes.extend([0; 2]);
    bytes.extend((length as u32).to_le_bytes());
    bytes.extend(catalog.bindings.source);
    bytes.extend(catalog.bindings.schema);
    bytes.extend(catalog.bindings.bootstrap);
    bytes.extend((catalog.targets.len() as u16).to_le_bytes());
    bytes.extend((catalog.objects.len() as u16).to_le_bytes());
    bytes.extend([0; 4]);
    for object in &catalog.objects {
        bytes.extend(object.device.to_le_bytes());
        bytes.extend(object.inode.to_le_bytes());
        bytes.extend(object.size.to_le_bytes());
        bytes.extend(object.digest);
        bytes.extend((object.executable.len() as u16).to_le_bytes());
        bytes.extend((object.backing_path.as_ref().map_or(0, Vec::len) as u16).to_le_bytes());
        bytes.extend([0; 4]);
        for extent in &object.executable {
            bytes.extend(extent.start.to_le_bytes());
            bytes.extend(extent.end.to_le_bytes());
        }
        if let Some(path) = &object.backing_path {
            bytes.extend(path);
        }
    }
    for target in &catalog.targets {
        bytes.extend((target.path.len() as u16).to_le_bytes());
        for index in [
            target.image,
            target.stock_loader,
            target.runner,
            target.native_config,
        ] {
            bytes.extend(index.to_le_bytes());
        }
        bytes.extend([0; 6]);
        bytes.extend(&target.path);
    }
    debug_assert_eq!(bytes.len(), length);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Catalog {
        Catalog {
            bindings: Bindings {
                source: [2; 32],
                schema: *b"0123456789abcdef",
                bootstrap: [3; 32],
            },
            objects: (0..4)
                .map(|i| Object {
                    device: 2,
                    inode: i + 1,
                    size: 4097,
                    digest: [i as u8 + 1; 32],
                    backing_path: (i != 3).then(|| format!("/objects/{i}").into_bytes()),
                    executable: if i == 3 {
                        vec![]
                    } else {
                        vec![Extent {
                            start: 0,
                            end: 8192,
                        }]
                    },
                })
                .collect(),
            targets: vec![Target {
                path: b"/arbitrary/program".to_vec(),
                image: 0,
                stock_loader: 1,
                runner: 2,
                native_config: 3,
            }],
        }
    }
    #[test]
    fn exact_roundtrip_and_independent_bindings() {
        let mut catalog = fixture();
        catalog.targets[0].path.push(0xff);
        let bytes = encode(&catalog).unwrap();
        assert_eq!(&bytes[..8], b"HLNCAT01");
        assert_eq!(decode(&bytes).unwrap(), catalog);
        let mut expected = catalog.bindings;
        expected.source[31] ^= 1;
        assert_eq!(catalog.check_bindings(expected), Err(Error::Mismatch));
    }
    #[test]
    fn exact_shape_never_accepts_truncation_or_trailing_data() {
        let bytes = encode(&fixture()).unwrap();
        for end in 0..bytes.len() {
            assert!(decode(&bytes[..end]).is_err());
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(decode(&extra).is_err());
        for offset in [0, 8, 10, 12, 100, 101, 102, 103] {
            let mut wrong = bytes.clone();
            wrong[offset] ^= 0x80;
            assert!(decode(&wrong).is_err(), "offset {offset}");
        }
    }
    #[test]
    fn reference_path_and_executable_extent_negatives() {
        for path in [
            b"relative".as_slice(),
            b"/a/../b",
            b"/a//b",
            b"/a/.",
            b"/a/",
            b"/a\0b",
        ] {
            let mut bad = fixture();
            bad.targets[0].path = path.to_vec();
            assert_eq!(encode(&bad), Err(Error::Path));
        }
        let mut bad = fixture();
        bad.targets.push(bad.targets[0].clone());
        assert_eq!(encode(&bad), Err(Error::Path));
        let mut bad = fixture();
        bad.targets[0].native_config = 0;
        assert_eq!(encode(&bad), Err(Error::Role));
        let mut bad = fixture();
        bad.targets[0].runner = 400;
        assert_eq!(encode(&bad), Err(Error::Role));
        let mut bad = fixture();
        bad.objects[0].executable[0].end = 12288;
        assert_eq!(encode(&bad), Err(Error::Extent));
        let mut bad = fixture();
        bad.objects[0].executable.push(Extent {
            start: 4096,
            end: 8192,
        });
        assert_eq!(encode(&bad), Err(Error::Extent));
        let mut bad = fixture();
        bad.objects[1].inode = bad.objects[0].inode;
        assert_eq!(encode(&bad), Err(Error::Object));
    }

    #[test]
    fn backing_paths_preserve_source_identity_and_reject_unbound_roles() {
        let original = fixture();
        let encoded = encode(&original).unwrap();
        assert_eq!(u16::from_le_bytes(encoded[8..10].try_into().unwrap()), 2);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.objects[0].device, original.objects[0].device);
        assert_eq!(decoded.objects[0].inode, original.objects[0].inode);
        assert_eq!(decoded.objects[0].digest, original.objects[0].digest);
        for path in [b"/a/../b".as_slice(), b"/a//b", PATH, b"relative", b"/a\0b"] {
            let mut bad = original.clone();
            bad.objects[0].backing_path = Some(path.to_vec());
            assert_eq!(encode(&bad), Err(Error::Path));
        }
        let mut duplicate = original.clone();
        duplicate.objects[1].backing_path = duplicate.objects[0].backing_path.clone();
        assert_eq!(encode(&duplicate), Err(Error::Path));
        let mut missing = original.clone();
        missing.objects[0].backing_path = None;
        assert_eq!(encode(&missing), Err(Error::Role));
        let mut old = encoded;
        old[8..10].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(decode(&old), Err(Error::Version));
    }
}
