use crate::transport::{PacketReceiver, PacketSender, MTU};
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};

pub fn socket_addr_to_id(socket_addr: &SocketAddr) -> u64 {
    let SocketAddr::V6(addr) = socket_addr else {
        return u64::MAX;
    };
    addr.ip().to_bits() as u64
}

pub fn id_to_socket_addr(id: u64) -> SocketAddr {
    SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(id as u128), 0, 0, 0))
}
