/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! An example that tracks thread pedigree using local state.

use std::io;
use std::mem;

use bitvec::bitvec;
use bitvec::order::Msb0;
use bitvec::vec::BitVec;
use libc::pid_t;
use nix::unistd::Pid;
use serde::Deserialize;
use serde::Serialize;

/// Helper function that finds the longest run of repeating bits in a bitvec
fn longest_run(sequence: &BitVec) -> (usize, usize) {
    let mut prev_bit = false;
    let mut prev_count = 1;

    let mut max_count = 0;
    let mut max_start = 0;

    for (index, bit) in sequence.iter().enumerate() {
        let count = if index > 0 && prev_bit == *bit {
            prev_count + 1
        } else {
            1
        };
        if count > max_count {
            max_count = count;
            max_start = index + 1 - count;
        }

        prev_count = count;
        prev_bit = *bit;
    }

    (max_start, max_count)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
/// Unbounded bitstring representation of process pedigree (i.e. tree path or tree index)
/// which can be forked and converted to a deterministic virtual PID.
///
/// As a binary tree-index, a Pedigree can be viewed as a series of "left"/"right"
/// directions for how to navigate the tree.  Therefore a zero-length Pedigree refers to
/// the root.  For convenience, we refer to "parent/child" rather than "left/right",
/// following the normal conventions of process or thread forking.  Note that this
/// pedigree datatype does not represent "joins" within a set of running processes, nor
/// does it otherwise represent dependencies or "happens before" edges.
///
/// TODO: Add serialization / deserialization
pub struct Pedigree {
    pedigree: BitVec,
}

impl Pedigree {
    /// Create a new root pedigree representing the top of a tree of processes or threads.
    pub fn new() -> Self {
        Pedigree {
            pedigree: bitvec![0],
        }
    }

    /// Split a pedigree into a pedigree for the two execution points
    /// after the fork: `(parent,child)`.  I.e. both tree-paths
    /// returned are one level deeper from the root than the input was.
    pub fn fork(&self) -> (Self, Self) {
        let mut parent = self.clone();
        let child = parent.fork_mut();
        (parent, child)
    }

    /// Fork a pedigree, destructively.
    /// Mutates parent pedigree, returns new child pedigree.
    ///
    /// Since parent pedigree is being copied to the child, this function will
    /// have O(n) complexity with respect to pedigree length.
    pub fn fork_mut(&mut self) -> Self {
        let mut child_pedigree = self.pedigree.clone();
        child_pedigree.push(true);
        self.pedigree.push(false);
        Pedigree {
            pedigree: child_pedigree,
        }
    }

    /// Get pedigree's inner BitVec representation
    pub fn raw(&self) -> BitVec {
        self.pedigree.clone()
    }
}

/// Attempts to convert the pedigree bitstring into a deterministic virtual PID
impl TryFrom<&Pedigree> for Pid {
    type Error = io::Error;
    fn try_from(pedigree: &Pedigree) -> Result<Self, Self::Error> {
        // Define mpping of pedigree bits -> PID bits
        const MSB_ZERO_BITS: usize = 1;
        const TREE_BITS: usize = 16;
        const RUN_INDEX_BITS: usize = 4;
        const RUN_TYPE_BITS: usize = 1;
        const RUN_LENGTH_BITS: usize = 10;
        debug_assert!(
            MSB_ZERO_BITS + TREE_BITS + RUN_INDEX_BITS + RUN_TYPE_BITS + RUN_LENGTH_BITS
                == mem::size_of::<pid_t>() * 8
        );

        // Trim off any trailing P's from pedigree, i.e. viewing it as
        // a sequence of 'P' (parent) and 'C' (child) directions.
        let mut sequence = pedigree.raw();
        while sequence.len() > 1 && sequence.last() == Some(&false) {
            sequence.pop();
        }

        // Find longest run in pedigree sequence
        let (index, len) = longest_run(&sequence);

        // Make sure pedigree will fit into the bit encoding
        if index >= 2_usize.pow(RUN_INDEX_BITS as u32)
            || len >= 2_usize.pow(RUN_LENGTH_BITS as u32)
            || sequence.len() - len > TREE_BITS
        {
            Err(Self::Error::new(
                io::ErrorKind::Other,
                "Pedigree is too large or complex to be deterministically converted into virtual PID.",
            ))
        } else {
            // Extract the longest run of bits from pedigree
            let mut lower_tree = sequence.split_off(index + len);
            let run = sequence.split_off(index);
            let mut tree = sequence;
            tree.append(&mut lower_tree);

            // Construct a BitVec which will be interpreted as a pid_t
            let mut vpid_bits: BitVec<Msb0, u32> =
                BitVec::with_capacity(mem::size_of::<pid_t>() * 8);

            // pid_t is signed, so MSB must always be zero or it will be interpreted as error
            // when returned from fork, clone, etc.
            vpid_bits.push(false);

            // Pack the rest of the bits, using asserts to make sure the bitfield sizing
            // is correct. Any errors here are fatal bugs, so assert seems acceptable.

            let mut tree_bits: BitVec<Msb0, u32> = BitVec::repeat(false, TREE_BITS - tree.len());
            tree_bits.append(&mut tree);
            debug_assert!(tree_bits.len() == TREE_BITS);
            vpid_bits.append(&mut tree_bits);

            let mut run_index_bits = BitVec::<Msb0, u32>::from_element(index as u32);
            run_index_bits = run_index_bits.split_off(run_index_bits.len() - RUN_INDEX_BITS);
            debug_assert!(run_index_bits.len() == RUN_INDEX_BITS);
            vpid_bits.append(&mut run_index_bits);

            let mut run_type_bits: BitVec<Msb0, u32> = BitVec::new();
            run_type_bits.push(run[0]);
            debug_assert!(run_type_bits.len() == RUN_TYPE_BITS);
            vpid_bits.append(&mut run_type_bits);

            let mut run_length_bits = BitVec::<Msb0, u32>::from_element(len as u32);
            run_length_bits = run_length_bits.split_off(run_length_bits.len() - RUN_LENGTH_BITS);
            debug_assert!(run_length_bits.len() == RUN_LENGTH_BITS);
            vpid_bits.append(&mut run_length_bits);

            debug_assert!(vpid_bits.len() == mem::size_of::<pid_t>() * 8);

            Ok(Pid::from_raw(vpid_bits.into_vec()[0] as i32))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_longest_run() {
        let sequence = bitvec![1, 1, 0, 0, 0];
        let (index, len) = longest_run(&sequence);
        assert_eq!((index, len), (2, 3));

        let sequence = bitvec![1, 1, 1, 0, 0];
        let (index, len) = longest_run(&sequence);
        assert_eq!((index, len), (0, 3));

        let sequence = bitvec![1, 1, 0, 0, 1, 1];
        let (index, len) = longest_run(&sequence);
        assert_eq!((index, len), (0, 2));

        let sequence = bitvec![1, 0, 0, 0, 0, 1];
        let (index, len) = longest_run(&sequence);
        assert_eq!((index, len), (1, 4));

        let sequence = bitvec![1, 0, 1, 0, 1, 0];
        let (index, len) = longest_run(&sequence);
        assert_eq!((index, len), (0, 1));
    }

    #[test]
    fn test_pedigree_basic() {
        // FIXME: These tests are dependent on the bit widths used to convert
        // pedigree into PID, but the tests below assume that these values
        // do not change.

        let mut parent = Pedigree::new();
        // Root pedigree = P
        assert_eq!(Pid::try_from(&parent).unwrap(), Pid::from_raw(0x1));

        let child = parent.fork_mut();
        // Parent pedigree = PP
        assert_eq!(Pid::try_from(&parent).unwrap(), Pid::from_raw(0x1));
        // Child pedigree == PC
        assert_eq!(Pid::try_from(&child).unwrap(), Pid::from_raw(0x00008001));

        let child2 = parent.fork_mut();
        // Parent pedigree == PPP
        assert_eq!(Pid::try_from(&parent).unwrap(), Pid::from_raw(0x1));
        // Child pedigree == PPC
        assert_eq!(Pid::try_from(&child2).unwrap(), Pid::from_raw(0x00008002));
    }

    #[test]
    fn test_pedigree_many_forks() {
        let mut many_forks_bitstring = BitVec::repeat(false, 1023);
        many_forks_bitstring.push(true);
        let many_forks_pedigree = Pedigree {
            pedigree: many_forks_bitstring,
        };
        assert_eq!(
            Pid::try_from(&many_forks_pedigree).unwrap(),
            Pid::from_raw(0x000083FF)
        );
    }

    #[test]
    fn test_pedigree_overflow() {
        let mut many_forks_bitstring = BitVec::repeat(false, 1024);
        many_forks_bitstring.push(true);
        let many_forks_pedigree = Pedigree {
            pedigree: many_forks_bitstring,
        };
        assert!(Pid::try_from(&many_forks_pedigree).is_err());
    }
}
