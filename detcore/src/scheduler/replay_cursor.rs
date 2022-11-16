/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::VecDeque;
use std::iter::FromIterator;

/// This is a queue like data structure used when replaying schedule
///
/// Apart from being an [Iterator] is supports methods [peek] and [peek_nth] to be able to look ahead arbitrary number of events forward
/// look ahead while replaying.
/// all of supported operations have O(1) time complexity
#[derive(Debug)]
pub struct ReplayCursor<T> {
    inner_data: VecDeque<T>,
}

impl<T> ReplayCursor<T> {
    /// peeks the following nth element from the top of the cursor
    pub fn peek_nth(&self, index: usize) -> Option<&T> {
        self.inner_data.get(index)
    }

    /// peeks the following item from the top of the cursor
    pub fn peek(&self) -> Option<&T> {
        self.peek_nth(0)
    }
}

impl<T> Iterator for ReplayCursor<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.inner_data.is_empty() {
            None
        } else {
            self.inner_data.pop_front()
        }
    }
}

impl<T> FromIterator<T> for ReplayCursor<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let reverse_data: VecDeque<_> = iter.into_iter().collect();
        Self {
            inner_data: reverse_data,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use pretty_assertions::assert_eq;

    use super::*;

    fn peek_pair<T>(cursor: &ReplayCursor<T>) -> (Option<&T>, Option<&T>) {
        (cursor.peek_nth(0), cursor.peek_nth(1))
    }

    #[test]
    fn test_peek() {
        let cursor: ReplayCursor<usize> = vec![1, 2, 3, 4, 5, 6].into_iter().collect();
        assert_eq!(cursor.peek(), Some(&1));
    }

    #[test]
    fn test_peek_empty() {
        let cursor = ReplayCursor::<usize> {
            inner_data: VecDeque::new(),
        };
        assert_eq!(cursor.peek(), None);
    }

    #[test]
    fn test_peek_pair() {
        let cursor: ReplayCursor<usize> = vec![1, 2, 3].into_iter().collect();
        assert_eq!(peek_pair(&cursor), (Some(&1), Some(&2)));
        assert_eq!(peek_pair(&cursor), (Some(&1), Some(&2)));
    }

    #[test]
    fn test_peek_pair_one() {
        let cursor: ReplayCursor<usize> = vec![1].into_iter().collect();
        assert_eq!(peek_pair(&cursor), (Some(&1), None));
        assert_eq!(peek_pair(&cursor), (Some(&1), None));
    }

    #[test]
    fn test_peek_pair_empty() {
        let cursor: ReplayCursor<usize> = ReplayCursor {
            inner_data: VecDeque::new(),
        };
        assert_eq!(peek_pair(&cursor), (None, None));
        assert_eq!(peek_pair(&cursor), (None, None));
    }

    #[test]
    fn test_next() {
        let mut cursor: ReplayCursor<usize> = vec![1, 2, 3].into_iter().collect();
        let result = vec![cursor.next(), cursor.next(), cursor.next(), cursor.next()];
        assert_eq!(result, vec![Some(1), Some(2), Some(3), None]);
    }

    #[test]
    fn test_peek_nth() {
        let cursor: ReplayCursor<usize> = vec![1, 2, 3].into_iter().collect();
        let result = vec![
            cursor.peek_nth(0),
            cursor.peek_nth(1),
            cursor.peek_nth(2),
            cursor.peek_nth(3),
        ];
        assert_eq!(result, vec![Some(&1), Some(&2), Some(&3), None]);
    }
}
