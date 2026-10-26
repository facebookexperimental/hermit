/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Rewriting canonical load/store-exclusive retry loops into single FEAT_LSE
//! atomics, in place and at the same length.
//!
//! Why in place: every instruction keeps its address, so every PC-relative
//! branch, return address, unwind table, symbol and signal PC in the image
//! stays valid, and the rewrite needs no trampoline, no scratch page and no
//! free register beyond the ones the loop itself already destroys.
//!
//! The scratch registers are the loop's own. A retry loop writes its status
//! register unconditionally before anything can read it, and its load
//! destination likewise, so both hold no live value at the loop's entry: the
//! replacement may use them to materialise an operand, and restores the exact
//! value the original left in each one on every exit path.
//!
//! Ordering is carried across exactly, pair to atomic (Arm ARM DDI 0487 L.a,
//! C3.2.6 and C3.2.12):
//!
//! | load | store | atomic |
//! |---|---|---|
//! | LDAXR | STLXR | `AL` |
//! | LDAXR | STXR | `A` |
//! | LDXR | STLXR | `L` |
//! | LDXR | STXR | plain |
//!
//! Only register-only instructions are allowed between the pair, so an
//! acquire taken at the atomic rather than at the load, and a release taken at
//! the atomic rather than at the store, order the same memory accesses. The
//! one place the rewrite is strictly stronger is a comparison that fails:
//! `CASAL` keeps its release semantics where the original simply never
//! executed its `STLXR`. Extra ordering cannot create an execution the
//! original could not have, but it can hide a weak-memory race, which is why
//! this is opt-in.

use std::collections::HashMap;
use std::collections::HashSet;

use super::insn::AluOp;
use super::insn::AtomicOp;
use super::insn::Insn;
use super::insn::Operand;
use super::insn::Ordering;
use super::insn::Reg;
use super::insn::Size;
use super::insn::ZR;
use super::insn::decode;
use super::insn::{self};

/// The loop shapes this rewriter recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Idiom {
    /// `LDxXR; <alu>; STxXR; CBNZ` - read-modify-write.
    Rmw,
    /// `LDxXR; STxXR; CBNZ` - unconditional store, that is, a swap.
    Swap,
    /// `LDxXR; CMP; B.NE out; STxXR; CBNZ` - compare-and-swap.
    CasCompare,
    /// `LDxXR; CBNZ out; STxXR; CBNZ` - store only into a zero location.
    CasZero,
}

/// Why a store-exclusive was left alone. Every exclusive instruction in the
/// scanned range ends up either rewritten or carrying one of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A load-exclusive or store-exclusive pair instruction (LDXP/STXP).
    ExclusivePair,
    /// No load-exclusive on the same address immediately before the store.
    NoPairedLoad,
    /// An instruction in the window is outside the decoded subset.
    Undecoded,
    /// The window is decoded but is not one of the four idioms.
    NotCanonical,
    /// A direct branch elsewhere in the scanned range targets the inside of
    /// the sequence, so the replacement could be entered part-way. Only
    /// direct branches are checked: an indirect branch's target (a jump
    /// table, a landing pad) is not known here.
    BranchIntoSequence,
    /// The idiom matched but its registers break an assumption the
    /// replacement needs; the string names which one.
    Registers(&'static str),
    /// The operation has no single-instruction LSE equivalent.
    UnsupportedOperation,
    /// A branch the replacement must move no longer reaches its target.
    BranchOutOfRange,
    /// A relocation points into the sequence, so the loader writes those
    /// bytes itself, for the instruction that used to be there.
    Relocated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// Virtual address of the first instruction of the sequence.
    pub addr: u64,
    pub idiom: Idiom,
    /// The replacement words, one per original instruction.
    pub words: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedSite {
    /// Virtual address of the store-exclusive (or pair instruction).
    pub addr: u64,
    pub reason: Refusal,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub patched: Vec<Site>,
    pub refused: Vec<RefusedSite>,
    /// Exclusive instructions seen, whether or not they were rewritten.
    pub exclusive_instructions: usize,
}

impl Report {
    pub fn by_idiom(&self) -> HashMap<Idiom, usize> {
        let mut counts = HashMap::new();
        for site in &self.patched {
            *counts.entry(site.idiom).or_default() += 1;
        }
        counts
    }
}

/// A code range to scan: `words` are the instructions at `base`.
pub fn plan(words: &[u32], base: u64) -> Report {
    let decoded: Vec<Insn> = words.iter().copied().map(decode).collect();
    let targets = direct_branch_targets(&decoded, base);
    let addr = |index: usize| base + 4 * index as u64;
    let mut report = Report::default();
    for (index, insn) in decoded.iter().enumerate() {
        match insn {
            Insn::ExclPair => {
                report.exclusive_instructions += 1;
                // Report the store side only, so a pair loop counts once, as
                // a single-register loop does.
                if (words[index] >> 22) & 1 == 0 {
                    report.refused.push(RefusedSite {
                        addr: addr(index),
                        reason: Refusal::ExclusivePair,
                    });
                }
            }
            Insn::LoadExcl { .. } => report.exclusive_instructions += 1,
            Insn::StoreExcl { .. } => {
                report.exclusive_instructions += 1;
                match match_sequence(words, &decoded, index, base) {
                    Ok(site) => {
                        let start = ((site.addr - base) / 4) as usize;
                        let interior_entered = (start + 1..start + site.words.len())
                            .any(|i| targets.contains(&addr(i)));
                        if interior_entered {
                            report.refused.push(RefusedSite {
                                addr: addr(index),
                                reason: Refusal::BranchIntoSequence,
                            });
                        } else {
                            report.patched.push(site);
                        }
                    }
                    Err(reason) => report.refused.push(RefusedSite {
                        addr: addr(index),
                        reason,
                    }),
                }
            }
            _ => {}
        }
    }
    report
}

/// Apply a plan to the same words it was computed from.
pub fn apply(words: &mut [u32], base: u64, report: &Report) {
    for site in &report.patched {
        let index = ((site.addr - base) / 4) as usize;
        words[index..index + site.words.len()].copy_from_slice(&site.words);
    }
}

fn direct_branch_targets(decoded: &[Insn], base: u64) -> HashSet<u64> {
    let mut targets = HashSet::new();
    for (index, insn) in decoded.iter().enumerate() {
        let here = base + 4 * index as u64;
        let offset = match *insn {
            Insn::Branch { offset, .. }
            | Insn::CondBranch { offset, .. }
            | Insn::CompareBranch { offset, .. }
            | Insn::TestBranch { offset } => offset,
            _ => continue,
        };
        targets.insert(here.wrapping_add(offset as u64));
    }
    targets
}

/// Match the sequence ending at the store-exclusive `store`, returning the
/// index of its first instruction, the replacement words and the idiom.
fn match_sequence(
    words: &[u32],
    decoded: &[Insn],
    store: usize,
    base: u64,
) -> Result<Site, Refusal> {
    let (size, store_ord, rs, rv, rn) = match decoded[store] {
        Insn::StoreExcl {
            size,
            ord,
            rs,
            rt,
            rn,
        } => (size, ord, rs, rt, rn),
        _ => unreachable!("caller matched a store-exclusive"),
    };
    // The loop must close with `CBNZ Rs, <load>` right after the store.
    let close = store + 1;
    let (closes_loop, back) = match decoded.get(close) {
        Some(&Insn::CompareBranch {
            nonzero: true,
            sf: false,
            rt,
            offset,
        }) if rt == rs => (true, offset),
        _ => (false, 0),
    };
    if !closes_loop {
        return Err(Refusal::NotCanonical);
    }
    let start_isize = close as i64 + back / 4;
    if start_isize < 0 || start_isize as usize >= store {
        return Err(Refusal::NotCanonical);
    }
    let start = start_isize as usize;
    let (load_size, load_ord, rt) = match decoded[start] {
        Insn::LoadExcl {
            size,
            ord,
            rt,
            rn: load_rn,
        } if load_rn == rn => (size, ord, rt),
        Insn::LoadExcl { .. } => return Err(Refusal::NotCanonical),
        _ => return Err(Refusal::NoPairedLoad),
    };
    if load_size != size {
        return Err(Refusal::NotCanonical);
    }
    if decoded[start + 1..store].contains(&Insn::Other) {
        return Err(Refusal::Undecoded);
    }
    // The status register must be a register the loop itself destroys and
    // nothing else needs: the architecture already requires it to differ from
    // the stored value and the base. It MAY be the load destination, which is
    // what LLVM emits for a compare-and-swap, and each idiom below says
    // whether its replacement can still restore both.
    if rs == ZR || rs == rn || rs == rv {
        return Err(Refusal::Registers("the status register is not free"));
    }
    if rt == rn || rv == rn {
        return Err(Refusal::Registers("a value register is the base register"));
    }
    let ord = Ordering {
        acquire: load_ord.acquire,
        release: store_ord.release,
    };
    let sf = size.is_64();
    let zero_status = insn::mov_reg(false, rs, ZR);
    let (idiom, words) = match (store - start, decoded[start + 1]) {
        (1, _) => (Idiom::Swap, swap(size, ord, rs, rt, rv, rn, zero_status)?),
        (2, alu @ Insn::Alu { .. }) => (
            Idiom::Rmw,
            rmw(
                alu,
                words[start + 1],
                size,
                ord,
                rs,
                rt,
                rv,
                rn,
                sf,
                zero_status,
            )?,
        ),
        (
            2,
            Insn::CompareBranch {
                nonzero: true,
                sf: branch_sf,
                rt: branch_rt,
                offset: branch_offset,
            },
        ) if branch_rt == rt && branch_sf == sf => (
            Idiom::CasZero,
            cas_zero(size, ord, rt, rv, rn, sf, branch_offset, zero_status)?,
        ),
        (3, cmp) => (
            Idiom::CasCompare,
            cas_compare(
                cmp,
                decoded[start + 2],
                words[start + 1],
                size,
                ord,
                rt,
                rv,
                rn,
                sf,
                zero_status,
            )?,
        ),
        _ => return Err(Refusal::NotCanonical),
    };
    Ok(Site {
        addr: base + 4 * start as u64,
        idiom,
        words,
    })
}

/// `LDxXR Rt, [Rn]; STxXR Rs, Rv, [Rn]; CBNZ Rs` becomes one SWP.
fn swap(
    size: Size,
    ord: Ordering,
    rs: Reg,
    rt: Reg,
    rv: Reg,
    rn: Reg,
    zero_status: u32,
) -> Result<Vec<u32>, Refusal> {
    let _ = rs;
    // `SWPA`/`SWPAL` acquire only when their destination is not the zero
    // register, where `LDAXR WZR` acquires regardless (Arm ARM C6.2, the
    // "Operation" of each; the alias is `STxxx`). A loop that discards its
    // loaded value is rare, so refuse rather than special-case it.
    if rt == ZR {
        return Err(Refusal::Registers("the loaded value is discarded"));
    }
    if rv == rt {
        return Err(Refusal::Registers(
            "the stored register is the load destination",
        ));
    }
    Ok(vec![
        insn::atomic(AtomicOp::Swp, size, ord, rv, rt, rn),
        zero_status,
        insn::NOP,
    ])
}

/// `LDxXR Rt, [Rn]; CBNZ Rt, out; STxXR Rs, Rv, [Rn]; CBNZ Rs` stores only
/// into a zero location, which is a compare-and-swap against zero.
#[allow(clippy::too_many_arguments)]
fn cas_zero(
    size: Size,
    ord: Ordering,
    rt: Reg,
    rv: Reg,
    rn: Reg,
    sf: bool,
    branch_offset: i64,
    zero_status: u32,
) -> Result<Vec<u32>, Refusal> {
    if rt == ZR || rv == rt {
        return Err(Refusal::Registers(
            "the load destination must carry the expected value",
        ));
    }
    // The kept branch sits one instruction later than it did, so its
    // displacement shrinks by that instruction to reach the same target.
    let branch =
        insn::compare_branch(true, sf, rt, branch_offset - 4).ok_or(Refusal::BranchOutOfRange)?;
    Ok(vec![
        insn::mov_reg(sf, rt, ZR),
        insn::cas(size, ord, rt, rv, rn),
        branch,
        zero_status,
    ])
}

#[allow(clippy::too_many_arguments)]
fn rmw(
    alu: Insn,
    alu_word: u32,
    size: Size,
    ord: Ordering,
    rs: Reg,
    rt: Reg,
    rv: Reg,
    rn: Reg,
    sf: bool,
    zero_status: u32,
) -> Result<Vec<u32>, Refusal> {
    let (op, alu_sf, rd, alu_rn, operand) = match alu {
        Insn::Alu {
            op,
            sf,
            rd,
            rn,
            operand,
        } => (op, sf, rd, rn, operand),
        _ => return Err(Refusal::NotCanonical),
    };
    // The stored value must be what the operation computed from the loaded one.
    if rd != rv || alu_rn != rt || alu_sf != sf {
        return Err(Refusal::NotCanonical);
    }
    // `LD<op>` with the zero register as its destination is the `ST<op>`
    // alias, which performs no load and, in the A and AL forms, takes no
    // acquire (Arm ARM C6.2). An immediate add or subtract also encodes
    // register 31 as SP rather than the zero register, so a loop that loads
    // into the zero register could match an operation on SP.
    if rt == ZR {
        return Err(Refusal::Registers("the loaded value is discarded"));
    }
    // The atomic carries the operand in Rs and the loaded value in Rt, and
    // the architecture leaves one register in both roles unpredictable.
    if rs == rt {
        return Err(Refusal::Registers(
            "the status register is the load destination",
        ));
    }
    if let Operand::Reg(rm) = operand {
        if rm == rs {
            return Err(Refusal::Registers(
                "the second operand is the status register",
            ));
        }
        if rm == rt {
            return Err(Refusal::Registers("the second operand is the loaded value"));
        }
    }
    // Materialise the atomic's operand in the status register, which the
    // original loop also writes before anything reads it.
    let (materialise, atomic_op) = match (op, operand) {
        (AluOp::Add, Operand::Reg(rm)) => (insn::mov_reg(sf, rs, rm), AtomicOp::Add),
        (AluOp::Add, Operand::Imm(k)) if k <= 0xffff => (insn::movz(sf, rs, k), AtomicOp::Add),
        (AluOp::Sub, Operand::Reg(rm)) => (insn::neg(sf, rs, rm), AtomicOp::Add),
        // MOVN materialises -k as !(k - 1); k is a 12-bit CMP/ADD immediate.
        (AluOp::Sub, Operand::Imm(k)) if (1..=0x10000).contains(&k) => {
            (insn::movn(sf, rs, k - 1), AtomicOp::Add)
        }
        (AluOp::Orr, Operand::Reg(rm)) => (insn::mov_reg(sf, rs, rm), AtomicOp::Set),
        (AluOp::Eor, Operand::Reg(rm)) => (insn::mov_reg(sf, rs, rm), AtomicOp::Eor),
        (AluOp::Bic, Operand::Reg(rm)) => (insn::mov_reg(sf, rs, rm), AtomicOp::Clr),
        // AND clears the bits that are *not* set in the operand.
        (AluOp::And, Operand::Reg(rm)) => (insn::mvn(sf, rs, rm), AtomicOp::Clr),
        _ => return Err(Refusal::UnsupportedOperation),
    };
    Ok(vec![
        materialise,
        insn::atomic(atomic_op, size, ord, rs, rt, rn),
        alu_word,
        zero_status,
    ])
}

#[allow(clippy::too_many_arguments)]
fn cas_compare(
    cmp: Insn,
    branch: Insn,
    cmp_word: u32,
    size: Size,
    ord: Ordering,
    rt: Reg,
    rv: Reg,
    rn: Reg,
    sf: bool,
    zero_status: u32,
) -> Result<Vec<u32>, Refusal> {
    let (cmp_sf, cmp_rn, operand) = match cmp {
        Insn::Cmp { sf, rn, operand } => (sf, rn, operand),
        _ => return Err(Refusal::NotCanonical),
    };
    // B.NE, condition 0b0001.
    let Insn::CondBranch { cond: 1, offset } = branch else {
        return Err(Refusal::NotCanonical);
    };
    // The kept branch sits one instruction later than it did.
    let branch_word = insn::cond_branch(1, offset - 4).ok_or(Refusal::BranchOutOfRange)?;
    if cmp_rn != rt || cmp_sf != sf {
        return Err(Refusal::NotCanonical);
    }
    if rt == ZR || rv == rt {
        return Err(Refusal::Registers(
            "the load destination must carry the expected value",
        ));
    }
    let expected = match operand {
        Operand::Reg(rm) if rm == rt => {
            return Err(Refusal::Registers("the compared value is the loaded value"));
        }
        // A sub-word compare against a register compares the zero-extended
        // loaded value over the full register, while CASB/CASH compare only
        // the low bits, so the two disagree whenever the register's high bits
        // are set. An immediate that fits the access width cannot disagree.
        Operand::Reg(_) if !matches!(size, Size::W | Size::X) => {
            return Err(Refusal::Registers("a sub-word compare against a register"));
        }
        Operand::Reg(rm) => insn::mov_reg(sf, rt, rm),
        Operand::Imm(k) if k >= width_limit(size) => {
            return Err(Refusal::Registers(
                "the compared immediate exceeds the access width",
            ));
        }
        Operand::Imm(k) => insn::movz(sf, rt, k),
    };
    Ok(vec![
        expected,
        insn::cas(size, ord, rt, rv, rn),
        cmp_word,
        branch_word,
        zero_status,
    ])
}

fn width_limit(size: Size) -> u32 {
    match size {
        Size::B => 1 << 8,
        Size::H => 1 << 16,
        _ => u32::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // @generated by the arm64 harness probe/mktests.py: every word below is
    // GNU as's own encoding of the sequence named in the first column, so the
    // expectations are the assembler's and not hand-computed. A replacement
    // keeps every instruction at the address the original one had, so a
    // branch the rewrite moves within its sequence is re-encoded to reach the
    // same absolute target.
    const CASES: &[(&str, Idiom, &[u32], &[u32])] = &[
        (
            "rmw_add_imm_x_al",
            Idiom::Rmw,
            &[0xc85ffc64, 0x91000484, 0xc805fc64, 0x35ffffa5],
            &[0xd2800025, 0xf8e50064, 0x91000484, 0x2a1f03e5],
        ),
        (
            "rmw_sub_imm_x_plain",
            Idiom::Rmw,
            &[0xc85f7c64, 0xd1002084, 0xc8057c64, 0x35ffffa5],
            &[0x928000e5, 0xf8250064, 0xd1002084, 0x2a1f03e5],
        ),
        (
            "rmw_and_reg_w_al",
            Idiom::Rmw,
            &[0x885ffc64, 0x0a060084, 0x8805fc64, 0x35ffffa5],
            &[0x2a2603e5, 0xb8e51064, 0x0a060084, 0x2a1f03e5],
        ),
        (
            "rmw_eor_reg_x_release",
            Idiom::Rmw,
            &[0xc85f7c64, 0xca060084, 0xc805fc64, 0x35ffffa5],
            &[0xaa0603e5, 0xf8652064, 0xca060084, 0x2a1f03e5],
        ),
        (
            "rmw_orr_byte_acquire",
            Idiom::Rmw,
            &[0x085ffc20, 0x2a030002, 0x08047c22, 0x35ffffa4],
            &[0x2a0303e4, 0x38a43020, 0x2a030002, 0x2a1f03e4],
        ),
        (
            "rmw_sub_reg_x_al",
            Idiom::Rmw,
            &[0xc85ffc64, 0xcb060084, 0xc805fc64, 0x35ffffa5],
            &[0xcb0603e5, 0xf8e50064, 0xcb060084, 0x2a1f03e5],
        ),
        (
            "swap_w_al",
            Idiom::Swap,
            &[0x885ffc20, 0x8802fc23, 0x35ffffc2],
            &[0xb8e38020, 0x2a1f03e2, 0xd503201f],
        ),
        (
            "swap_byte_acquire_only",
            Idiom::Swap,
            &[0x085ffc20, 0x08027c23, 0x35ffffc2],
            &[0x38a38020, 0x2a1f03e2, 0xd503201f],
        ),
        (
            "cas_compare_gcc_x_al",
            Idiom::CasCompare,
            &[0xc85ffc66, 0xeb0400df, 0x54000081, 0xc807fc62, 0x35ffff87],
            &[0xaa0403e6, 0xc8e6fc62, 0xeb0400df, 0x54000061, 0x2a1f03e7],
        ),
        (
            "cas_compare_llvm_status_is_destination",
            Idiom::CasCompare,
            &[0xc85ffd4c, 0xeb0b019f, 0x540000a1, 0xc80cfd48, 0x35ffff8c],
            &[0xaa0b03ec, 0xc8ecfd48, 0xeb0b019f, 0x54000081, 0x2a1f03ec],
        ),
        (
            "cas_compare_immediate_halfword",
            Idiom::CasCompare,
            &[0x485ffc20, 0x71001c1f, 0x54000081, 0x4802fc23, 0x35ffff82],
            &[0x528000e0, 0x48e0fc23, 0x71001c1f, 0x54000061, 0x2a1f03e2],
        ),
        (
            "cas_zero_x_al",
            Idiom::CasZero,
            &[0xc85ffc20, 0xb5000080, 0xc802fc23, 0x35ffffa2],
            &[0xaa1f03e0, 0xc8e0fc23, 0xb5000060, 0x2a1f03e2],
        ),
    ];

    /// Sequences that must survive untouched, with the reason they do.
    const REFUSALS: &[(&str, &[u32], Refusal)] = &[
        (
            "pair",
            &[0xc87f8440, 0xc8238440, 0x35ffffc3],
            Refusal::ExclusivePair,
        ),
        (
            "compare_branches_on_equal",
            &[0xc85ffc66, 0xeb0400df, 0x54000080, 0xc807fc62, 0x35ffff87],
            Refusal::NotCanonical,
        ),
        (
            "undecoded_instruction_between_the_pair",
            &[0xc85ffc64, 0x9bc67c84, 0xc805fc64, 0x35ffffa5],
            Refusal::Undecoded,
        ),
        (
            "no_loop_branch",
            &[0xc85ffc64, 0x91000484, 0xc805fc64, 0xd503201f],
            Refusal::NotCanonical,
        ),
        (
            "subword_compare_against_a_register",
            &[0x085ffc20, 0x6b04001f, 0x54000081, 0x0802fc23, 0x35ffff82],
            Refusal::Registers("a sub-word compare against a register"),
        ),
        (
            "memory_access_between_the_pair",
            &[0xc85ffc64, 0xf94000e6, 0xc805fc64, 0x35ffffa5],
            Refusal::Undecoded,
        ),
    ];

    const BASE: u64 = 0x40_0000;

    #[test]
    fn each_canonical_loop_becomes_the_assembler_s_lse_encoding() {
        for (name, idiom, original, expected) in CASES {
            let report = plan(original, BASE);
            assert_eq!(report.refused, vec![], "{name} was refused");
            assert_eq!(report.patched.len(), 1, "{name} matched {report:?}");
            let site = &report.patched[0];
            assert_eq!(site.idiom, *idiom, "{name}");
            assert_eq!(site.addr, BASE, "{name}");
            assert_eq!(
                site.words,
                expected.to_vec(),
                "{name}: got [{}], want [{}]",
                site.words
                    .iter()
                    .map(|w| format!("{w:#010x}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                expected
                    .iter()
                    .map(|w| format!("{w:#010x}"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            let mut words = original.to_vec();
            apply(&mut words, BASE, &report);
            assert_eq!(
                &words[..expected.len()],
                expected.to_vec(),
                "{name} applied"
            );
            // The rewritten code holds no exclusive instruction, so nothing in
            // it can retry.
            let after = plan(&words, BASE);
            assert_eq!(after.exclusive_instructions, 0, "{name} after rewriting");
        }
    }

    #[test]
    fn a_sequence_outside_the_idiom_set_is_left_alone_with_a_reason() {
        for (name, words, reason) in REFUSALS {
            let report = plan(words, BASE);
            assert_eq!(report.patched, vec![], "{name} was rewritten");
            assert_eq!(
                report.refused.iter().map(|r| &r.reason).collect::<Vec<_>>(),
                vec![reason],
                "{name}"
            );
        }
    }

    #[test]
    fn a_branch_into_the_middle_of_a_sequence_refuses_it() {
        let (_, _, original, _) = CASES[0];
        // `b .+8` lands on the second instruction of the loop that follows it.
        let mut words = vec![0x1400_0002];
        words.extend_from_slice(original);
        let report = plan(&words, BASE);
        assert_eq!(report.patched, vec![]);
        assert_eq!(
            report.refused.first().map(|r| r.reason.clone()),
            Some(Refusal::BranchIntoSequence)
        );

        // The loop's own backward branch targets its first instruction, which
        // is where the replacement starts too, so it does not refuse it.
        let report = plan(original, BASE);
        assert_eq!(report.patched.len(), 1);
    }

    #[test]
    fn a_loop_inside_a_larger_region_is_found_at_its_own_address() {
        let (_, _, original, expected) = CASES[8];
        let mut words = vec![insn::NOP, insn::NOP];
        words.extend_from_slice(original);
        words.push(insn::NOP);
        let report = plan(&words, BASE);
        assert_eq!(report.patched.len(), 1, "{report:?}");
        assert_eq!(report.patched[0].addr, BASE + 8);
        apply(&mut words, BASE, &report);
        assert_eq!(
            &words[2..2 + expected.len()],
            expected.to_vec(),
            "applied at an offset"
        );
        assert_eq!(words[0], insn::NOP);
        assert_eq!(*words.last().unwrap(), insn::NOP);
    }

    #[test]
    fn a_loop_that_discards_its_loaded_value_is_refused() {
        // ldaxr xzr, [x3]; add x4, x31, #1; stlxr w5, x4, [x3]; cbnz w5.
        // Register 31 is SP in the immediate add and the zero register in the
        // exclusive, and `LDADD` into the zero register neither loads nor
        // acquires, so this must not be rewritten.
        let words = [0xc85f_fc7f, 0x9100_07e4, 0xc805_fc64, 0x35ff_ffa5];
        let report = plan(&words, BASE);
        assert_eq!(report.patched, vec![]);
        assert_eq!(
            report.refused.first().map(|r| r.reason.clone()),
            Some(Refusal::Registers("the loaded value is discarded"))
        );

        // ldaxr xzr, [x1]; stlxr w2, x3, [x1]; cbnz w2: the swap form.
        let words = [0xc85f_fc3f, 0xc802_fc23, 0x35ff_ffc2];
        let report = plan(&words, BASE);
        assert_eq!(report.patched, vec![]);
        assert_eq!(
            report.refused.first().map(|r| r.reason.clone()),
            Some(Refusal::Registers("the loaded value is discarded"))
        );
    }

    #[test]
    fn every_ordering_combination_maps_to_the_matching_atomic() {
        // ldxr/ldaxr x4,[x3]; add x4,x4,#1; stxr/stlxr w5,x4,[x3]; cbnz w5.
        let orderings = [
            (0xc85f7c64u32, 0xc8057c64u32, 0xf8250064u32), // plain
            (0xc85ffc64, 0xc8057c64, 0xf8a50064),          // acquire only
            (0xc85f7c64, 0xc805fc64, 0xf8650064),          // release only
            (0xc85ffc64, 0xc805fc64, 0xf8e50064),          // acquire-release
        ];
        for (load, store, atomic) in orderings {
            let words = [load, 0x9100_0484, store, 0x35ff_ffa5];
            let report = plan(&words, BASE);
            assert_eq!(report.patched.len(), 1, "{load:#010x}/{store:#010x}");
            assert_eq!(
                report.patched[0].words[1], atomic,
                "{load:#010x}/{store:#010x} produced {:#010x}",
                report.patched[0].words[1]
            );
        }
    }
}
