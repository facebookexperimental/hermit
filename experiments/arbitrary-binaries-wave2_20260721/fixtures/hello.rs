use std::thread;

fn main() {
    let handles: Vec<_> = (0_u64..4)
        .map(|index| thread::spawn(move || (0_u64..100_000).map(|n| n ^ index).sum::<u64>()))
        .collect();
    let total: u64 = handles.into_iter().map(|handle| handle.join().unwrap()).sum();
    println!("rustc-ok {total}");
}
