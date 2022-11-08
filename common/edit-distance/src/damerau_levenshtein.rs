/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use core::hash::Hash;
use std::collections::HashMap;

fn index(row_length: usize, row: usize, col: usize) -> usize {
    row_length * row + col
}

fn longest_common_prefix<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    let mut count = 0;

    let mut first = a.iter();
    let mut second = b.iter();

    while first.next() == second.next() {
        count += 1;
    }

    count
}

fn longest_common_suffix<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    let mut count = 0;

    let mut first = a.iter().rev();
    let mut second = b.iter().rev();

    while first.next() == second.next() {
        count += 1;
    }

    count
}

/// Finds the edit distance between `source` and `target`, taking into account
/// additions, deletions, and swaps. This has a time complexity of O(n^2) and a
/// space complexity of O(n^2).
pub fn damerau_lev<T>(mut source: &[T], mut target: &[T]) -> usize
where
    T: PartialEq + Hash + Eq,
{
    let prefix = longest_common_prefix(source, target);
    source = &source[prefix..];
    target = &target[prefix..];

    let suffix = longest_common_suffix(source, target);
    source = &source[..source.len() - suffix];
    target = &target[..target.len() - suffix];

    let m = source.len();
    let n = target.len();

    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }

    let width = n + 2;
    let height = m + 2;

    let mut matrix: Vec<usize> = vec![0; width * height];

    let inf: usize = m + n;

    matrix[0] = inf;
    for i in 0..=m {
        matrix[index(width, i + 1, 1)] = i;
        matrix[index(width, i + 1, 0)] = inf;
    }

    for j in 0..=n {
        matrix[index(width, 1, j + 1)] = j;
        matrix[index(width, 0, j + 1)] = inf;
    }

    let mut last_row = HashMap::<&T, usize>::new();

    for row in 1..=m {
        let ch_s = &source[row - 1];
        let mut last_match_col = 0;

        for col in 1..=n {
            let ch_t = &target[col - 1];
            let last_match_row = *last_row.get(&ch_t).unwrap_or(&0);

            let cost = if ch_s == ch_t { 0 } else { 1 };

            let dist_add = matrix[index(width, row, col + 1)] + 1;
            let dist_del = matrix[index(width, row + 1, col)] + 1;
            let dist_sub = matrix[index(width, row, col)] + cost;

            let dist_trans = matrix[index(width, last_match_row, last_match_col)]
                + (row - last_match_row - 1)
                + 1
                + (col - last_match_col - 1);

            // Find the minimum of the distances
            let mut min = dist_add;
            if dist_del < min {
                min = dist_del;
            }
            if dist_sub < min {
                min = dist_sub;
            }
            if dist_trans < min {
                min = dist_trans;
            }

            // Store the minimum as the cost for this cell
            matrix[index(width, row + 1, col + 1)] = min;

            // If there was a match, update the last matching column
            if cost == 0 {
                last_match_col = col;
            }
        }

        // Update the entry for this row's character
        last_row.insert(ch_s, row);
    }

    // Extract the bottom right-most cell -- that's the total distance
    matrix[index(width, m + 1, n + 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(unused)]
    fn print_matrix(matrix: &[usize], cols: usize) {
        let rows = matrix.len() / cols;
        for i in 0..rows {
            for j in 0..cols {
                print!("{:#3}, ", matrix[index(cols, i, j)]);
            }
            println!();
        }
        println!();
    }

    // Test cases
    //  * Two long strings with only one difference at the end
    //  * One long, one short
    //  * Two same-length, shuffled

    #[test]
    fn longest_common_prefix_test() {
        let source = vec!['A'; 10_000];
        let mut target = vec!['A'; 10_000];
        *target.last_mut().unwrap() = 'B';
        assert_eq!(longest_common_prefix(&source, &target), 9_999);
    }

    #[test]
    fn longest_common_suffix_test() {
        let source = vec!['A'; 10_000];
        let mut target = vec!['A'; 10_000];
        *target.last_mut().unwrap() = 'B';
        assert_eq!(longest_common_suffix(&source, &target), 0);

        let source = vec!['A'; 10_000];
        let mut target = vec!['A'; 10_000];
        *target.first_mut().unwrap() = 'B';
        assert_eq!(longest_common_suffix(&source, &target), 9_999);
    }

    #[test]
    fn smoke_test() {
        assert_eq!(
            damerau_lev(&['a', ' ', 'a', 'b', 'c', 't'], &['a', ' ', 'c', 'a', 't'],),
            2
        );
    }

    #[test]
    fn long_common_prefix_same_len() {
        let source = vec!['A'; 10_000];
        let mut target = vec!['A'; 10_000];
        *target.last_mut().unwrap() = 'B';
        assert_eq!(damerau_lev(&source, &target), 1);
    }

    #[test]
    fn long_common_suffix_same_len() {
        let source = vec!['A'; 10_000];
        let mut target = vec!['A'; 10_000];
        *target.first_mut().unwrap() = 'B';
        assert_eq!(damerau_lev(&source, &target), 1);
    }

    #[test]
    fn long_common_suffix_diff_len() {
        let source = vec!['A'; 10_000];

        let mut target = vec!['B', 'C'];
        target.append(&mut vec!['A'; 10_000]);

        assert_eq!(damerau_lev(&source, &target), 2);
    }

    #[test]
    fn same_length_shuffled() {
        use rand::seq::SliceRandom;

        const COUNT: usize = 1_000;

        let mut rng = rand::thread_rng();
        let mut source: Vec<u8> = Vec::with_capacity(COUNT);

        let mut x = 0;
        for _ in 0..COUNT {
            source.push(x);
            x = x.wrapping_add(1)
        }

        let target = source.clone();

        source.shuffle(&mut rng);

        assert!(damerau_lev(&source, &target) > 500);
    }
}
