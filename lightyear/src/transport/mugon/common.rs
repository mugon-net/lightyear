use crate::transport::{PacketReceiver, PacketSender, MTU};
use serde::Deserialize;
use std::future::Future;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::pin::Pin;

pub const YIELD_DELAY: u32 = 5;

pub fn socket_addr_to_id(socket_addr: &SocketAddr) -> u64 {
    let SocketAddr::V6(addr) = socket_addr else {
        return u64::MAX;
    };
    addr.ip().to_bits() as u64
}

pub fn id_to_socket_addr(id: u64) -> SocketAddr {
    SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(id as u128), 0, 0, 0))
}

#[cfg(target_arch = "wasm32")]
pub fn yield_to_browser() -> Pin<Box<dyn Future<Output = ()>>> {
    // Prevents stalling of the main thread by making sure to yield to the browser
    Box::pin(gloo_timers::future::TimeoutFuture::new(YIELD_DELAY))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn yield_to_browser() -> Pin<Box<dyn Future<Output = ()> + Send>> {
    // Prevents stalling of the main thread by making sure to yield to the browser
    Box::pin(tokio::time::sleep(std::time::Duration::from_millis(
        YIELD_DELAY,
    )))
}
