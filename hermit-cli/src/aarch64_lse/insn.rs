/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The subset of A64 this rewriter decodes, and the instructions it emits.
//!
//! Decoding is deliberately partial and total: every 32-bit word maps to a
//! variant, and everything outside the subset becomes [`Insn::Other`], which no
//! idiom accepts. A word is only ever rewritten when every instruction of its
//! sequence decoded into a known variant, so an unrecognised encoding can never
//! be rewritten by accident.
//!
//! Field positions follow the Arm Architecture Reference Manual for A-profile
//! (DDI 0487, issue L.a): C6.2 for the instruction encodings, C3.2.6 "Load-
//! Exclusive and Store-Exclusive" for the exclusive pair, and C3.2.12 "Atomic
//! instructions" for the FEAT_LSE operations this rewriter emits.

/// A general-purpose register number; 31 is XZR/WZR in these encodings, and SP
/// only in the base-register position, which the rewriter refuses.
pub type Reg = u8;

pub const ZR: Reg = 31;

/// Access size of an exclusive or atomic access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    B = 0,
    H = 1,
    W = 2,
    X = 3,
}

impl Size {
    fn from_bits(bits: u32) -> Size {
        match bits {
            0 => Size::B,
            1 => Size::H,
            2 => Size::W,
            _ => Size::X,
        }
    }

    /// Whether the value registers of this access are 64-bit.
    pub fn is_64(self) -> bool {
        matches!(self, Size::X)
    }
}

/// Memory ordering carried by one half of an exclusive pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ordering {
    pub acquire: bool,
    pub release: bool,
}

/// The data-processing operations an RMW idiom may use between its pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AluOp {
    Add,
    Sub,
    And,
    Orr,
    Eor,
    Bic,
}

/// The second operand of a decoded data-processing instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand {
    Reg(Reg),
    Imm(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Insn {
    /// LDXR/LDAXR and their byte and halfword forms.
    LoadExcl {
        size: Size,
        ord: Ordering,
        rt: Reg,
        rn: Reg,
    },
    /// STXR/STLXR and their byte and halfword forms.
    StoreExcl {
        size: Size,
        ord: Ordering,
        rs: Reg,
        rt: Reg,
        rn: Reg,
    },
    /// LDXP/LDAXP/STXP/STLXP: recognised so they are reported, never rewritten.
    ExclPair,
    /// ADD/SUB/AND/ORR/EOR/BIC, register form with no shift, or immediate form
    /// with no shift. `rd` is never the zero register here: a flag-setting or
    /// discarding form decodes as [`Insn::Cmp`] or [`Insn::Other`].
    Alu {
        op: AluOp,
        sf: bool,
        rd: Reg,
        rn: Reg,
        operand: Operand,
    },
    /// UXTB/UXTH: a zero extension, which proves the bits above its width
    /// are clear in the destination.
    ZeroExtend {
        bits: u32,
        rd: Reg,
        rn: Reg,
    },
    /// SUBS with a discarded result: CMP, register or immediate.
    Cmp {
        sf: bool,
        rn: Reg,
        operand: Operand,
    },
    /// CBZ/CBNZ.
    CompareBranch {
        nonzero: bool,
        sf: bool,
        rt: Reg,
        /// Byte offset from this instruction.
        offset: i64,
    },
    /// B.cond.
    CondBranch {
        cond: u32,
        offset: i64,
    },
    /// B and BL.
    Branch {
        link: bool,
        offset: i64,
    },
    /// TBZ/TBNZ.
    TestBranch {
        offset: i64,
    },
    Clrex,
    Nop,
    /// Anything outside the decoded subset, including every indirect branch.
    Other,
}

fn sign_extend(value: u32, bits: u32) -> i64 {
    let shift = 64 - bits;
    ((value as i64) << shift) >> shift
}

pub fn decode(word: u32) -> Insn {
    // C3.2.6: exclusives share bits 29:24 = 001000 with the FEAT_LSE compare-
    // and-swap instructions, which set bit 23; those are left as Other.
    if (word >> 24) & 0x3f == 0b001000 && (word >> 23) & 1 == 0 {
        let size = Size::from_bits(word >> 30);
        let load = (word >> 22) & 1 == 1;
        let pair = (word >> 21) & 1 == 1;
        let rs = ((word >> 16) & 0x1f) as Reg;
        let o0 = (word >> 15) & 1 == 1;
        let rt2 = ((word >> 10) & 0x1f) as Reg;
        let rn = ((word >> 5) & 0x1f) as Reg;
        let rt = (word & 0x1f) as Reg;
        if pair {
            return Insn::ExclPair;
        }
        if rt2 != ZR {
            return Insn::Other;
        }
        return if load {
            // A load-exclusive leaves Rs unused and encoded as 0b11111.
            if rs != ZR {
                Insn::Other
            } else {
                Insn::LoadExcl {
                    size,
                    ord: Ordering {
                        acquire: o0,
                        release: false,
                    },
                    rt,
                    rn,
                }
            }
        } else {
            Insn::StoreExcl {
                size,
                ord: Ordering {
                    acquire: false,
                    release: o0,
                },
                rs,
                rt,
                rn,
            }
        };
    }
    if word == NOP {
        return Insn::Nop;
    }
    // CLREX takes a CRm barrier operand; only the architectural default is
    // recognised, and any other operand stays Other.
    if word == CLREX {
        return Insn::Clrex;
    }
    // Add/subtract (immediate), C6.2: sf op S 100010 sh imm12 Rn Rd.
    if (word >> 23) & 0x3f == 0b100010 {
        let sf = word >> 31 == 1;
        let sub = (word >> 30) & 1 == 1;
        let set_flags = (word >> 29) & 1 == 1;
        let shift12 = (word >> 22) & 1 == 1;
        let imm = (word >> 10) & 0xfff;
        let rn = ((word >> 5) & 0x1f) as Reg;
        let rd = (word & 0x1f) as Reg;
        // An add or subtract may shift its immediate left by 12; the decoded
        // operand is the shifted value, which is what the rewrite must
        // materialise.
        let imm = if shift12 { imm << 12 } else { imm };
        return match (set_flags, sub, rd) {
            (true, true, ZR) if !shift12 => Insn::Cmp {
                sf,
                rn,
                operand: Operand::Imm(imm),
            },
            (false, _, ZR) => Insn::Other,
            (false, _, _) => Insn::Alu {
                op: if sub { AluOp::Sub } else { AluOp::Add },
                sf,
                rd,
                rn,
                operand: Operand::Imm(imm),
            },
            _ => Insn::Other,
        };
    }
    // Add/subtract (shifted register): sf op S 01011 shift(2) 0 Rm imm6 Rn Rd.
    if (word >> 24) & 0x1f == 0b01011 && (word >> 21) & 1 == 0 {
        let sf = word >> 31 == 1;
        let sub = (word >> 30) & 1 == 1;
        let set_flags = (word >> 29) & 1 == 1;
        let shift = (word >> 22) & 3;
        let rm = ((word >> 16) & 0x1f) as Reg;
        let imm6 = (word >> 10) & 0x3f;
        let rn = ((word >> 5) & 0x1f) as Reg;
        let rd = (word & 0x1f) as Reg;
        if shift != 0 || imm6 != 0 {
            return Insn::Other;
        }
        return match (set_flags, sub, rd) {
            (true, true, ZR) => Insn::Cmp {
                sf,
                rn,
                operand: Operand::Reg(rm),
            },
            (false, _, ZR) => Insn::Other,
            (false, _, _) => Insn::Alu {
                op: if sub { AluOp::Sub } else { AluOp::Add },
                sf,
                rd,
                rn,
                operand: Operand::Reg(rm),
            },
            _ => Insn::Other,
        };
    }
    // Logical (shifted register): sf opc(2) 01010 shift(2) N Rm imm6 Rn Rd.
    if (word >> 24) & 0x1f == 0b01010 {
        let sf = word >> 31 == 1;
        let opc = (word >> 29) & 3;
        let shift = (word >> 22) & 3;
        let negate = (word >> 21) & 1 == 1;
        let rm = ((word >> 16) & 0x1f) as Reg;
        let imm6 = (word >> 10) & 0x3f;
        let rn = ((word >> 5) & 0x1f) as Reg;
        let rd = (word & 0x1f) as Reg;
        if shift != 0 || imm6 != 0 || rd == ZR {
            return Insn::Other;
        }
        let op = match (opc, negate) {
            (0b00, false) => AluOp::And,
            (0b00, true) => AluOp::Bic,
            (0b01, false) => AluOp::Orr,
            (0b10, false) => AluOp::Eor,
            _ => return Insn::Other,
        };
        return Insn::Alu {
            op,
            sf,
            rd,
            rn,
            operand: Operand::Reg(rm),
        };
    }
    // Unsigned bitfield move (32-bit, immr = 0): UXTB is UBFM Wd, Wn, #0, #7
    // and UXTH is UBFM Wd, Wn, #0, #15.
    // sf=0, opc=10 (UBFM is opc=10 for the unsigned form), 100110, N=0.
    if (word >> 22) & 0x3ff == 0b01_0100_1100 {
        let imms = (word >> 10) & 0x3f;
        let immr = (word >> 16) & 0x3f;
        let rn = ((word >> 5) & 0x1f) as Reg;
        let rd = (word & 0x1f) as Reg;
        if immr == 0 && (imms == 7 || imms == 15) {
            return Insn::ZeroExtend {
                bits: imms + 1,
                rd,
                rn,
            };
        }
        return Insn::Other;
    }
    // Compare and branch (immediate): sf 011010 op imm19 Rt.
    if (word >> 25) & 0x3f == 0b011010 {
        return Insn::CompareBranch {
            nonzero: (word >> 24) & 1 == 1,
            sf: word >> 31 == 1,
            rt: (word & 0x1f) as Reg,
            offset: sign_extend((word >> 5) & 0x7ffff, 19) * 4,
        };
    }
    // Test and branch (immediate): b5 011011 op b40 imm14 Rt.
    if (word >> 25) & 0x3f == 0b011011 {
        return Insn::TestBranch {
            offset: sign_extend((word >> 5) & 0x3fff, 14) * 4,
        };
    }
    // Conditional branch (immediate): 0101010 o1 imm19 o0 cond.
    if (word >> 24) & 0xff == 0b0101_0100 && (word >> 4) & 1 == 0 {
        return Insn::CondBranch {
            cond: word & 0xf,
            offset: sign_extend((word >> 5) & 0x7ffff, 19) * 4,
        };
    }
    // Unconditional branch (immediate): op 00101 imm26.
    if (word >> 26) & 0x1f == 0b00101 {
        return Insn::Branch {
            link: word >> 31 == 1,
            offset: sign_extend(word & 0x03ff_ffff, 26) * 4,
        };
    }
    Insn::Other
}

pub const NOP: u32 = 0xd503_201f;
pub const CLREX: u32 = 0xd503_3f5f;

fn sf_bit(sf: bool) -> u32 {
    if sf { 1 << 31 } else { 0 }
}

/// `MOV Rd, Rm` (ORR Rd, ZR, Rm).
pub fn mov_reg(sf: bool, rd: Reg, rm: Reg) -> u32 {
    sf_bit(sf) | 0x2a00_03e0 | ((rm as u32) << 16) | rd as u32
}

/// `MOV Rd, #imm16` (MOVZ, no shift).
pub fn movz(sf: bool, rd: Reg, imm16: u32) -> u32 {
    movz_shifted(sf, rd, imm16, 0)
}

/// `MOVZ Rd, #imm16, LSL #(16 * hw)`.
pub fn movz_shifted(sf: bool, rd: Reg, imm16: u32, hw: u32) -> u32 {
    sf_bit(sf) | 0x5280_0000 | (hw << 21) | (imm16 << 5) | rd as u32
}

/// The single MOVZ that materialises `value`, if one exists: the immediate is
/// 16 bits at a multiple-of-16 shift, so only a value whose nonzero bits fall
/// inside one such window can be built in one instruction.
pub fn movz_for(sf: bool, rd: Reg, value: u64) -> Option<u32> {
    for hw in 0..if sf { 4 } else { 2 } {
        let shift = 16 * hw;
        if value >> shift << shift == value && (value >> shift) <= 0xffff {
            return Some(movz_shifted(sf, rd, (value >> shift) as u32, hw));
        }
    }
    None
}

/// `MOVN Rd, #imm16`: Rd = !imm16, so `movn(imm - 1)` materialises `-imm`.
pub fn movn(sf: bool, rd: Reg, imm16: u32) -> u32 {
    sf_bit(sf) | 0x1280_0000 | (imm16 << 5) | rd as u32
}

/// `NEG Rd, Rm` (SUB Rd, ZR, Rm).
pub fn neg(sf: bool, rd: Reg, rm: Reg) -> u32 {
    sf_bit(sf) | 0x4b00_03e0 | ((rm as u32) << 16) | rd as u32
}

/// `MVN Rd, Rm` (ORN Rd, ZR, Rm).
pub fn mvn(sf: bool, rd: Reg, rm: Reg) -> u32 {
    sf_bit(sf) | 0x2a20_03e0 | ((rm as u32) << 16) | rd as u32
}

/// The FEAT_LSE atomic memory operations this rewriter emits (C3.2.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomicOp {
    Add = 0,
    Clr = 1,
    Eor = 2,
    Set = 3,
    Swp = 8,
}

/// `LD<op>{A}{L}<size> Rs, Rt, [Rn]`, or `SWP{A}{L}<size>`.
pub fn atomic(op: AtomicOp, size: Size, ord: Ordering, rs: Reg, rt: Reg, rn: Reg) -> u32 {
    let (o3, opc) = if matches!(op, AtomicOp::Swp) {
        (1, 0)
    } else {
        (0, op as u32)
    };
    ((size as u32) << 30)
        | (0b111000 << 24)
        | ((ord.acquire as u32) << 23)
        | ((ord.release as u32) << 22)
        | (1 << 21)
        | ((rs as u32) << 16)
        | (o3 << 15)
        | (opc << 12)
        | ((rn as u32) << 5)
        | rt as u32
}

/// `B.<cond> .+offset`, re-encoded at a new address.
pub fn cond_branch(cond: u32, offset: i64) -> Option<u32> {
    imm19(offset).map(|imm| 0x5400_0000 | (imm << 5) | cond)
}

/// `CBZ`/`CBNZ Rt, .+offset`, re-encoded at a new address.
pub fn compare_branch(nonzero: bool, sf: bool, rt: Reg, offset: i64) -> Option<u32> {
    imm19(offset)
        .map(|imm| sf_bit(sf) | 0x3400_0000 | ((nonzero as u32) << 24) | (imm << 5) | rt as u32)
}

/// A 19-bit, word-aligned PC-relative displacement, or `None` when the
/// displacement does not fit the encoding.
fn imm19(offset: i64) -> Option<u32> {
    if offset % 4 != 0 || !(-(1 << 20)..(1 << 20)).contains(&offset) {
        return None;
    }
    Some(((offset / 4) as u32) & 0x7ffff)
}

/// `CAS{A}{L}<size> Rs, Rt, [Rn]`: Rs carries the expected value in and the
/// loaded value out, Rt is the value stored on a match.
pub fn cas(size: Size, ord: Ordering, rs: Reg, rt: Reg, rn: Reg) -> u32 {
    ((size as u32) << 30)
        | (0b001000 << 24)
        | (1 << 23)
        | ((ord.acquire as u32) << 22)
        | (1 << 21)
        | ((rs as u32) << 16)
        | ((ord.release as u32) << 15)
        | (0b11111 << 10)
        | ((rn as u32) << 5)
        | rt as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_exclusive_pair_of_a_gcc_rmw_loop() {
        // ldaxr x4, [x3] / add x4, x4, #1 / stlxr w5, x4, [x3] / cbnz w5, -12
        assert_eq!(
            decode(0xc85f_fc64),
            Insn::LoadExcl {
                size: Size::X,
                ord: Ordering {
                    acquire: true,
                    release: false
                },
                rt: 4,
                rn: 3
            }
        );
        assert_eq!(
            decode(0x9100_0484),
            Insn::Alu {
                op: AluOp::Add,
                sf: true,
                rd: 4,
                rn: 4,
                operand: Operand::Imm(1)
            }
        );
        assert_eq!(
            decode(0xc805_fc64),
            Insn::StoreExcl {
                size: Size::X,
                ord: Ordering {
                    acquire: false,
                    release: true
                },
                rs: 5,
                rt: 4,
                rn: 3
            }
        );
        assert_eq!(
            decode(0x35ff_ffa5),
            Insn::CompareBranch {
                nonzero: true,
                sf: false,
                rt: 5,
                offset: -12
            }
        );
    }

    #[test]
    fn a_compare_and_swap_is_not_decoded_as_an_exclusive() {
        // casa w1, w2, [x0] shares the exclusive major group but sets bit 23.
        assert_eq!(decode(0x88e1_7c02), Insn::Other);
    }

    #[test]
    fn pairs_are_recognised_and_never_treated_as_single_exclusives() {
        // ldaxp x0, x1, [x2] / stlxp w3, x4, x5, [x2]
        assert_eq!(decode(0xc87f_8440), Insn::ExclPair);
        assert_eq!(decode(0xc823_9444), Insn::ExclPair);
    }

    #[test]
    fn branch_offsets_are_signed_byte_offsets() {
        assert_eq!(
            decode(0x1400_0002),
            Insn::Branch {
                link: false,
                offset: 8
            }
        );
        assert_eq!(
            decode(0x17ff_fffe),
            Insn::Branch {
                link: false,
                offset: -8
            }
        );
        assert_eq!(
            decode(0x5400_0061),
            Insn::CondBranch {
                cond: 1,
                offset: 12
            }
        );
    }

    #[test]
    fn unknown_encodings_decode_as_other() {
        for word in [0xd65f_03c0, 0xf900_0000, 0xd61f_0000, 0x9ac0_0800] {
            assert_eq!(decode(word), Insn::Other, "word {word:#010x}");
        }
    }
}
