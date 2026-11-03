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
//! the atomic rather than at the store, order the same memory accesses.
//!
//! The conditional compare-and-swap loops are not rewritten at all: the
//! original stores only when its comparison matches, so on a read-only
//! word a mismatch exits without a store, while a CAS asks for write
//! permission before it compares and would fault where the original
//! does not. Those loops keep their native exclusive form.

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
    /// A conditional compare-and-swap loop (`CasZero` or `CasCompare`):
    /// the original stores only when its comparison matches, so on a
    /// read-only word a mismatch exits without any store, while a CAS
    /// asks for write permission before it compares and would fault.
    /// No same-length replacement preserves that, so the loop is left
    /// as it is.
    ConditionalCas,
    /// A relocation points into the sequence, so the loader writes those
    /// bytes itself, for the instruction that used to be there.
    Relocated,
    /// The scanned range (or another executable section) holds an
    /// instruction that is not positively known not to be a direct
    /// branch, so an interior entry cannot be ruled out.
    UnknownDirectBranch,
}

/// A sequence the rewriter replaces, and its replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// Virtual address of the first instruction of the sequence.
    pub addr: u64,
    /// Which canonical loop it is.
    pub idiom: Idiom,
    /// The replacement words, one per original instruction.
    pub words: Vec<u32>,
}

/// An exclusive the rewriter leaves alone, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedSite {
    /// Virtual address of the store-exclusive (or pair instruction).
    pub addr: u64,
    /// Why it is left alone.
    pub reason: Refusal,
}

/// What planning one run of instruction words found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The sequences to replace.
    pub patched: Vec<Site>,
    /// The exclusives left alone.
    pub refused: Vec<RefusedSite>,
    /// Exclusive instructions seen, whether or not they were rewritten.
    pub exclusive_instructions: usize,
}

impl Report {
    /// The replaced sequences, counted by idiom.
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
    plan_with_external_targets(words, base, &HashSet::new())
}

/// The same, told the direct branch targets of the code OUTSIDE this range:
/// an image's other executable sections can branch into this one, and a
/// sequence those branches reach is refused exactly as one this range reaches
/// would be.
pub fn plan_with_external_targets(
    words: &[u32],
    base: u64,
    external_targets: &HashSet<u64>,
) -> Report {
    let decoded: Vec<Insn> = words.iter().copied().map(decode).collect();
    let unknown_branch = words.iter().any(|w| unknown_direct_branch_possible(*w));
    let branches = direct_branches(&decoded, base);
    let mut targets: HashSet<u64> = branches.iter().map(|(_, target)| *target).collect();
    targets.extend(external_targets.iter().copied());
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
                if unknown_branch {
                    report.refused.push(RefusedSite {
                        addr: addr(index),
                        reason: Refusal::UnknownDirectBranch,
                    });
                    continue;
                }
                match match_sequence(words, &decoded, index, base, &branches, external_targets) {
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

/// Whether `word` might be a direct branch whose target is unknown.
///
/// The entry protection fails closed. Every PC-relative direct branch
/// in the A64 ISA lives in the branch/exception/system group (bits
/// 28:26 = 101) or is one of the families the decoder knows; the data
/// processing and load/store groups contain no direct branch at all.
/// So a word the decoder does not positively recognise is a possible
/// direct branch when it falls in that group — beyond the positively
/// known non-direct members (system/hint space and exception
/// generation, which have no image target, and the indirect
/// branch/return space) — or in the reserved top-level space (bits
/// 28:25 = 0000), which no current instruction occupies. Anything
/// else unrecognised is in a group with no direct branch and cannot
/// be aimed at a sequence's interior.
pub fn unknown_direct_branch_possible(word: u32) -> bool {
    if insn::decode(word) != Insn::Other {
        return false;
    }
    if (word >> 26) & 0x7 == 0b101 {
        // Positively known to have no image target: exception
        // generation (11010100: SVC/HVC/SMC/BRK and kin, which trap),
        // the system/hint space (11010101: MSR/MRS, barriers, hints,
        // CLREX), and the indirect branch/return forms (1101011 with
        // BR/BLR/RET/ERET/DRPS opcodes). Everything else in the group
        // that the decoder does not recognise — a branch family it does
        // not know, or an unallocated encoding — is a possible direct
        // branch.
        let known_non_direct = (word >> 24) & 0xff == 0b11010100
            || (word >> 24) & 0xff == 0b11010101
            || ((word >> 25) & 0x7f == 0b1101011
                && matches!(
                    (word >> 21) & 0xf,
                    0b0000 | 0b0001 | 0b0010 | 0b0100 | 0b0101
                ));
        !known_non_direct
    } else {
        (word >> 25) & 0xf == 0
    }
}

/// Apply a plan to the same words it was computed from.
pub fn apply(words: &mut [u32], base: u64, report: &Report) {
    for site in &report.patched {
        let index = ((site.addr - base) / 4) as usize;
        words[index..index + site.words.len()].copy_from_slice(&site.words);
    }
}

/// The addresses every direct branch in the range targets.
pub fn direct_branch_targets(words: &[u32], base: u64) -> HashSet<u64> {
    let decoded: Vec<Insn> = words.iter().copied().map(decode).collect();
    direct_branches(&decoded, base)
        .into_iter()
        .map(|(_, target)| target)
        .collect()
}

/// Every direct branch in the range, as (index of the branch, target address).
fn direct_branches(decoded: &[Insn], base: u64) -> Vec<(usize, u64)> {
    let mut branches = Vec::new();
    for (index, insn) in decoded.iter().enumerate() {
        let here = base + 4 * index as u64;
        let offset = match *insn {
            Insn::Branch { offset, .. }
            | Insn::CondBranch { offset, .. }
            | Insn::CompareBranch { offset, .. }
            | Insn::CompareAndBranch { offset }
            | Insn::TestBranch { offset } => offset,
            _ => continue,
        };
        branches.push((index, here.wrapping_add(offset as u64)));
    }
    branches
}

/// Match the sequence ending at the store-exclusive `store`, returning the
/// index of its first instruction, the replacement words and the idiom.
#[allow(clippy::too_many_arguments)]
fn match_sequence(
    words: &[u32],
    decoded: &[Insn],
    store: usize,
    base: u64,
    _branches: &[(usize, u64)],
    _external_targets: &HashSet<u64>,
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
    // Register 31 is SP in the base position of both the exclusives and the
    // atomics. An atomic on the stack pointer is not something a compiler
    // emits, and it is not worth reasoning about.
    if rn == ZR {
        return Err(Refusal::Registers("the stack pointer is the base register"));
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
                offset: _,
            },
        ) if branch_rt == rt && branch_sf == sf => {
            return Err(Refusal::ConditionalCas);
        }
        (3, Insn::Cmp { .. }) if matches!(decoded[start + 2], Insn::CondBranch { cond: 1, .. }) => {
            return Err(Refusal::ConditionalCas);
        }
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
    // The atomic carries the operand in Rs and the loaded value in Rt. Arm
    // ARM DDI 0487 L.a, C6.2 (the LD<op> instructions, "Constrained
    // unpredictable behavior"): when Rs and Rt are the same register the
    // behaviour is CONSTRAINED UNPREDICTABLE.
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
        // A plain 12-bit immediate, or one the instruction shifted left by 12,
        // whenever a single MOVZ can build it.
        (AluOp::Add, Operand::Imm(k)) => (
            insn::movz_for(sf, rs, u64::from(k)).ok_or(Refusal::UnsupportedOperation)?,
            AtomicOp::Add,
        ),
        (AluOp::Sub, Operand::Reg(rm)) => (insn::neg(sf, rs, rm), AtomicOp::Add),
        // MOVN materialises -k as !(k - 1), exactly, for every k up to
        // 0x10000, the shifted immediates in that range included. A larger
        // one has no single-instruction negation, because MOVN at a
        // multiple-of-16 shift leaves the bits below the shift set.
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
    ];

    /// Sequences that must survive untouched, with the reason they do.
    const REFUSALS: &[(&str, &[u32], Refusal)] = &[
        (
            "pair",
            &[0xc87f8440, 0xc8238440, 0x35ffffc3],
            Refusal::ExclusivePair,
        ),
        (
            "cas_zero_x_al",
            &[0xc85ffc20, 0xb5000080, 0xc802fc23, 0x35ffffa2],
            Refusal::ConditionalCas,
        ),
        (
            "cas_compare_gcc_x_al",
            &[0xc85ffc66, 0xeb0400df, 0x54000081, 0xc807fc62, 0x35ffff87],
            Refusal::ConditionalCas,
        ),
        (
            "cas_compare_llvm_status_is_destination",
            &[0xc85ffd4c, 0xeb0b019f, 0x540000a1, 0xc80cfd48, 0x35ffff8c],
            Refusal::ConditionalCas,
        ),
        (
            "cas_compare_immediate_halfword",
            &[0x485ffc20, 0x71001c1f, 0x54000081, 0x4802fc23, 0x35ffff82],
            Refusal::ConditionalCas,
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
            "subword_compare_against_a_register_is_a_conditional_cas",
            &[0x085ffc20, 0x6b04001f, 0x54000081, 0x0802fc23, 0x35ffff82],
            Refusal::ConditionalCas,
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

    /// The read-only mismatch oracle: the conditional-CAS loops are
    /// refused, so a guest whose comparison misses on a read-only word
    /// keeps its native no-store exit instead of faulting at a CAS that
    /// demands write permission before comparing. The unconditional
    /// loops, whose native form always attempts the store, still rewrite.
    #[test]
    fn a_conditional_cas_is_refused_so_a_read_only_mismatch_cannot_fault() {
        let conditional_cas: [&[u32]; 2] = [
            &[0xc85ffc20, 0xb5000080, 0xc802fc23, 0x35ffffa2], // cas-zero
            &[0xc85ffc66, 0xeb0400df, 0x54000081, 0xc807fc62, 0x35ffff87], // cas-compare
        ];
        for words in conditional_cas {
            let report = plan(words, BASE);
            assert_eq!(report.patched, vec![]);
            assert_eq!(
                report.refused.first().map(|r| r.reason.clone()),
                Some(Refusal::ConditionalCas)
            );
        }
        let (_, _, rmw, _) = CASES[0];
        assert_eq!(plan(rmw, BASE).patched.len(), 1);
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
    fn a_bc_cond_into_the_middle_of_a_sequence_refuses_it() {
        // clrex; bc.ne .+8; ldaxr x4,[x3]; add x4,x4,#1; stlxr w5,x4,[x3];
        // cbnz w5, .-12; ret. The BC.ne lands on the ADD inside the
        // canonical loop, so the loop must be refused, exactly as the
        // same entry by B.ne is.
        let words = [
            0xd503_3f5f, // clrex
            0x5400_0051, // bc.ne .+8
            0xc85f_fc64, // ldaxr x4, [x3]
            0x9100_0484, // add x4, x4, #1
            0xc805_fc64, // stlxr w5, x4, [x3]
            0x35ff_ffa5, // cbnz w5, .-12
            0xd65f_03c0, // ret
        ];
        let report = plan(&words, BASE);
        assert_eq!(report.patched, vec![]);
        assert_eq!(
            report.refused.first().map(|r| r.reason.clone()),
            Some(Refusal::BranchIntoSequence)
        );

        // Control: without the interior entry the same loop is rewritten.
        let mut control = words;
        control[1] = insn::NOP;
        let report = plan(&control, BASE);
        assert_eq!(report.patched.len(), 1);
    }

    #[test]
    fn a_cmpbr_into_the_middle_of_a_sequence_refuses_it() {
        // clrex; CB<cc> .+8 (register, immediate, byte and halfword
        // forms); then the canonical loop. Each branch lands on the ADD
        // inside the loop, so the loop is refused, and without the
        // branch the same loop is rewritten.
        let (_, _, loop_words, _) = CASES[0];
        for branch in [0xf4c6_0046, 0xf5c0_0046, 0x74c6_8046, 0x74c6_c046] {
            let mut words = vec![insn::CLREX, branch];
            words.extend_from_slice(loop_words);
            let report = plan(&words, BASE);
            assert_eq!(report.patched, vec![], "{branch:#x}");
            assert_eq!(
                report.refused.first().map(|r| r.reason.clone()),
                Some(Refusal::BranchIntoSequence),
                "{branch:#x}"
            );
        }
        let mut words = vec![insn::CLREX, insn::NOP];
        words.extend_from_slice(loop_words);
        assert_eq!(plan(&words, BASE).patched.len(), 1);
    }

    #[test]
    fn an_unrecognised_possible_branch_refuses_the_whole_range() {
        // A reserved (permanently unallocated) word and an unallocated
        // branch-group word are each treated as a direct branch with an
        // unknown target: no site in the range rewrites.
        let (_, _, loop_words, _) = CASES[0];
        for unknown in [0x0000_0000, 0xd700_0000] {
            let mut words = vec![unknown];
            words.extend_from_slice(loop_words);
            let report = plan(&words, BASE);
            assert_eq!(report.patched, vec![], "{unknown:#x}");
            assert_eq!(
                report.refused.first().map(|r| r.reason.clone()),
                Some(Refusal::UnknownDirectBranch),
                "{unknown:#x}"
            );
        }
    }

    #[test]
    fn a_loop_inside_a_larger_region_is_found_at_its_own_address() {
        let (_, _, original, expected) = CASES[0];
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
    fn an_immediate_the_instruction_shifted_by_twelve_is_materialised() {
        // ldxr x9,[x8]; add x9,x9,#0x100,lsl #12; stxr w10,x9,[x8]; cbnz w10
        // becomes movz x10,#0x10,lsl #16; ldadd x10,x9,[x8]; <the add>; mov w10,wzr.
        let words = [0xc85f_7d09, 0x9144_0129, 0xc80a_7d09, 0x35ff_ffaa];
        let report = plan(&words, BASE);
        assert_eq!(report.refused, vec![]);
        assert_eq!(
            report.patched[0].words,
            vec![0xd2a0_020a, 0xf82a_0109, 0x9144_0129, 0x2a1f_03ea]
        );

        // The same subtraction has no one-instruction negation: MOVN yields one
        // less than a multiple of 4096.
        let words = [0xc85f_7d09, 0xd160_0129, 0xc80a_7d09, 0x35ff_ffaa];
        let report = plan(&words, BASE);
        assert_eq!(report.patched, vec![]);
        assert_eq!(
            report.refused.first().map(|r| r.reason.clone()),
            Some(Refusal::UnsupportedOperation)
        );
    }

    #[test]
    fn a_sub_word_register_compare_is_a_conditional_cas_and_stays_refused() {
        // uxtb w16,w0; ldaxrb w0,[x2]; cmp w0,w16; b.ne out; stlxrb w17,w1,[x2]; cbnz w17
        // is the compiler's own byte compare-and-swap helper. It used to be
        // rewritten when a zero extension proved the compare's high bits
        // clear; as a conditional CAS it now stays native like the others,
        // with or without that proof.
        let loop_words = [
            0x085f_fc40,
            0x6b10_001f,
            0x5400_0081,
            0x0811_fc41,
            0x35ff_ff91,
        ];
        for words in [loop_words.to_vec(), {
            let mut w = vec![0x5300_1c10];
            w.extend_from_slice(&loop_words);
            w
        }] {
            let report = plan(&words, BASE);
            assert_eq!(report.patched, vec![], "{report:?}");
            assert_eq!(
                report.refused.first().map(|r| r.reason.clone()),
                Some(Refusal::ConditionalCas)
            );
        }
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
