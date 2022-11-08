// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::net::TcpListener;

fn main() {
    let mut port_list = Vec::new();
    for i in 32768..65535 {
        port_list.push(TcpListener::bind(("127.0.0.1", i as u16)).unwrap());
        assert_eq!(
            port_list.last().unwrap().local_addr().unwrap(),
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), i))
        );
    }
    let result = TcpListener::bind("127.0.0.1:0");
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EADDRINUSE));
    drop(port_list.remove(0));
    let result = TcpListener::bind("127.0.0.1:0").unwrap();
    assert_eq!(
        result.local_addr().unwrap(),
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 32768))
    );
    for port in port_list {
        drop(port);
    }
    println!("Done");
}
