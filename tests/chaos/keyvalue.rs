/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The second thread expects to have the first's writes to the keyvalue service
//! be visible because it does lots of work first, but this ordering isn't
//! actually enforced.

use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;

type Key = u64;
type Value = u64;

fn keyvalue_service(store: Receiver<(Key, Value)>, rx: Receiver<Key>, tx: Sender<Option<Value>>) {
    let mut kv = std::collections::HashMap::new();
    loop {
        match store.try_recv() {
            Ok((k, v)) => kv.insert(k, v),
            _ => None,
        };
        match rx.try_recv() {
            Ok(k) => tx.send(kv.get(&k).copied()).ok(),
            _ => None,
        };
        std::thread::sleep(std::time::Duration::from_micros(1));
    }
}

fn main() {
    let (store_tx1, store_rx) = std::sync::mpsc::channel();
    let (req_tx, req_rx) = std::sync::mpsc::channel();
    let (resp_tx, resp_rx) = std::sync::mpsc::channel();
    let store_tx2 = store_tx1.clone();

    let _h1 = std::thread::spawn(move || {
        keyvalue_service(store_rx, req_rx, resp_tx);
    });

    let h2 = std::thread::spawn(move || {
        store_tx1.send((1, 10)).unwrap();
        store_tx1.send((2, 20)).unwrap();
        store_tx1.send((3, 30)).unwrap();
    });

    let h3 = std::thread::spawn(move || {
        for i in 10..10000 {
            store_tx2.send((i, i * 10)).unwrap();
        }
        req_tx.send(2).unwrap();
        let resp = resp_rx.recv().unwrap();
        assert_eq!(resp, Some(20))
    });

    h2.join().unwrap();
    h3.join().unwrap();
    // Let h1 get cancelled
    println!("Success!");
}
