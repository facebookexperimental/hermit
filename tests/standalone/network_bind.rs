// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::net::TcpListener;

fn main() {
    let first_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    assert_eq!(
        first_listener.local_addr().unwrap(),
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 32768))
    );
    let second_listener = TcpListener::bind("127.0.0.1:32769").unwrap();
    assert_eq!(
        second_listener.local_addr().unwrap(),
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 32769))
    );
    let third_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    assert_eq!(
        third_listener.local_addr().unwrap(),
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 32770))
    );
    drop(second_listener);
    let second_listener = TcpListener::bind("127.0.0.1:32769").unwrap();
    assert_eq!(
        second_listener.local_addr().unwrap(),
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 32769))
    );
    println!("Done");
}
