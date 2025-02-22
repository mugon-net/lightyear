#![cfg(target_family = "wasm")]

pub struct MugonServerBuilder {
    pub(crate) server_addr: SocketAddr,
}

use super::super::error::Result;
use crate::client::io::transport::ClientTransportBuilder;
use crate::server::io::transport::{ServerTransportBuilder, ServerTransportEnum};
use crate::server::io::{ServerIoEvent, ServerIoEventReceiver, ServerNetworkEventSender};
use crate::transport::error::Error;
use crate::transport::io::IoState;
use crate::transport::mugon::common::{id_to_socket_addr, socket_addr_to_id};
use crate::transport::{BoxedReceiver, BoxedSender, PacketReceiver, PacketSender, Transport, MTU};
use async_compat::Compat;
use bevy::tasks::{futures_lite, IoTaskPool};
use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{debug, error, info};
use wasm_bindgen::prelude::wasm_bindgen;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = window, js_name = acceptMugonSocketConnection)]
    async fn accept_new_connection() -> Option<u64>;

    #[wasm_bindgen(js_namespace = window, js_name = sendFromMugonSocket)]
    fn send(to_id: u64, value: &[u8]);

    #[wasm_bindgen(js_namespace = window, js_name = closeMugonSocket)]
    fn close(id: u64);

    #[wasm_bindgen(js_namespace = window, js_name = receiveFromMugonSocket)]
    async fn receive(from_id: u64) -> Option<(Vec<u8>, bool)>;
}

type ClientBoundTxMap = Arc<Mutex<HashMap<SocketAddr, UnboundedSender<Message>>>>;

enum Message {
    Binary(Vec<u8>),
    Close,
}

impl ServerTransportBuilder for MugonServerBuilder {
    fn start(
        self,
    ) -> Result<(
        ServerTransportEnum,
        IoState,
        Option<ServerIoEventReceiver>,
        Option<ServerNetworkEventSender>,
    )> {
        let (serverbound_tx, serverbound_rx) = unbounded_channel::<(SocketAddr, Message)>();
        let clientbound_tx_map = ClientBoundTxMap::new(Mutex::new(HashMap::new()));
        // channels used to cancel the task
        let (close_tx, close_rx) = async_channel::unbounded();
        // channels used to check the status of the io task
        let (status_tx, status_rx) = async_channel::unbounded();
        let addr_to_task = Arc::new(Mutex::new(HashMap::new()));

        let sender = MugonServerSocketSender {
            server_addr: self.server_addr,
            addr_to_clientbound_tx: clientbound_tx_map.clone(),
        };
        let receiver = MugonServerSocketReceiver {
            buffer: [0; MTU],
            server_addr: self.server_addr,
            serverbound_rx,
        };
        IoTaskPool::get()
            .spawn(Compat::new(async move {
                info!("Starting mugon server task");
                status_tx
                    .send(ServerIoEvent::ServerConnected)
                    .await
                    .unwrap();
                loop {
                    tokio::select! {
                        Ok(event) = close_rx.recv() => {
                            match event {
                                ServerIoEvent::ServerDisconnected(e) => {
                                    debug!("Stopping mugon io task. Reason: {:?}", e);
                                    drop(addr_to_task);
                                    return;
                                }
                                ServerIoEvent::ClientDisconnected(addr) => {
                                    debug!("Stopping mugon io task associated with address: {:?} because we received a disconnection signal from netcode", addr);
                                    addr_to_task.lock().unwrap().remove(&addr);
                                    clientbound_tx_map.lock().unwrap().remove(&addr);
                                }
                                _ => {}
                            }
                        }
                        Ok(id) = accept_new_connection() => {
                            let clientbound_tx_map = clientbound_tx_map.clone();
                            let serverbound_tx = serverbound_tx.clone();
                            let task = IoTaskPool::get().spawn(Compat::new(
                                MugonServerSocket::handle_client(id_to_socket_addr(id), serverbound_tx, clientbound_tx_map, status_tx.clone())
                            ));
                            addr_to_task.lock().unwrap().insert(id_to_socket_addr(id), task);
                        }
                    }
                }
            }))
            .detach();
        Ok((
            ServerTransportEnum::Mugon(MugonServerSocket {
                local_addr: self.server_addr,
                sender,
                receiver,
            }),
            IoState::Connecting,
            Some(ServerIoEventReceiver(status_rx)),
            None,
        ))
    }
}

pub struct MugonServerSocket {
    local_addr: SocketAddr,
    sender: MugonServerSocketSender,
    receiver: MugonServerSocketReceiver,
}

impl Transport for MugonServerSocket {
    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn split(self) -> (BoxedSender, BoxedReceiver) {
        (Box::new(self.sender), Box::new(self.receiver))
    }
}

impl MugonServerSocket {
    async fn handle_client(
        addr: SocketAddr,
        serverbound_tx: UnboundedSender<(SocketAddr, Message)>,
        clientbound_tx_map: Arc<Mutex<HashMap<SocketAddr, UnboundedSender<Message>>>>,
        status_tx: async_channel::Sender<ServerIoEvent>,
    ) {
        info!("New MugonSocket connection: {}", addr);
        let (clientbound_tx, mut clientbound_rx) = unbounded_channel::<Message>();
        clientbound_tx_map
            .lock()
            .unwrap()
            .insert(addr, clientbound_tx);
        let clientbound_handle = IoTaskPool::get().spawn(async move {
            while let Some(msg) = clientbound_rx.recv().await {
                match msg {
                    Message::Binary(data) => {
                        send(socket_addr_to_id(&addr), &*data);
                    }
                    Message::Close => {
                        close(socket_addr_to_id(&addr));
                    }
                }
            }
            close(socket_addr_to_id(&addr));
        });
        let serverbound_handle = IoTaskPool::get().spawn(async move {
            while let Some((data, closed)) = receive(socket_addr_to_id(&addr)).await {
                let msg = if closed {
                    Message::Close
                } else {
                    Message::Binary(data)
                };
                serverbound_tx
                    .send((addr, msg))
                    .unwrap_or_else(|e| error!("receive mugon socket error: {:?}", e));
            }
        });
        let _closed = futures_lite::future::race(clientbound_handle, serverbound_handle).await;
        info!("Connection with {} closed", addr);
        clientbound_tx_map.lock().unwrap().remove(&addr);
        // notify netcode that the io task got disconnected
        let _ = status_tx
            .send(ServerIoEvent::ClientDisconnected(addr))
            .await;
        // dropping the task handles cancels them
    }
}

struct MugonServerSocketSender {
    server_addr: SocketAddr,
    addr_to_clientbound_tx: ClientBoundTxMap,
}

impl PacketSender for MugonServerSocketSender {
    fn send(&mut self, payload: &[u8], address: &SocketAddr) -> Result<()> {
        if let Some(clientbound_tx) = self.addr_to_clientbound_tx.lock().unwrap().get(address) {
            clientbound_tx
                .send(Message::Binary(payload.to_vec()))
                .map_err(|e| {
                    Error::Io(
                        std::io::Error::other(format!("unable to send message to client: {}", e))
                            .into(),
                    )
                })
        } else {
            // consider that if the channel doesn't exist, it's because the connection was closed
            Ok(())
            // Err(std::io::Error::other(format!(
            //     "unable to find channel for client: {}",
            //     address
            // )))
        }
    }
}

struct MugonServerSocketReceiver {
    buffer: [u8; MTU],
    server_addr: SocketAddr,
    serverbound_rx: UnboundedReceiver<(SocketAddr, Message)>,
}

impl PacketReceiver for MugonServerSocketReceiver {
    fn recv(&mut self) -> Result<Option<(&mut [u8], SocketAddr)>> {
        match self.serverbound_rx.try_recv() {
            Ok((addr, msg)) => match msg {
                Message::Binary(buf) => {
                    self.buffer[..buf.len()].copy_from_slice(&buf);
                    Ok(Some((&mut self.buffer[..buf.len()], addr)))
                }
                Message::Close => {
                    info!("Mugon connection closed");
                    Ok(None)
                }
                _ => Ok(None),
            },
            Err(e) => {
                if e == TryRecvError::Empty {
                    Ok(None)
                } else {
                    Err(Error::Io(
                        std::io::Error::other(format!(
                            "unable to receive message from client: {}",
                            e
                        ))
                        .into(),
                    ))
                }
            }
        }
    }
}
