#![cfg(feature = "mugon")]

pub struct MugonServerBuilder {
    pub(crate) server_addr: SocketAddr,
}

use super::super::error::Result as LightyearResult;
use crate::client::io::transport::ClientTransportBuilder;
use crate::server::io::transport::{ServerTransportBuilder, ServerTransportEnum};
use crate::server::io::{ServerIoEvent, ServerIoEventReceiver, ServerNetworkEventSender};
use crate::transport::error::Error;
use crate::transport::io::IoState;
use crate::transport::mugon::common::{id_to_socket_addr, socket_addr_to_id};
use crate::transport::{BoxedReceiver, BoxedSender, PacketReceiver, PacketSender, Transport, MTU};
use bevy::tasks::{futures_lite, IoTaskPool, Task};
use js_sys::Uint8Array;
use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{debug, error, info, warn};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    // TODO Also disconnect / status callback?
    #[wasm_bindgen(js_namespace = window, js_name = hostAndRegisterCallbacks)]
    fn host_and_register_callbacks(new_connection_callback: &JsValue, receive_callback: &JsValue);

    #[wasm_bindgen(js_namespace = window, js_name = sendFromMugonSocket)]
    fn send(to_id: u64, value: &[u8]) -> bool;

    #[wasm_bindgen(js_namespace = window, js_name = closeMugonSocket)]
    fn close(id: u64);
}

type ClientBoundTxMap = Arc<Mutex<HashMap<SocketAddr, UnboundedSender<Message>>>>;

enum Message {
    Binary(Vec<u8>),
    Close,
}

impl ServerTransportBuilder for MugonServerBuilder {
    fn start(
        self,
    ) -> LightyearResult<(
        ServerTransportEnum,
        IoState,
        Option<ServerIoEventReceiver>,
        Option<ServerNetworkEventSender>,
    )> {
        // channels used to pass messages from/to the rest of the lightyear framework
        let (serverbound_tx, serverbound_rx) = unbounded_channel::<(SocketAddr, Message)>();
        let clientbound_tx_map = ClientBoundTxMap::new(Mutex::new(HashMap::new()));

        // channel used to cancel the io task and check if it was cancelled
        let (close_tx, close_rx) = async_channel::unbounded::<ServerIoEvent>();

        // channel used to send/check the status of the io task
        let (status_tx, status_rx) = async_channel::unbounded();

        // TODO Remove from map, to release handle after it finished / client disconnected
        #[cfg(not(target_arch = "wasm32"))]
        let addr_to_task = Arc::new(Mutex::new(HashMap::<SocketAddr, Task<()>>::new()));

        let sender = MugonServerSocketSender {
            server_addr: self.server_addr,
            addr_to_clientbound_tx: clientbound_tx_map.clone(),
        };
        let receiver = MugonServerSocketReceiver {
            buffer: [0; MTU],
            server_addr: self.server_addr,
            serverbound_rx,
        };

        status_tx.try_send(ServerIoEvent::ServerConnected)?;

        let new_connection_callback: Closure<dyn FnMut(u64)> = Closure::new(move |id: u64| {
            let clientbound_tx_map = clientbound_tx_map.clone();
            #[cfg(target_arch = "wasm32")]
            wasm_bindgen_futures::spawn_local(MugonServerSocket::handle_client(
                id_to_socket_addr(id),
                clientbound_tx_map,
                status_tx.clone(),
            ));
            #[cfg(not(target_arch = "wasm32"))]
            {
                let task = IoTaskPool::get().spawn(MugonServerSocket::handle_client(
                    id_to_socket_addr(id),
                    clientbound_tx_map,
                    status_tx.clone(),
                ));
                addr_to_task
                    .lock()
                    .unwrap()
                    .insert(id_to_socket_addr(id), task);
            }
        });
        let receive_callback: Closure<dyn FnMut(u64, Uint8Array)> =
            Closure::new(move |client_id: u64, data: Uint8Array| {
                let addr = id_to_socket_addr(client_id);
                serverbound_tx
                    .send((addr, Message::Binary(data.to_vec())))
                    .unwrap_or_else(|e| error!("receive mugon socket error: {:?}", e));
            });

        host_and_register_callbacks(
            &new_connection_callback.as_ref().unchecked_ref(),
            &receive_callback.as_ref().unchecked_ref(),
        );

        // Leaking closures to js, so they continue to function after connect call has returned
        new_connection_callback.forget();
        receive_callback.forget();

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
        clientbound_tx_map: Arc<Mutex<HashMap<SocketAddr, UnboundedSender<Message>>>>,
        status_tx: async_channel::Sender<ServerIoEvent>,
    ) {
        debug!("New MugonSocket connection: {}", addr);
        let (clientbound_tx, mut clientbound_rx) = unbounded_channel::<Message>();
        clientbound_tx_map
            .lock()
            .unwrap()
            .insert(addr, clientbound_tx);
        let mut closed = false;
        while !closed {
            debug!("handle_client send loop");
            crate::transport::mugon::common::yield_to_browser().await;
            if let Ok(msg) = clientbound_rx.try_recv() {
                debug!("handle_client send loop (recv)");
                match msg {
                    Message::Binary(data) => {
                        debug!("Server sending");
                        if !send(socket_addr_to_id(&addr), &*data) {
                            debug!("Connection with {} lost", addr);
                            return;
                        }
                        debug!("Server sent");
                    }
                    Message::Close => {
                        debug!("Server close received");
                        close(socket_addr_to_id(&addr));
                        closed = true;
                    }
                }
            }
            // tokio::select! {
            //     msg = clientbound_rx.recv() => {
            //         debug!("handle_client send loop (recv)");
            //         if let Some(msg) = msg {
            //             match msg {
            //                 Message::Binary(data) => {
            //                     debug!("Server sending");
            //                     if !send(socket_addr_to_id(&addr), &*data) {
            //                         debug!("Connection with {} lost", addr);
            //                         return;
            //                     }
            //                     debug!("Server sent");
            //                 }
            //                 Message::Close => {
            //                     debug!("Server close received");
            //                     close(socket_addr_to_id(&addr));
            //                     closed = true;
            //                 }
            //             }
            //         } else {
            //             debug!("Server close received");
            //             close(socket_addr_to_id(&addr));
            //             closed = true;
            //         }
            //     },
            //     _ = crate::transport::mugon::common::yield_to_browser() => {debug!("yield")}
            // }
        }
        close(socket_addr_to_id(&addr));
        debug!("Connection with {} closed", addr);
        clientbound_tx_map.lock().unwrap().remove(&addr);
        // notify netcode that the io task got disconnected
        let _ = status_tx
            .try_send(ServerIoEvent::ClientDisconnected(addr))
            .unwrap();
        // dropping the task handles cancels them
    }
}

struct MugonServerSocketSender {
    server_addr: SocketAddr,
    addr_to_clientbound_tx: ClientBoundTxMap,
}

impl PacketSender for MugonServerSocketSender {
    fn send(&mut self, payload: &[u8], address: &SocketAddr) -> LightyearResult<()> {
        debug!("Mugon Server Packet Sender send");
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
    fn recv(&mut self) -> LightyearResult<Option<(&mut [u8], SocketAddr)>> {
        debug!("Mugon Server Packet Receiver recv");
        match self.serverbound_rx.try_recv() {
            Ok((addr, msg)) => match msg {
                Message::Binary(buf) => {
                    debug!("Mugon Server Packet Receiver received");
                    self.buffer[..buf.len()].copy_from_slice(&buf);
                    Ok(Some((&mut self.buffer[..buf.len()], addr)))
                }
                Message::Close => {
                    debug!("Mugon connection closed");
                    Ok(None)
                }
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
