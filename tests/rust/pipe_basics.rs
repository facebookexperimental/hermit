// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::str;

use nix::unistd;

fn main() {
    let (fdread, fdwrite) = unistd::pipe().unwrap();
    let handle = std::thread::spawn(move || {
        let mut buf: [u8; 14] = [0; 14];
        for _ in 10..20 {
            assert_eq!(unistd::read(fdread, &mut buf), Ok(14));
            println!("Child received message: {}", str::from_utf8(&buf).unwrap());
        }
        assert!(unistd::close(fdread).is_ok());
    });

    for ix in 10..20 {
        println!("Parent writing to pipe..");
        let msg = format!("hello world {}", ix);
        assert_eq!(unistd::write(fdwrite, msg.as_bytes()), Ok(14));
    }
    assert!(unistd::close(fdwrite).is_ok());

    println!("Joining child..");
    handle.join().unwrap();
}
