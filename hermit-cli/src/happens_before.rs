/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Debug-info resolution for happens-before anchors.
//!
//! The anchor/edge *model* lives in [`detcore_model::happens_before`]; it leaves
//! `func`/`file:line` code locations unresolved (a [`Position::Rip`] with
//! `addr: None`). This module turns those human-legible locations into concrete
//! instruction pointers by reading the target binary's symbol table (for
//! function entry addresses) and DWARF line program (for source lines), and
//! provides the reverse mapping (address → function/file/line) used by
//! introspection / `--list-events`.
//!
//! It builds *owned* indices in a single pass and drops the mmap/DWARF context,
//! so the resolver has no self-referential lifetimes and is cheap to keep around.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::Path;

use addr2line::gimli;
use anyhow::Context as _;
use detcore_model::happens_before::Anchor;
use detcore_model::happens_before::CodeLocation;
use detcore_model::happens_before::HappensBeforeProgram;
use detcore_model::happens_before::Position;
use object::Object;
use object::ObjectSection;
use object::ObjectSymbol;
use object::SymbolKind;

/// The virtual-address extent of a function symbol.
#[derive(Clone, Copy, Debug)]
struct FuncExtent {
    /// Entry virtual address.
    addr: u64,
    /// Size in bytes (0 when the symbol table does not record one).
    size: u64,
}

impl FuncExtent {
    /// True when `addr` falls within `[self.addr, self.addr + self.size)`. A
    /// zero-sized symbol only matches its exact entry address.
    fn contains(&self, addr: u64) -> bool {
        if self.size == 0 {
            addr == self.addr
        } else {
            addr >= self.addr && addr < self.addr + self.size
        }
    }
}

/// One resolved source-line row: an address and the file/line it maps to.
#[derive(Clone, Debug)]
struct LineRow {
    addr: u64,
    file: Option<String>,
    line: u32,
}

/// A resolved source location for introspection output.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedLocation {
    /// Function name containing the address, if known.
    pub function: Option<String>,
    /// Source file basename, if known.
    pub file: Option<String>,
    /// Source line, if known.
    pub line: Option<u32>,
}

/// Owned debug-info indices for one binary.
pub struct DebugInfoResolver {
    /// Function name → extent (from the ELF symbol table).
    functions: BTreeMap<String, FuncExtent>,
    /// Line rows sorted ascending by address.
    lines: Vec<LineRow>,
}

impl DebugInfoResolver {
    /// Read `path` and build the symbol and line-number indices.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let data = std::fs::read(path)
            .with_context(|| format!("reading binary for debug info: {}", path.display()))?;
        Self::from_bytes(&data)
            .with_context(|| format!("parsing debug info from {}", path.display()))
    }

    /// Build the indices from in-memory object bytes.
    pub fn from_bytes(data: &[u8]) -> anyhow::Result<Self> {
        let object = object::File::parse(data)?;

        // --- function symbols -------------------------------------------------
        let mut functions: BTreeMap<String, FuncExtent> = BTreeMap::new();
        for sym in object.symbols() {
            if sym.kind() != SymbolKind::Text {
                continue;
            }
            let addr = sym.address();
            if addr == 0 {
                continue;
            }
            if let Ok(name) = sym.name() {
                if name.is_empty() {
                    continue;
                }
                // Prefer the first definition (typically the primary symbol) but
                // upgrade a zero-sized entry if we later learn a real size.
                functions
                    .entry(name.to_string())
                    .and_modify(|e| {
                        if e.size == 0 && sym.size() > 0 {
                            *e = FuncExtent {
                                addr,
                                size: sym.size(),
                            };
                        }
                    })
                    .or_insert(FuncExtent {
                        addr,
                        size: sym.size(),
                    });
            }
        }

        // --- DWARF line rows --------------------------------------------------
        let lines = Self::load_line_rows(&object).unwrap_or_default();

        Ok(Self { functions, lines })
    }

    /// Parse the DWARF line programs into an address-sorted list of rows.
    fn load_line_rows(object: &object::File<'_>) -> anyhow::Result<Vec<LineRow>> {
        let endian = if object.is_little_endian() {
            gimli::RunTimeEndian::Little
        } else {
            gimli::RunTimeEndian::Big
        };

        let load_section = |id: gimli::SectionId| -> Result<Cow<'_, [u8]>, gimli::Error> {
            match object.section_by_name(id.name()) {
                Some(section) => Ok(section
                    .uncompressed_data()
                    .unwrap_or(Cow::Borrowed(&[][..]))),
                None => Ok(Cow::Borrowed(&[][..])),
            }
        };

        let dwarf_sections = gimli::DwarfSections::load(load_section)?;
        let dwarf = dwarf_sections.borrow(|section| gimli::EndianSlice::new(section, endian));

        let mut rows: Vec<LineRow> = Vec::new();
        let mut units = dwarf.units();
        while let Some(header) = units.next()? {
            let unit = dwarf.unit(header)?;
            let Some(program) = unit.line_program.clone() else {
                continue;
            };
            let mut state = program.rows();
            while let Some((header, row)) = state.next_row()? {
                if row.end_sequence() {
                    continue;
                }
                let Some(line) = row.line() else {
                    continue;
                };
                let file = row.file(header).and_then(|file_entry| {
                    dwarf
                        .attr_string(&unit, file_entry.path_name())
                        .ok()
                        .and_then(|r| r.to_string().ok().map(basename))
                });
                rows.push(LineRow {
                    addr: row.address(),
                    file,
                    line: line.get() as u32,
                });
            }
        }
        rows.sort_by_key(|r| r.addr);
        Ok(rows)
    }

    /// True when no debug/symbol information was found at all.
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty() && self.lines.is_empty()
    }

    /// The entry address of a named function, if present in the symbol table.
    pub fn function_addr(&self, name: &str) -> Option<u64> {
        self.functions.get(name).map(|e| e.addr)
    }

    /// The lowest address whose line row matches `line` (and `file`, when given),
    /// optionally constrained to a `(addr, size)` extent (a zero size matches only
    /// the exact entry address).
    pub fn line_addr(
        &self,
        file: Option<&str>,
        line: u32,
        within: Option<(u64, u64)>,
    ) -> Option<u64> {
        let file_base = file.map(basename);
        let extent = within.map(|(addr, size)| FuncExtent { addr, size });
        self.lines
            .iter()
            .filter(|r| r.line == line)
            .filter(|r| match (&file_base, &r.file) {
                (Some(want), Some(have)) => want == have,
                (Some(_), None) => false,
                (None, _) => true,
            })
            .filter(|r| extent.map(|f| f.contains(r.addr)).unwrap_or(true))
            .map(|r| r.addr)
            .min()
    }

    /// Resolve a [`CodeLocation`] to a concrete address, preferring
    /// function+line (the address of `line` inside `function`), then bare
    /// function entry, then file:line.
    pub fn resolve_location(&self, loc: &CodeLocation) -> Option<u64> {
        match (&loc.function, loc.line) {
            (Some(func), Some(line)) => {
                let extent = self.functions.get(func).copied();
                self.line_addr(loc.file.as_deref(), line, extent.map(|e| (e.addr, e.size)))
                    // Fall back to the function entry if the exact line row is
                    // absent (e.g. no DWARF, or inlined line).
                    .or(extent.map(|e| e.addr))
            }
            (Some(func), None) => self.function_addr(func),
            (None, Some(line)) => self.line_addr(loc.file.as_deref(), line, None),
            (None, None) => None,
        }
    }

    /// Describe an address in source terms (nearest preceding line row and the
    /// enclosing function), for introspection output.
    pub fn describe(&self, addr: u64) -> ResolvedLocation {
        // Enclosing function: the entry whose extent contains `addr`, else the
        // closest preceding entry.
        let function = self
            .functions
            .iter()
            .filter(|(_, e)| e.contains(addr) || e.addr <= addr)
            .max_by_key(|(_, e)| e.addr)
            .map(|(name, _)| name.clone());

        // Nearest preceding line row.
        let idx = self.lines.partition_point(|r| r.addr <= addr);
        let line_row = if idx > 0 {
            self.lines.get(idx - 1)
        } else {
            None
        };

        ResolvedLocation {
            function,
            file: line_row.and_then(|r| r.file.clone()),
            line: line_row.map(|r| r.line),
        }
    }
}

/// Resolve every anchor in `program` whose position is an unresolved RIP carrying
/// a code location. Returns the names of anchors that could not be resolved.
pub fn resolve_program(
    program: &mut HappensBeforeProgram,
    resolver: &DebugInfoResolver,
) -> Vec<String> {
    let mut unresolved = Vec::new();
    for (name, anchor) in program.anchors.iter_mut() {
        if let Position::Rip { addr: None, nth } = &anchor.position {
            let nth = *nth;
            if let Some(resolved) = resolver.resolve_location(&anchor.location) {
                anchor.position = Position::Rip {
                    addr: Some(resolved),
                    nth,
                };
            } else {
                unresolved.push(name.clone());
            }
        }
    }
    unresolved
}

/// Render one anchor for `--list-events`-style introspection, showing the
/// resolved address and source description when available.
pub fn describe_anchor(anchor: &Anchor, resolver: Option<&DebugInfoResolver>) -> String {
    let mut out = format!("{}", anchor);
    if let (Position::Rip { addr: Some(a), .. }, Some(r)) = (&anchor.position, resolver) {
        let d = r.describe(*a);
        let where_ = match (&d.function, &d.file, d.line) {
            (Some(f), _, Some(l)) => format!(" -> {:#x} ({}:{})", a, f, l),
            (Some(f), _, None) => format!(" -> {:#x} ({})", a, f),
            _ => format!(" -> {:#x}", a),
        };
        out.push_str(&where_);
    }
    out
}

/// The final path component of a possibly-slashed path string.
fn basename(s: &str) -> String {
    match s.rsplit_once('/') {
        Some((_, base)) => base.to_string(),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use detcore_model::happens_before::HappensBeforeSpec;

    use super::*;

    /// Compile a tiny -g fixture to a temp path; returns None if no C compiler.
    /// Each call gets a unique directory so parallel tests never collide.
    fn compile_fixture() -> Option<std::path::PathBuf> {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        let cc = ["cc", "gcc", "clang"]
            .into_iter()
            .find(|c| Command::new(c).arg("--version").output().is_ok())?;

        let uniq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hb_fixture_{}_{}", std::process::id(), uniq));
        std::fs::create_dir_all(&dir).ok()?;
        let src = dir.join("fixture.c");
        let bin = dir.join("fixture");
        std::fs::write(
            &src,
            "int free_buffer(int x) { return x + 1; }\n\
             int read_buffer(int x) { return x * 2; }\n\
             int main(void) { return free_buffer(read_buffer(3)); }\n",
        )
        .ok()?;
        let status = Command::new(cc)
            .args(["-g", "-O0", "-o"])
            .arg(&bin)
            .arg(&src)
            .status()
            .ok()?;
        if status.success() { Some(bin) } else { None }
    }

    #[test]
    fn resolves_function_and_line_from_dwarf() {
        let Some(bin) = compile_fixture() else {
            eprintln!("skipping: no C compiler available");
            return;
        };
        let resolver = DebugInfoResolver::open(&bin).unwrap();
        assert!(!resolver.is_empty(), "expected symbols/lines in -g binary");

        // Function entry resolves to a nonzero address.
        let free_addr = resolver.function_addr("free_buffer");
        assert!(free_addr.is_some(), "free_buffer should resolve");
        assert_ne!(free_addr.unwrap(), 0);

        // free_buffer is defined on line 1; its line row should fall within the
        // function extent.
        let loc = CodeLocation {
            function: Some("free_buffer".to_string()),
            file: None,
            line: Some(1),
        };
        let addr = resolver.resolve_location(&loc);
        assert!(addr.is_some(), "func+line should resolve");

        // Reverse description of the entry address names the function.
        let d = resolver.describe(free_addr.unwrap());
        assert_eq!(d.function.as_deref(), Some("free_buffer"));

        let _ = std::fs::remove_dir_all(bin.parent().unwrap());
    }

    #[test]
    fn resolve_program_fills_rips() {
        let Some(bin) = compile_fixture() else {
            eprintln!("skipping: no C compiler available");
            return;
        };
        let resolver = DebugInfoResolver::open(&bin).unwrap();

        let json = r#"{
          "version": 1,
          "threads": { "t": {"label": "t"} },
          "events": {
            "A": {"thread": "t", "func": "free_buffer"},
            "B": {"thread": "t", "func": "read_buffer", "line": 2}
          },
          "edges": [ {"before": "A", "after": "B"} ]
        }"#;
        let mut prog = HappensBeforeSpec::from_json(json)
            .unwrap()
            .normalize()
            .unwrap();
        assert_eq!(prog.unresolved_locations().count(), 2);

        let missing = resolve_program(&mut prog, &resolver);
        assert!(
            missing.is_empty(),
            "all locations should resolve: {:?}",
            missing
        );
        assert_eq!(prog.unresolved_locations().count(), 0);

        for anchor in prog.anchors.values() {
            match &anchor.position {
                Position::Rip { addr: Some(a), .. } => assert_ne!(*a, 0),
                other => panic!("expected resolved RIP, got {:?}", other),
            }
        }

        let _ = std::fs::remove_dir_all(bin.parent().unwrap());
    }

    #[test]
    fn missing_function_reported() {
        let Some(bin) = compile_fixture() else {
            eprintln!("skipping: no C compiler available");
            return;
        };
        let resolver = DebugInfoResolver::open(&bin).unwrap();
        let json = r#"{
          "version": 1,
          "events": { "A": {"thread": "1", "func": "no_such_function"} },
          "edges": []
        }"#;
        let mut prog = HappensBeforeSpec::from_json(json)
            .unwrap()
            .normalize()
            .unwrap();
        let missing = resolve_program(&mut prog, &resolver);
        assert_eq!(missing, vec!["A".to_string()]);

        let _ = std::fs::remove_dir_all(bin.parent().unwrap());
    }
}
