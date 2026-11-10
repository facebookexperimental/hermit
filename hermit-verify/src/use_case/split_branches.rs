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

/// `event`, a branch event, as two: its first half of the branches, ending
/// with no RIP, then the rest, ending where `event` ended, with its RIP. A
/// schedule replay stops a thread at a branch event's count when the event
/// has a RIP (a timer stop), or when another branch event or another thread's
/// event follows it (`Replayer::resolve_time_to_run`). The first half is
/// always followed by the second, so it stays a stop. The second keeps
/// `event`'s RIP, so it is a stop exactly where `event` was, even when the
/// thread's own OtherInstructions follows it, as `hermit analyze`'s fragments
/// keep theirs.
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
            // A new cut: no recorded end, taken without a stop or not, is here.
            stopless: false,
            // The offset names where the original batch ended, which only
            // the second fragment does.
            instruction_offset: None,
            ..event
        },
        SchedEvent {
            count: second_count,
            op: Op::Branch,
            start_rip: None,
            end_rip: event.end_rip,
            ..event
        },
    ]
}

fn split_branches(
    event: SchedEvent,
    cursor: &ReplayCursor<SchedEvent>,
) -> impl IntoIterator<Item = SchedEvent> + use<> {
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
        ) if *count > 1 && event.instruction_offset.is_none() => split_one(event),
        _ => vec![event],
    }
}

/// Split the batches of branches in the preemption record at `from_file`.
/// The record is read fallibly: a format this Hermit does not read is a
/// typed refusal (`detcore::preemptions::RecordFormatRefusal`) and a
/// malformed one an error, never a panic.
pub fn split_branches_in_file(from_file: impl AsRef<Path>, backup: bool) -> anyhow::Result<()> {
    let mut preemptions = PreemptionReader::try_new(from_file.as_ref())?.into_inner();
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
            stopless: false,
            instruction_offset: None,
            op: Op::Branch,
            start_rip: None,
            end_rip: None,
        };

        let counts: Vec<_> = split_one(event)
            .into_iter()
            .map(|x| (x.op, x.count, x.end_time.unwrap().as_nanos()))
            .collect();

        assert_eq!(counts, vec![(Op::Branch, 1, 9980), (Op::Branch, 2, 10000)]);
    }

    /// The second half of a split timer stop keeps its RIP, so a schedule
    /// replay still stops the thread there when the thread's own
    /// OtherInstructions follows; the first half, which the second follows,
    /// needs none.
    #[test]
    fn a_split_timer_stop_keeps_its_rip_on_the_second_half() {
        let rip = std::num::NonZeroUsize::new(0x0040_110b).unwrap();
        let event = SchedEvent {
            count: 7,
            dettid: DetPid::from_raw(3),
            end_time: Some(LogicalTime::from_nanos(10000)),
            op: Op::Branch,
            start_rip: None,
            end_rip: Some(rip),
            stopless: false,
            instruction_offset: None,
        };
        let rips: Vec<_> = split_one(event)
            .into_iter()
            .map(|x| (x.count, x.end_rip))
            .collect();
        assert_eq!(rips, vec![(3, None), (4, Some(rip))]);
    }

    /// A batch's instruction offset names where the batch ended, so only the
    /// fragment that ends there keeps it.
    #[test]
    pub fn a_split_keeps_the_instruction_offset_on_the_fragment_that_ends_the_batch() {
        let offset = detcore::preemptions::InstructionOffset {
            rcb_target: 40,
            instructions: 5,
        };
        let event =
            SchedEvent::branches(DetPid::from_raw(3), 3).with_instruction_offset(Some(offset));
        let offsets: Vec<_> = split_one(event)
            .into_iter()
            .map(|fragment| fragment.instruction_offset)
            .collect();
        assert_eq!(offsets, vec![None, Some(offset)]);
    }

    /// Through the file the splitter edits: a batch of branches that an
    /// instruction-offset timer ended stays one unchanged event (its last
    /// branches can lie past the timer's target), while the same batch
    /// without an offset is split. A record of a newer format is refused,
    /// typed, without a panic.
    #[test]
    pub fn split_branches_in_file_leaves_an_offset_batch_whole() {
        let offset = detcore::preemptions::InstructionOffset {
            rcb_target: 40,
            instructions: 5,
        };
        let dir = tempfile::tempdir().unwrap();
        let tid = DetPid::from_raw(3);
        let schedule = |offset| {
            vec![
                SchedEvent::branches(tid, 10).with_instruction_offset(offset),
                SchedEvent {
                    op: Op::OtherInstructions,
                    count: 1,
                    ..SchedEvent::branches(tid, 1)
                        .with_end_rip(std::num::NonZeroUsize::new(0x1000).unwrap())
                },
                SchedEvent::branches(tid, 5),
            ]
        };
        let split = |name: &str, events: Vec<SchedEvent>| {
            let path = dir.path().join(name);
            let mut writer = detcore::preemptions::PreemptionWriter::new(None);
            for event in events {
                writer.insert_schedevent(event);
            }
            std::fs::write(&path, writer.into_string()).unwrap();
            split_branches_in_file(&path, false).unwrap();
            PreemptionReader::try_new(&path)
                .unwrap()
                .into_inner()
                .into_global()
        };
        let with = schedule(Some(offset));
        assert_eq!(split("with.json", with.clone()), with);
        let without = split("without.json", schedule(None));
        assert_eq!(without.len(), 4, "{without:?}");
        assert_eq!((without[0].count, without[1].count), (5, 5), "{without:?}");

        let newer = dir.path().join("newer.json");
        std::fs::write(
            &newer,
            serde_json::json!({"per_thread": {}, "global": [], "format_version": 99}).to_string(),
        )
        .unwrap();
        let error = split_branches_in_file(&newer, false).unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<detcore::preemptions::RecordFormatRefusal>()),
            "{error:#}"
        );
    }
}
