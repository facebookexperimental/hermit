/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A datatype to abstract a record of thread preemptions, as generated during chaos mode execution.

use std::collections::BTreeMap;
use std::fs::File;
use std::iter::FromIterator;
use std::path::Path;
use std::path::PathBuf;

use detcore_model::collections::ReplayCursor;
use serde::Deserialize;
use serde::Serialize;
use tracing::trace;

use crate::scheduler::runqueue::is_ordinary_priority;
use crate::scheduler::runqueue::DEFAULT_PRIORITY;
use crate::scheduler::runqueue::FIRST_PRIORITY;
use crate::scheduler::runqueue::LAST_PRIORITY;
use crate::scheduler::Priority;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::types::SchedEvent;

/// A record of all the preemptions and other scheduling events that occur during execution.
#[derive(PartialEq, Default, Debug, Eq, Clone, Hash, Serialize, Deserialize)]
pub struct PreemptionRecord {
    // TODO(T106838933): switch the keys from DetTid to Pedigree.
    /// A sorted list of end-of-timeslice preemption times for each thread.
    per_thread: BTreeMap<DetTid, ThreadHistory>,
    global: Vec<SchedEvent>,
}

impl std::fmt::Display for PreemptionRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let str = serde_json::to_string(&self).unwrap();
        write!(f, "{}", str)
    }
}

impl PreemptionRecord {
    /// TODO: This is not ideal because PreemptionRecord shouldn't be doing double duty as an eventlog.
    pub fn from_sched_events(events: Vec<SchedEvent>) -> Self {
        Self {
            per_thread: Default::default(),
            global: events,
        }
    }

    /// Make a copy of everything in the `PreemptionRecord` in a public format.
    pub fn extract_all(&self) -> BTreeMap<DetTid, ThreadHistory> {
        self.per_thread.clone()
    }

    /// Inverse of from_vecs.
    pub fn as_vecs(&self) -> BTreeMap<DetTid, Vec<(LogicalTime, Priority)>> {
        let mut bt = BTreeMap::new();
        for (tid, th) in &self.per_thread {
            bt.insert(*tid, th.as_vec());
        }
        bt
    }
    /// TODO: provide a better way to create an empty PreemptionRecord but with entries for a given set of tids.
    pub fn strip_contents(mut self) -> Self {
        for (_tid, history) in self.per_thread.iter_mut() {
            history.prio_changes = Vec::new();
            history.final_prio = 1000;
        }
        self.global = Vec::new();
        self
    }

    /// Iterates over each sched event and applies function event -> *event yielding resulting iterator as new events
    /// This effectively moves content of global allowing to split or delete events based on the function provided
    pub fn split_map<F, R>(&mut self, splitter: F)
    where
        R: IntoIterator<Item = SchedEvent>,
        F: Fn(SchedEvent, &ReplayCursor<SchedEvent>) -> R,
    {
        let mut result = Vec::new();
        let global = std::mem::take(&mut self.global);
        let mut cursor = ReplayCursor::from_iter(global);

        while let Some(event) = cursor.next() {
            for new_event in splitter(event, &cursor) {
                result.push(new_event);
            }
        }
        self.global = result;
    }

    /// Iterate over the schedevents.
    pub fn schedevents_iter_mut(&mut self) -> std::slice::IterMut<SchedEvent> {
        self.global.iter_mut()
    }

    /// Leave only the per-thread preemption records, not the global schedule.
    pub fn preemptions_only(&mut self) {
        self.global.clear();
    }

    /// Convert from a flat vector representation (Time,Priority_AFTER_Time) into the internal representation.
    /// The very first timeslice should have a zero timestamp, but it is ignored if nonzero.
    pub fn from_vecs(bt: &BTreeMap<DetTid, Vec<(LogicalTime, Priority)>>) -> Self {
        let mut bt2 = BTreeMap::new();
        for (tid, vec) in bt {
            let th = if vec.is_empty() {
                ThreadHistory {
                    final_prio: DEFAULT_PRIORITY,
                    prio_changes: Vec::new(),
                }
            } else {
                // We could insist on this invariant, but it's a little more flexible not to:
                // assert!(vec.get(0).unwrap().per_thread.is_zero());
                let (_final_end, final_prio) = vec.last().unwrap();
                let final_prio = *final_prio;

                let mut prio_changes = Vec::new();
                let mut it = vec.iter().peekable();
                while let Some((_this_ns, this_p)) = it.next() {
                    if let Some((next_ns, _next_p)) = it.peek() {
                        prio_changes.push((*next_ns, *this_p));
                    } else {
                        // Don't need the last one, because we've already used it's
                        // timestamp, and retrieved final_prio.
                        break;
                    }
                }
                assert!(final_prio >= FIRST_PRIORITY);
                assert!(final_prio <= LAST_PRIORITY);
                ThreadHistory {
                    final_prio,
                    prio_changes,
                }
            };
            bt2.insert(*tid, th);
        }
        PreemptionRecord {
            per_thread: bt2,
            global: Vec::new(),
        }
    }

    /// Extracts the inner global events.
    pub fn into_global(self) -> Vec<SchedEvent> {
        self.global
    }

    /// Save to disk.
    pub fn write_to_disk(&self, path: &Path) -> Result<(), String> {
        let mut str: String = self.to_string();
        str.push('\n');
        match File::create(path) {
            Ok(mut file) => match std::io::Write::write_all(&mut file, str.as_bytes()) {
                Ok(_) => Ok(()),
                Err(err) => Err(format!(
                    "Failed to write preemption record to file {:?}, error: {}",
                    path, err
                )),
            },
            Err(err) => Err(format!(
                "Failed to create file for preemption record {:?}, error: {}",
                path, err
            )),
        }
    }

    /// Perform internal invariant checks on the PreemptionRecord and return an error if
    /// it is not well formed.
    pub fn validate(&self) -> Result<(), String> {
        for (tid, history) in &self.per_thread {
            if !is_ordinary_priority(history.final_prio) {
                return Err(format!(
                    "final priority for thread {} invalid: {}",
                    tid, history.final_prio
                ));
            }
            {
                let mut time_last = None;
                for (count, (ns, prio)) in history.prio_changes.iter().enumerate() {
                    if let Some(last) = time_last {
                        if !is_ordinary_priority(*prio) {
                            return Err(format!(
                                "preemption priority #{} for thread {} invalid: {}",
                                count, tid, history.final_prio
                            ));
                        }
                        if *ns <= last {
                            return Err(format!(
                                "Timestamps failed to monotonically increase ({}), in series:\n {:?}",
                                ns, history.prio_changes
                            ));
                        }
                    }
                    time_last = Some(*ns);
                }
            }
        }
        Ok(())
    }

    /// Normalize priorities for readibility.
    pub fn normalize(&self) -> PreemptionRecord {
        let mut clone = self.clone();

        let mut priomap: BTreeMap<Priority, Priority> = BTreeMap::new();
        for history in clone.per_thread.values_mut() {
            let _ = priomap.insert(history.final_prio, 0);
            for (_ns, prio) in &history.prio_changes {
                let _ = priomap.insert(*prio, 0);
            }
        }

        let mut cur_prio = DEFAULT_PRIORITY;
        for val in priomap.values_mut() {
            assert!(cur_prio <= LAST_PRIORITY);
            *val = cur_prio;
            cur_prio += 1;
        }
        for history in clone.per_thread.values_mut() {
            history.final_prio = *priomap.get(&history.final_prio).unwrap();
            for (_ns, prio) in &mut history.prio_changes {
                *prio = *priomap.get(prio).unwrap();
            }
        }

        // One more pass to remove anything that just has default priority its whole lifetime.
        let mut finalmap = BTreeMap::new();
        for (tid, history) in clone.per_thread.into_iter() {
            if history.final_prio != DEFAULT_PRIORITY || !history.prio_changes.is_empty() {
                assert!(finalmap.insert(tid, history).is_none());
            }
        }
        clone.per_thread = finalmap;
        clone
    }

    /// Remove whichever is the latest preemption across all thread histories. If
    /// there are no priority changes in any thread history, the entire last thread
    /// (highest thread number) is completely removed.
    /// Note this function assumes the last priority change in the list has the latest
    /// preempt time.
    pub fn with_latest_preempt_removed(&self) -> PreemptionRecord {
        let mut preempts_latest_prio_changes: Vec<(DetTid, LogicalTime)> = self
            .per_thread
            .clone()
            .into_iter()
            .map(|(tid, th)| {
                (
                    tid,
                    th.prio_changes
                        .last() // Assume last prio change in the list has the latest preempt time
                        .map_or(LogicalTime::ZERO, |prio_change| prio_change.0),
                )
            })
            .collect();
        preempts_latest_prio_changes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        let tid_with_preempt_to_drop = preempts_latest_prio_changes
            .into_iter()
            .map(|(tid, _th)| tid)
            .next();

        let mut clone = self.clone();
        if let Some(tid) = tid_with_preempt_to_drop {
            let thread_history = &mut clone.per_thread.get_mut(&tid).unwrap();
            if !thread_history.prio_changes.is_empty() {
                // There were prio changes for at least one of the threads, remove that
                // preemption as the one to drop
                let removed_prio = thread_history.prio_changes.pop().unwrap().1;
                thread_history.final_prio = removed_prio;
            } else {
                // There were no prio changes for any threads, so just remove the last
                // thread completely as the drop
                clone.per_thread.pop_last();
            }
        }
        clone
    }
}

/// The record of priorities and preemptions for a particular thread.
#[derive(
    PartialEq, // Silly protection from rustfmt disagreements.
    Debug,
    Eq,
    Clone,
    Hash,
    Serialize,
    Deserialize,
)]
pub struct ThreadHistory {
    /// The priority of the last timeslice, after the last preemption.
    /// (This is also the initial priority if there are no preemptions.)
    pub final_prio: Priority,

    /// The preemption points and priority just BEFORE the preemption occurred.  It's
    /// private, and thus you must go through the iterator, as this will change in the
    /// future.
    prio_changes: Vec<(LogicalTime, Priority)>,
}

impl ThreadHistory {
    /// An empty thread history, with default priority only.
    pub fn new() -> Self {
        ThreadHistory {
            final_prio: DEFAULT_PRIORITY,
            prio_changes: Vec::new(),
        }
    }

    /// Turns the thread history into an iterator that produces a stream of preemption events.
    #[allow(clippy::should_implement_trait)]
    pub fn into_iter(self) -> ThreadHistoryIterator {
        ThreadHistoryIterator {
            full_history: self,
            ix: 0,
        }
    }

    /// Flatten the time series into a uniform representation.
    /// This is a `(Time, Priority_AFTER_Time)` representation.
    /// The very first timeslice will have a zero timestamp.
    // Note that this is different than the internal representation which groups together
    // `Priority_UNTIL_Time, Time`, i.e. shifted by one.
    pub fn as_vec(&self) -> Vec<(LogicalTime, Priority)> {
        let mut vec = Vec::new();
        let mut time0 = LogicalTime::from_nanos(0);

        for (time1, prio) in &self.prio_changes {
            vec.push((time0, *prio));
            time0 = *time1;
        }
        vec.push((time0, self.final_prio));
        vec
    }

    /// Return the priority of the initial timeslice after the thread starts running.
    pub fn initial_priority(&self) -> Priority {
        if let Some((_ns, pr)) = self.prio_changes.get(0) {
            *pr
        } else {
            self.final_prio
        }
    }
}

impl Default for ThreadHistory {
    fn default() -> Self {
        Self::new()
    }
}

/// An iterator over the preemption history of a thread.  Does not include the final
/// timeslice, which does not have an ending time.
// TODO: in the future this will perform "lazy IO" and pull from disk in batches.
#[derive(PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct ThreadHistoryIterator {
    /// For now we store the full history in memory:
    full_history: ThreadHistory,
    /// Index that tracks our position.
    ix: usize,
}

impl ThreadHistoryIterator {
    /// Return the priority of the initial slice after the thread starts running.
    pub fn initial_priority(&self) -> Priority {
        self.full_history.initial_priority()
    }

    /// Return the priority of the last slice after the last preemption.
    pub fn final_priority(&self) -> Priority {
        self.full_history.final_prio
    }
}

impl Iterator for ThreadHistoryIterator {
    type Item = (LogicalTime, Priority);

    fn next(&mut self) -> Option<Self::Item> {
        let vec = &self.full_history.prio_changes;
        if vec.len() > self.ix {
            let elt = vec[self.ix];
            self.ix += 1;
            Some(elt)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use detcore_model::schedule::Op;
    use pretty_assertions::assert_eq;
    use test_case::test_case;

    use super::*;

    #[test]
    fn print_preemptionrecord() {
        let (file, path) = tempfile::NamedTempFile::new().unwrap().keep().unwrap();
        drop(file);
        let mut pw = PreemptionWriter::new(Some(path.clone()));
        let tid1 = DetTid::from_raw(2);
        let tid2 = DetTid::from_raw(4);
        pw.register_thread(tid1, 1000);
        pw.register_thread(tid2, 1000);

        pw.insert_reprioritization(tid1, LogicalTime::from_nanos(3), 1000, 3);
        pw.insert_reprioritization(tid1, LogicalTime::from_nanos(30), 3, 30);
        pw.insert_reprioritization(tid1, LogicalTime::from_nanos(300), 30, 300);
        pw.insert_reprioritization(tid2, LogicalTime::from_nanos(2), 1000, 2);
        pw.insert_reprioritization(tid2, LogicalTime::from_nanos(20), 2, 20);
        pw.insert_reprioritization(tid2, LogicalTime::from_nanos(200), 20, 200);

        // First, round trip through a pretty-printed string
        // ------------------------------------------------
        let str: String = serde_json::to_string_pretty(&pw.inner).unwrap();
        eprintln!("{}", str);
        let pr2: PreemptionRecord = serde_json::from_str(&str).unwrap();
        eprintln!("Round trip {:?}", pr2);
        assert_eq!(pw.inner, pr2);

        // Second, actually write it out to disk
        // -------------------------------------
        pw.flush().unwrap();
        let reader = PreemptionReader::new(&path);
        let th1 = reader.extract_thread_record(&tid1).unwrap();
        let th2 = reader.extract_thread_record(&tid2).unwrap();
        assert_eq!(th1.final_prio, 300);
        assert_eq!(th2.final_prio, 200);

        let it1 = th1.into_iter();
        let it2 = th2.into_iter();

        assert_eq!(it1.initial_priority(), 1000);
        assert_eq!(it2.initial_priority(), 1000);

        let v1: Vec<(LogicalTime, Priority)> = it1.collect();
        let v2: Vec<(LogicalTime, Priority)> = it2.collect();
        assert_eq!(
            v1,
            vec![
                (LogicalTime::from_nanos(3), 1000),
                (LogicalTime::from_nanos(30), 3),
                (LogicalTime::from_nanos(300), 30)
            ]
        );
        assert_eq!(
            v2,
            vec![
                (LogicalTime::from_nanos(2), 1000),
                (LogicalTime::from_nanos(20), 2),
                (LogicalTime::from_nanos(200), 20)
            ]
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn round_trip_vec_representations() {
        let str = r#"{"per_thread":{"2":{"final_prio":1716,"prio_changes":[[946684799000013020,7301],[946684799000034020,9081],[946684799000041600,9238],[946684799000054790,865],
[946684799000057440,751],[946684799000061970,275],[946684799000062730,5135],[946684799000069530,6339],
[946684799000082850,1123],[946684799000101140,7875],[946684799000140625,4203],[946684799000171780,8611],
[946684799000183550,6306],[946684799000184440,7958],[946684799000195750,8919],[946684799000226150,69],
[946684799000236380,5915],[946684799000278180,3514],[946684799000320050,30],[946684799000334630,4629],
[946684799000344650,2926],[946684799000355020,710],[946684799000365030,3513],[946684799000386350,4881],
[946684799000396360,4852],[946684799000406840,4935],[946684799000426980,6672],[946684799000437970,7727],
[946684799000452410,7017],[946684799000462430,1572],[946684799000546210,6395],[946684799000548120,3726],
[946684799000562700,846],[946684799000583090,7838],[946684799000603310,8291],[946684799000655180,210],
[946684799000666230,4576],[946684799000680910,4974],[946684799000723020,9160],[946684799000776780,439],
[946684799000777080,6791],[946684799000787220,3015],[946684799000809090,7489],[946684799000840870,7165],
[946684799000852855,4326],[946684799000854355,358],[946684799000866575,4448],[946684799000903415,6848],
[946684799000913455,1899],[946684799000923630,2117],[946684799000963980,7705],[946684799001007090,8683],
[946684799001016950,3317],[946684799001017980,5261],[946684799001027540,3478],[946684799001029990,6474],
[946684799001053545,4823],[946684799001068395,4508],[946684799001073095,194],[946684799001121035,5944],
[946684799001171285,8408],[946684799001171295,4493],[946684799001192845,3481]]}},"global":[]}"#;
        let pr: PreemptionRecord = serde_json::from_str(str).unwrap();
        let vecs = pr.as_vecs();
        let pr2 = PreemptionRecord::from_vecs(&vecs);
        assert_eq!(pr, pr2);
    }

    #[test]
    fn normalize_preemption_record() {
        let str = r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1000,"prio_changes":[]},
"7":{"final_prio":9722,"prio_changes":[]},
"9":{"final_prio":9982,"prio_changes":[[946684799006227400,7839]]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#;
        let pr: PreemptionRecord = serde_json::from_str(&str).unwrap();
        let pr2 = pr.normalize();
        pr2.validate().unwrap();
        let bmap = pr2.as_vecs();
        // Normalization may remove the useless default-prio entry:
        if let Some(x) = bmap.get(&DetTid::from_raw(3)) {
            assert_eq!(x, &vec![(LogicalTime::from_nanos(0), 1000)]);
        }
        if let Some(x) = bmap.get(&DetTid::from_raw(5)) {
            assert_eq!(x, &vec![(LogicalTime::from_nanos(0), 1000)]);
        }
        assert_eq!(
            bmap.get(&DetTid::from_raw(7)).unwrap(),
            &vec![(LogicalTime::from_nanos(0), 1002)]
        );
        assert_eq!(
            bmap.get(&DetTid::from_raw(9)).unwrap(),
            &vec![
                (LogicalTime::from_nanos(0), 1001),
                (LogicalTime::from_nanos(946684799006227400), 1003)
            ]
        );
        if let Some(x) = bmap.get(&DetTid::from_raw(11)) {
            assert_eq!(x, &vec![(LogicalTime::from_nanos(0), 1000)]);
        }
    }

    #[test_case(
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1002,"prio_changes":[[946684799006227410,1000],[946684799006227415,1001]]},
"9":{"final_prio":1002,"prio_changes":[[946684799006227400,1000],[946684799006227405,1001]]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#,
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1001,"prio_changes":[[946684799006227410,1000]]},
"9":{"final_prio":1002,"prio_changes":[[946684799006227400,1000],[946684799006227405,1001]]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#
        ; "removes latest priority change and coalesces into final priority"
    )]
    #[test_case(
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1000,"prio_changes":[]},
"9":{"final_prio":1001,"prio_changes":[[946684799006227400,1000]]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#,
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1000,"prio_changes":[]},
"9":{"final_prio":1000,"prio_changes":[]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#
        ; "removes only priority change and coalesces into final priority"
    )]
    #[test_case(
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1000,"prio_changes":[]},
"9":{"final_prio":1000,"prio_changes":[]},
"11":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#,
        r#"{"per_thread":{
"3":{"final_prio":1000,"prio_changes":[]},
"5":{"final_prio":1000,"prio_changes":[]},
"9":{"final_prio":1000,"prio_changes":[]}},"global":[]}"#
        ; "removes entire history for last tid if no non-default priority changes"
    )]
    fn with_latest_preempt_removed(pr_json: &str, expected_pr_json: &str) {
        let pr: PreemptionRecord = serde_json::from_str(pr_json).unwrap();
        let expected_pr: PreemptionRecord = serde_json::from_str(expected_pr_json).unwrap();

        let pr_with_latest_removed = pr.with_latest_preempt_removed();

        pr_with_latest_removed.validate().unwrap();
        self::assert_eq!(pr_with_latest_removed, expected_pr);
    }

    #[test]
    fn test_split_map() {
        let mut pr: PreemptionRecord = serde_json::from_str(
            r#"
            {
                "per_thread" : {
                },
                "global" : [
                    {
                        "dettid": 3,
                        "op": "OtherInstructions",
                        "count": 1,
                        "start_rip": null,
                        "end_rip": null,
                        "end_time": 946684799000000000
                    },
                    {
                        "dettid": 3,
                        "op": "Branch",
                        "count": 311,
                        "start_rip": null,
                        "end_rip": null,
                        "end_time": 946684799000003110
                    },
                    {
                        "dettid": 3,
                        "op": "OtherInstructions",
                        "count": 1,
                        "start_rip": null,
                        "end_rip": null,
                        "end_time": 946684799000003110
                    }
                ]
            }
        "#,
        )
        .unwrap();
        let original = pr.global.clone();
        pr.split_map(|e, _| vec![e]); // shouldn't change contents with this
        assert_eq!(original, pr.global);

        pr.split_map(|e, _| match &e {
            // this will split event with Op::Branch into two
            SchedEvent { op: Op::Branch, .. } => vec![e.clone(), e],
            _ => vec![e],
        });

        assert_eq!(
            pr.global.iter().map(|e| e.op).collect::<Vec<_>>(),
            vec![
                Op::OtherInstructions,
                Op::Branch,
                Op::Branch,
                Op::OtherInstructions
            ]
        );

        pr.split_map(|_, _| vec![]); // this will remove all entries
        assert_eq!(pr.global, Vec::new());
    }
}

// Reader and Writer implementations:
// =============================================================================

/// A writer for a stream of preemption events.
#[derive(Debug)]
pub struct PreemptionWriter {
    inner: PreemptionRecord,
    dest: Option<PathBuf>,
    flushed: bool,
}

impl PreemptionWriter {
    /// A new, empty record of preemptions, with an optional location on disk that it will be
    /// written to.  If no path is supplied, the results will accumulate in memory only.
    pub fn new(path: Option<PathBuf>) -> Self {
        PreemptionWriter {
            inner: Default::default(),
            dest: path,
            flushed: false,
        }
    }

    /// Does the record have zero entries?
    pub fn is_empty(&self) -> bool {
        self.inner.per_thread.is_empty()
    }

    /// Total number of elements in the record, across all threads.
    pub fn len(&self) -> usize {
        let mut count = 0;
        for v in self.inner.per_thread.values() {
            count += v.prio_changes.len();
        }
        count
    }

    /// Register a thread with its default/final priority.
    pub fn register_thread(&mut self, tid: DetTid, prio: Priority) {
        if self
            .inner
            .per_thread
            .insert(
                tid,
                ThreadHistory {
                    final_prio: prio,
                    prio_changes: Vec::new(),
                },
            )
            .is_some()
        {
            panic!(
                "PreemptionRecord: error, cannot re-register thread id already registered: {}",
                tid
            )
        }
    }

    /// Insert a new preemption point for the given thread.
    /// It must monotonically increase in time.
    ///
    /// For now it (redundantly) takes both the priority of the timeslice just finished,
    /// and the next one coming up.  This serves as a sanity check.
    pub fn insert_reprioritization(
        &mut self,
        tid: DetTid,
        time: LogicalTime,
        prior_prio: Priority,
        next_prio: Priority,
    ) {
        let history = self.inner.per_thread.get_mut(&tid).unwrap_or_else(|| {
            panic!(
                "PreemptionRecord: Cannot insert a preemption before registering thread {}",
                &tid
            )
        });
        assert_eq!(history.final_prio, prior_prio);

        if let Some((last, _prio)) = history.prio_changes.last() {
            assert!(&time > last);
        }
        history.prio_changes.push((time, prior_prio));
        history.final_prio = next_prio;
    }

    /// Add a SchedEvent to the global log of thread behavior.
    pub fn insert_schedevent(&mut self, ev: SchedEvent) {
        if ev.count > 0 {
            self.inner.global.push(ev)
            // TODO, aggregation: possibly check if the last event was the SAME, and combine them,
            // increasing the count.
        } else {
            trace!("NOT recording scheduled event with zero count!");
        }
    }

    /// Set the priority of the current thread after a change.
    pub fn set_current(&mut self, tid: DetTid, new_prio: Priority) {
        let history = self.inner.per_thread.get_mut(&tid).unwrap_or_else(|| {
            panic!(
                "PreemptionRecord: Cannot set current priority before registering thread {}",
                &tid
            )
        });
        history.final_prio = new_prio;
    }

    /// Abort writing to disk and instead gather the output thusfar into a string.
    pub fn into_string(mut self) -> String {
        self.flushed = true;
        self.inner.to_string()
    }

    /// Finish any IO necessary to flush the record to disk.
    /// This will also happen automatically on drop.
    pub fn flush(mut self) -> Result<(), String> {
        self.flushed = true;
        self.write_to_disk()
    }

    fn write_to_disk(&mut self) -> Result<(), String> {
        if let Some(path) = &self.dest {
            self.inner.write_to_disk(path)
        } else {
            Err(
                "Cannot write_to_disk because this PreemptionWriter was created without a backing file.".to_string()
            )
        }
    }
}

impl Drop for PreemptionWriter {
    fn drop(&mut self) {
        if !self.flushed && self.dest.is_some() {
            if let Err(e) = self.write_to_disk() {
                panic!("Error while dropping PreemptionWriter: {}", e);
            }
        }
    }
}

/// A reader for a stream of preemption events.
#[derive(Debug)]
pub struct PreemptionReader {
    inner: PreemptionRecord,
}

/// Read a full trace from disk.  Panic if it doesn't load.
pub fn read_trace(path: &Path) -> Vec<SchedEvent> {
    let pr = read_preemption_record(path);
    pr.global
}

// TODO: we should implement streaming and not read this all at once.
fn read_preemption_record(path: &Path) -> PreemptionRecord {
    let string = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("Error reading file {:?}:\n {}", &path, e));
    let pr: PreemptionRecord = serde_json::from_str(&string).unwrap_or_else(|e| {
        panic!(
            "Error parsing PreemptionRecord from JSON: {}\nJSON contents:\n{}",
            e, string
        )
    });
    if let Err(e) = pr.validate() {
        panic!(
            "Invalid PreemptionRecord when loading from path {}. Error:\n {}",
            path.display(),
            e
        );
    }
    pr
}

// TODO: eventually this should do streaming IO, and abstract it behind this interface.
impl PreemptionReader {
    /// Access the stored `PreemptionRecord`, and eagerly or lazily load its data from disk.
    pub fn new(path: &Path) -> Self {
        let pr = read_preemption_record(path);
        PreemptionReader { inner: pr }
    }

    /// Gets the inner `PreemptionRecord`.
    pub fn into_inner(self) -> PreemptionRecord {
        self.inner
    }

    /// Extract a copy of all the entries for a particular thread.  Note that this returns None if
    /// there is NO record for the thread (not registered).  Otherwise, it returns a copy of the
    /// `ThreadHistory`, which may be read into memory or not.
    pub fn extract_thread_record(&self, tid: &DetTid) -> Option<ThreadHistory> {
        self.inner.per_thread.get(tid).cloned()
    }

    /// Return the initial priority for a thread, if it is recorded in the record.
    pub fn thread_initial_priority(&self, tid: &DetTid) -> Option<Priority> {
        self.inner.per_thread.get(tid).map(|x| x.initial_priority())
    }

    /// Return all the threads in this record.
    pub fn all_threads(&self) -> Vec<DetTid> {
        // TODO(T110956298): have this return an iterator.
        self.inner.per_thread.keys().copied().collect()
    }

    /// Load the full record of preemptions into memory.
    pub fn load_all(&self) -> PreemptionRecord {
        self.inner.clone()
    }

    /// Size in number of preemptions, across all threads.
    pub fn size(&self) -> usize {
        let mut sum = 0;
        for th in self.inner.per_thread.values() {
            sum += th.prio_changes.len()
        }
        sum
    }
}

/// Take a file containing a PreemptionRecord and strip the times from all the events.
/// Optionally take a destination path, otherwise create a destination file in the same directory
/// with our own naming convention.
pub fn strip_times_from_events_file(
    sched_path: &Path,
    dest: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let new_path = dest.unwrap_or_else(|| sched_path.with_extension("notimes"));
    let mut preemptions = PreemptionReader::new(sched_path).into_inner();
    for se in preemptions.schedevents_iter_mut() {
        se.end_time = None;
    }
    preemptions
        .write_to_disk(new_path.as_ref())
        .map_err(anyhow::Error::msg)?;
    Ok(new_path)
}
