/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::cmp::max;
use std::cmp::min;

type IndexAndAlignments = (usize, Vec<(Option<usize>, Option<usize>)>);

#[derive(PartialEq, Eq, Clone, Debug)]
pub enum Trace {
    Stop,
    Left,
    Up,
    MatchDiagonal,
    MisMatchDiagonal,
}

fn simple_score<T: PartialEq>(a: T, b: T) -> i32 {
    if a == b { 1 } else { -5 }
}

pub struct NeedlemanWunsch<T: PartialEq + Clone> {
    pub first_sequence: Vec<T>,
    pub second_sequence: Vec<T>,
    pub first_extra: Vec<T>,
    pub second_extra: Vec<T>,
    pub mismatches: Vec<(usize, usize)>,
    pub num_mismatches: usize,
}

impl<T: PartialEq + Clone> Default for NeedlemanWunsch<T> {
    fn default() -> NeedlemanWunsch<T> {
        NeedlemanWunsch {
            first_sequence: vec![],
            second_sequence: vec![],
            first_extra: vec![],
            second_extra: vec![],
            mismatches: vec![],
            num_mismatches: 0,
        }
    }
}

impl<T: PartialEq + Clone> NeedlemanWunsch<T> {
    pub fn match_sequences_base(&mut self) -> IndexAndAlignments {
        self.match_sequences(None, None, None)
    }

    /// Take the first alignment in NeedlemanWunsch struct and changes
    /// it using the mismatches to get closer to the second alignment
    pub fn generate_midpoint_schedule(
        &mut self,
        start_index: usize,
        alignment_difference: Vec<(Option<usize>, Option<usize>)>,
    ) -> Vec<T> {
        let mut midpoint_schedule: Vec<T> = vec![];
        let total_swaps = self.num_mismatches / 2;
        let available_full_swaps = self.mismatches.len();
        let mut current_swaps = 0;
        let mut full_swaps = 0;
        {
            let mut i = 0;
            while i < start_index {
                midpoint_schedule.push(self.first_sequence[i].clone());
                i += 1;
            }
        }
        {
            let mut i = 0;
            while i < alignment_difference.len() {
                if alignment_difference[i].0.is_none() && alignment_difference[i].1.is_none() {
                    if full_swaps < available_full_swaps && current_swaps < total_swaps {
                        midpoint_schedule
                            .push(self.first_sequence[self.mismatches[full_swaps].0].clone());
                    } else if full_swaps < available_full_swaps {
                        midpoint_schedule
                            .push(self.second_sequence[self.mismatches[full_swaps].1].clone());
                    }
                    current_swaps += 1;
                    full_swaps += 1;
                } else if alignment_difference[i].0.is_some() && current_swaps < total_swaps {
                    midpoint_schedule
                        .push(self.first_sequence[alignment_difference[i].0.unwrap()].clone());
                    current_swaps += 1;
                } else if alignment_difference[i].1.is_some() && current_swaps >= total_swaps {
                    midpoint_schedule
                        .push(self.second_sequence[alignment_difference[i].1.unwrap()].clone());
                    current_swaps += 1;
                }
                i += 1;
            }
        }
        midpoint_schedule
    }

    /// Globally aligns the two sequences in NeedlemanWunsch struct.
    /// Returns: (1) Index till which both sequences are complete same
    ///          (2) Vector of (Matching index from sequence 1,
    ///                         Matching index from sequence 2)
    /// None, None represents no match - these are stored mismatch vector
    pub fn match_sequences(
        &mut self,
        scoring_function: Option<fn(T, T) -> i32>,
        gap_penalty: Option<i32>,
        mismatch_penalty: Option<i32>,
    ) -> IndexAndAlignments {
        let gap_penalty: i32 = gap_penalty.unwrap_or(-1);
        let mismatch_penalty: i32 = mismatch_penalty.unwrap_or(-2);
        let scoring_function = scoring_function.unwrap_or(simple_score);
        let mut start_index = 0;
        let max_length: usize = min(self.first_sequence.len(), self.second_sequence.len());
        while start_index < max_length
            && scoring_function(
                self.first_sequence[start_index].clone(),
                self.second_sequence[start_index].clone(),
            ) > 0
        {
            start_index += 1;
        }
        let row = self.first_sequence.len() + 1 - start_index;
        let col = self.second_sequence.len() + 1 - start_index;
        let mut matrix: Vec<Vec<i32>> = vec![vec![0; col]; row];
        {
            let mut i = 0;
            while i < row {
                matrix[i][0] = mismatch_penalty * (i as i32);
                i += 1;
            }
        }

        for j in 1..col {
            matrix[0][j] = mismatch_penalty * (j as i32);
        }
        let mut tracing_matrix: Vec<Vec<Trace>> = vec![vec![Trace::Stop; col]; row];

        let mut max_score: i32 = -1;
        let mut max_index = (0, 0);
        for i in 1..row {
            for j in 1..col {
                let match_value: i32 = scoring_function(
                    self.first_sequence[i - 1 + start_index].clone(),
                    self.second_sequence[j - 1 + start_index].clone(),
                );

                if match_value > 0 {
                    let diagonal_score: i32 = matrix[i - 1][j - 1] + match_value;
                    matrix[i][j] = diagonal_score;
                    tracing_matrix[i][j] = Trace::MatchDiagonal;
                } else {
                    let vertical_score: i32 = matrix[i - 1][j] + gap_penalty;
                    let horizontal_score: i32 = matrix[i][j - 1] + gap_penalty;
                    let diagonal_score: i32 = matrix[i - 1][j - 1] + gap_penalty;
                    matrix[i][j] = max(horizontal_score, max(vertical_score, diagonal_score));
                    if matrix[i][j] == diagonal_score {
                        tracing_matrix[i][j] = Trace::MisMatchDiagonal;
                    } else if matrix[i][j] == horizontal_score {
                        tracing_matrix[i][j] = Trace::Left;
                    } else if matrix[i][j] == vertical_score {
                        tracing_matrix[i][j] = Trace::Up;
                    }
                }

                // Tracking the cell with the maximum score
                if matrix[i][j] >= max_score {
                    max_index = (i, j);
                    max_score = matrix[i][j];
                }
            }
        }

        // Initialize variables for tracing
        let mut aligned_seq: Vec<(Option<usize>, Option<usize>)> = vec![];
        let mut current_aligned_seq1: Option<usize> = None;
        let mut current_aligned_seq2: Option<usize> = None;
        let (mut max_i, mut max_j) = max_index;

        while tracing_matrix[max_i][max_j] != Trace::Stop {
            if tracing_matrix[max_i][max_j] == Trace::MatchDiagonal {
                current_aligned_seq1 = Some(max_i - 1 + start_index);
                current_aligned_seq2 = Some(max_j - 1 + start_index);
                max_i -= 1;
                max_j -= 1;
            } else if tracing_matrix[max_i][max_j] == Trace::Up {
                current_aligned_seq1 = Some(max_i - 1 + start_index);
                current_aligned_seq2 = None;
                max_i -= 1;
                self.num_mismatches += 1;
            } else if tracing_matrix[max_i][max_j] == Trace::Left {
                current_aligned_seq1 = None;
                current_aligned_seq2 = Some(max_j - 1 + start_index);
                self.num_mismatches += 1;
                max_j -= 1;
            } else if tracing_matrix[max_i][max_j] == Trace::MisMatchDiagonal {
                current_aligned_seq1 = None;
                current_aligned_seq2 = None;
                self.num_mismatches += 1;
                self.mismatches
                    .push((max_i + start_index, max_j + start_index));
                max_i -= 1;
                max_j -= 1;
            }
            aligned_seq.push((current_aligned_seq1, current_aligned_seq2));
        }
        self.mismatches.reverse();
        aligned_seq.reverse();

        (start_index, aligned_seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_complete_match() {
        let mut sw_object = NeedlemanWunsch {
            first_sequence: vec![10],
            second_sequence: vec![10],
            ..Default::default()
        };
        assert_eq!(sw_object.match_sequences_base(), (1, vec![]));
    }

    #[test]
    fn test_single_complete_mismatch() {
        let mut sw_object = NeedlemanWunsch {
            first_sequence: vec![0],
            second_sequence: vec![1],
            ..Default::default()
        };
        assert_eq!(sw_object.match_sequences_base(), (0, vec![(None, None)],));
    }

    #[test]
    fn test_partial_one_off_match() {
        let mut sw_object = NeedlemanWunsch {
            first_sequence: vec![3, 3, 4, 4, 3, 1, 2, 4, 1],
            second_sequence: vec![4, 3, 4, 4, 1, 2, 3, 3],
            ..Default::default()
        };
        assert_eq!(
            sw_object.match_sequences_base(),
            (
                0,
                vec![
                    (None, None),
                    (Some(1), Some(1)),
                    (Some(2), Some(2)),
                    (Some(3), Some(3)),
                    (Some(4), None),
                    (Some(5), Some(4)),
                    (Some(6), Some(5))
                ]
            )
        );
    }

    #[test]
    fn test_gap_match() {
        let mut sw_object = NeedlemanWunsch {
            first_sequence: vec![1, 2, 3],
            second_sequence: vec![1, 4, 3],
            ..Default::default()
        };
        assert_eq!(
            sw_object.match_sequences_base(),
            (1, vec![(None, None), (Some(2), Some(2))])
        );
    }

    #[test]
    fn test_gap_start_match() {
        let mut sw_object = NeedlemanWunsch {
            first_sequence: vec![2, 4, 3],
            second_sequence: vec![1, 4, 3],
            ..Default::default()
        };
        assert_eq!(
            sw_object.match_sequences_base(),
            (
                0,
                vec![(None, None), (Some(1), Some(1)), (Some(2), Some(2))],
            )
        );
    }
}
