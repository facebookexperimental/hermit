/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::cmp::min;

type IndexAndAlignments = (usize, Vec<(Option<usize>, Option<usize>)>);

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Trace {
    Stop,
    Left,
    Up,
    MatchDiagonal,
    MisMatchDiagonal,
}

#[derive(PartialEq, Eq, Debug)]
pub enum NeedlemanWunschError {
    MatrixTooLarge { cells: usize, max_cells: usize },
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
        self.match_sequences_bounded(scoring_function, gap_penalty, mismatch_penalty, usize::MAX)
            .expect("Needleman-Wunsch matrix dimensions overflowed")
    }

    /// Globally align the sequences, refusing work above `max_matrix_cells` before allocating the
    /// traceback matrix. Scores use two rolling rows, so only traceback storage remains quadratic.
    pub fn match_sequences_bounded(
        &mut self,
        scoring_function: Option<fn(T, T) -> i32>,
        gap_penalty: Option<i32>,
        mismatch_penalty: Option<i32>,
        max_matrix_cells: usize,
    ) -> Result<IndexAndAlignments, NeedlemanWunschError> {
        let gap_penalty: i32 = gap_penalty.unwrap_or(-1);
        let mismatch_penalty: i32 = mismatch_penalty.unwrap_or(-2);
        let scoring_function = scoring_function.unwrap_or(simple_score);

        self.mismatches.clear();
        self.num_mismatches = 0;

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
        let cells = row
            .checked_mul(col)
            .ok_or(NeedlemanWunschError::MatrixTooLarge {
                cells: usize::MAX,
                max_cells: max_matrix_cells,
            })?;
        if cells > max_matrix_cells {
            return Err(NeedlemanWunschError::MatrixTooLarge {
                cells,
                max_cells: max_matrix_cells,
            });
        }

        let mut previous_scores = (0..col).map(|j| gap_penalty * j as i32).collect::<Vec<_>>();
        let mut current_scores = vec![0; col];
        let mut tracing_matrix = vec![Trace::Stop; cells];
        for trace in tracing_matrix.iter_mut().take(col).skip(1) {
            *trace = Trace::Left;
        }
        for i in 1..row {
            current_scores[0] = gap_penalty * i as i32;
            tracing_matrix[i * col] = Trace::Up;
            for j in 1..col {
                let match_value: i32 = scoring_function(
                    self.first_sequence[i - 1 + start_index].clone(),
                    self.second_sequence[j - 1 + start_index].clone(),
                );

                let diagonal_score = previous_scores[j - 1]
                    + if match_value > 0 {
                        match_value
                    } else {
                        mismatch_penalty
                    };
                let horizontal_score = current_scores[j - 1] + gap_penalty;
                let vertical_score = previous_scores[j] + gap_penalty;

                let (score, trace) =
                    if diagonal_score >= horizontal_score && diagonal_score >= vertical_score {
                        (
                            diagonal_score,
                            if match_value > 0 {
                                Trace::MatchDiagonal
                            } else {
                                Trace::MisMatchDiagonal
                            },
                        )
                    } else if horizontal_score >= vertical_score {
                        (horizontal_score, Trace::Left)
                    } else {
                        (vertical_score, Trace::Up)
                    };
                current_scores[j] = score;
                tracing_matrix[i * col + j] = trace;
            }
            std::mem::swap(&mut previous_scores, &mut current_scores);
        }

        let mut aligned_seq: Vec<(Option<usize>, Option<usize>)> = vec![];
        let (mut i, mut j) = (row - 1, col - 1);

        while i > 0 || j > 0 {
            match tracing_matrix[i * col + j] {
                Trace::MatchDiagonal => {
                    aligned_seq.push((Some(i - 1 + start_index), Some(j - 1 + start_index)));
                    i -= 1;
                    j -= 1;
                }
                Trace::Up => {
                    aligned_seq.push((Some(i - 1 + start_index), None));
                    i -= 1;
                    self.num_mismatches += 1;
                }
                Trace::Left => {
                    aligned_seq.push((None, Some(j - 1 + start_index)));
                    j -= 1;
                    self.num_mismatches += 1;
                }
                Trace::MisMatchDiagonal => {
                    aligned_seq.push((None, None));
                    self.num_mismatches += 2;
                    self.mismatches
                        .push((i - 1 + start_index, j - 1 + start_index));
                    i -= 1;
                    j -= 1;
                }
                Trace::Stop => unreachable!("traceback stopped before reaching matrix origin"),
            }
        }
        self.mismatches.reverse();
        aligned_seq.reverse();

        Ok((start_index, aligned_seq))
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
                    (Some(6), Some(5)),
                    (None, None),
                    (None, None)
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

    #[test]
    fn bounded_alignment_rejects_large_matrices() {
        let mut sw_object = NeedlemanWunsch {
            first_sequence: vec![1; 100],
            second_sequence: vec![2; 100],
            ..Default::default()
        };
        assert_eq!(
            sw_object.match_sequences_bounded(None, None, None, 10_000),
            Err(NeedlemanWunschError::MatrixTooLarge {
                cells: 10_201,
                max_cells: 10_000,
            })
        );
    }

    #[test]
    fn alignment_includes_unmatched_suffix() {
        let mut sw_object = NeedlemanWunsch {
            first_sequence: vec![1, 2],
            second_sequence: vec![1, 2, 3],
            ..Default::default()
        };
        assert_eq!(sw_object.match_sequences_base(), (2, vec![(None, Some(2))]));
    }
}
