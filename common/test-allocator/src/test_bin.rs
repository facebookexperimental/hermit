// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// Do some general allocations as a test

use std::collections::BTreeMap;
use std::collections::HashMap;

#[global_allocator]
static ALLOC: test_allocator::Global = test_allocator::Global;

fn main() {
    let mut x = vec![0u64, 1];
    for _ in 0..92 {
        x.push(x[x.len() - 1] + x[x.len() - 2]);
    }
    assert_eq!(x[93], 12200160415121876738);
    println!("{:?}", x);
    println!("test print");

    let h = (0..100)
        .zip((100..200).map(|x| x.to_string()))
        .collect::<HashMap<_, _>>();
    let b = (0..100)
        .zip((100..200).map(|x| x.to_string()))
        .collect::<BTreeMap<_, _>>();

    assert_eq!(h.get(&50), b.get(&50));

    test_allocator::GLOBAL.skip_to_offset(0x1000000).unwrap();
    let x = Box::new(5u64);
    assert_eq!(
        Box::into_raw(x) as usize,
        test_allocator::GLOBAL_SLAB_TOP - 0x1000000 - std::mem::size_of::<u64>()
    );
    println!("Success!");
}
