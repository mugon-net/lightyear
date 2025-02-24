#![cfg(feature = "mugon")]

use crate::client::io::transport::{ClientTransportBuilder, ClientTransportEnum};
use crate::client::io::{ClientIoEventReceiver, ClientNetworkEventSender};
use crate::transport::error::{Error, Result};
use crate::transport::io::IoState;
use crate::transport::{
    BoxedReceiver, BoxedSender, PacketReceiver, PacketSender, Transport, LOCAL_SOCKET, MTU,
};
use std::net::SocketAddr;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

pub(crate) struct MugonClientSocketBuilder {
    pub(crate) server_addr: SocketAddr,
}

impl ClientTransportBuilder for MugonClientSocketBuilder {
    fn connect(
        self,
    ) -> Result<(
        ClientTransportEnum,
        IoState,
        Option<ClientIoEventReceiver>,
        Option<ClientNetworkEventSender>,
    )> {
        todo!()
    }
}

pub struct MugonClientSocket {
    sender: MugonClientSocketSender,
    receiver: MugonClientSocketReceiver,
}

impl Transport for MugonClientSocket {
    fn local_addr(&self) -> SocketAddr {
        LOCAL_SOCKET
    }

    fn split(self) -> (BoxedSender, BoxedReceiver) {
        (Box::new(self.sender), Box::new(self.receiver))
    }
}

struct MugonClientSocketSender {
    serverbound_tx: UnboundedSender<Vec<u8>>,
}

impl PacketSender for MugonClientSocketSender {
    fn send(&mut self, payload: &[u8], address: &SocketAddr) -> Result<()> {
        self.serverbound_tx.send(payload.to_vec()).map_err(|e| {
            std::io::Error::other(format!("unable to send message to server: {:?}", e)).into()
        })
    }
}

struct MugonClientSocketReceiver {
    buffer: [u8; MTU],
    server_addr: SocketAddr,
    clientbound_rx: UnboundedReceiver<Vec<u8>>,
}

impl PacketReceiver for MugonClientSocketReceiver {
    fn recv(&mut self) -> Result<Option<(&mut [u8], SocketAddr)>> {
        match self.clientbound_rx.try_recv() {
            Ok(msg) => {
                self.buffer[..msg.len()].copy_from_slice(&msg);
                Ok(Some((&mut self.buffer[..msg.len()], self.server_addr)))
            }
            Err(e) => {
                if e == TryRecvError::Empty {
                    Ok(None)
                } else {
                    Err(std::io::Error::other(format!(
                        "unable to receive message from client: {}",
                        e
                    ))
                    .into())
                }
            }
        }
    }
}
