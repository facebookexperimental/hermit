/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;

use detcore::preemptions::PreemptionReader;
use detcore_model::collections::ReplayCursor;
use detcore_model::schedule::Op;
use detcore_model::schedule::SchedEvent;
use detcore_model::time::LogicalTime;

fn split_one(event: SchedEvent) -> Vec<SchedEvent> {
    let first_count = event.count / 2;
    let second_count = event.count - first_count;

    let first_end_time = event.end_time.map(|t| {
        LogicalTime::from_nanos((t.as_nanos() as i64 - 10i64 * second_count as i64) as u64)
    });

    vec![
        SchedEvent {
            count: first_count,
            op: Op::Branch,
            end_time: first_end_time,
            start_rip: None,
            end_rip: None,
            ..event
        },
        SchedEvent {
            count: 1,
            op: Op::OtherInstructions,
            end_time: first_end_time,
            start_rip: None,
            end_rip: None,
            ..event
        },
        SchedEvent {
            count: second_count,
            op: Op::Branch,
            start_rip: None,
            end_rip: None,
            ..event
        },
    ]
}

fn split_branches(
    event: SchedEvent,
    cursor: &ReplayCursor<SchedEvent>,
) -> impl IntoIterator<Item = SchedEvent> {
    match (&event, cursor.peek_nth(0), cursor.peek_nth(1)) {
        (
            SchedEvent {
                op: Op::Branch,
                count,
                ..
            },
            Some(SchedEvent {
                op: Op::OtherInstructions,
                ..
            }),
            Some(SchedEvent { op: Op::Branch, .. }),
        )
        | (
            SchedEvent {
                op: Op::Branch,
                count,
                ..
            },
            Some(SchedEvent { op: Op::Branch, .. }),
            _,
        ) if *count > 1 => split_one(event),
        _ => vec![event],
    }
}

pub fn split_branches_in_file(from_file: impl AsRef<Path>, backup: bool) -> anyhow::Result<()> {
    let mut preemptions = PreemptionReader::new(from_file.as_ref()).into_inner();
    preemptions.split_map(split_branches);

    if backup {
        let mut new_name = from_file.as_ref().to_path_buf();
        new_name.set_extension("bak");
        std::fs::rename(&from_file, &new_name)?;
    }

    preemptions
        .write_to_disk(from_file.as_ref())
        .map_err(anyhow::Error::msg)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use detcore::types::DetPid;
    use detcore::types::LogicalTime;
    use detcore::types::Op;
    use detcore::types::SchedEvent;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    pub fn test_split_one() {
        let event = SchedEvent {
            count: 3,
            dettid: DetPid::from_raw(3),
            end_time: Some(LogicalTime::from_nanos(10000)),
            op: Op::Branch,
            start_rip: None,
            end_rip: None,
        };

        let counts: Vec<_> = split_one(event)
            .into_iter()
            .map(|x| (x.op, x.count, x.end_time.unwrap().as_nanos()))
            .collect();

        assert_eq!(
            counts,
            vec![
                (Op::Branch, 1, 9980),
                (Op::OtherInstructions, 1, 9980),
                (Op::Branch, 2, 10000)
            ]
        );
    }
}
