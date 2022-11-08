// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use nix::sys::socket::socketpair;
use nix::sys::socket::AddressFamily;
use nix::sys::socket::SockFlag;
use nix::sys::socket::SockType;

fn main() {
    if matches!(std::env::var("HERMIT_MODE"), Ok(mode) if mode == "record") {
        // TODO: Record mode currently hangs for this test
        eprintln!("Skipping test in record mode.");
        return;
    }

    let (sock1, sock2) = socketpair(
        AddressFamily::Unix,
        SockType::Stream,
        None,
        SockFlag::empty(),
    )
    .unwrap();

    // WARNING: this assumes the Linux socket behavior, which is to NOT block a write call if there
    // is enough buffer space.  This test therefore depends on that buffer being more than 5 bytes.
    nix::unistd::write(sock1, b"Hello").unwrap();

    let mut buf = [0; 5];
    nix::unistd::read(sock2, &mut buf).unwrap();
    assert_eq!(&buf[..], b"Hello");
    println!("Received message. Test complete.\n");
}
