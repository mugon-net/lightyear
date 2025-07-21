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
    #[wasm_bindgen(js_namespace = window, js_name = registerCallbacks)]
    fn register_callbacks(
        own_id: u64,
        on_new_connection_callback: &JsValue,
        on_new_message: &JsValue,
        on_disconnected_from: &JsValue,
    );

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
        let status_tx_2 = status_tx.clone();

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

        let on_new_connection_callback: Closure<dyn FnMut(u64)> = Closure::new(move |id: u64| {
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
        let on_new_message: Closure<dyn FnMut(u64, Uint8Array)> =
            Closure::new(move |client_id: u64, data: Uint8Array| {
                let addr = id_to_socket_addr(client_id);
                serverbound_tx
                    .send((addr, Message::Binary(data.to_vec())))
                    .unwrap_or_else(|e| error!("receive mugon socket error: {:?}", e));
            });

        let on_disconnected_from: Closure<dyn FnMut(u64)> = Closure::new(move |id: u64| {
            let addr = id_to_socket_addr(id);
            status_tx_2
                .try_send(ServerIoEvent::ClientDisconnected(addr))
                .unwrap_or_else(|e| error!("receive disconnected from socket: {:?}", e));
        });

        register_callbacks(
            socket_addr_to_id(&self.server_addr),
            &on_new_connection_callback.as_ref().unchecked_ref(),
            &on_new_message.as_ref().unchecked_ref(),
            &on_disconnected_from.as_ref().unchecked_ref(),
        );

        // Leaking closures to js, so they continue to function after connect call has returned
        on_new_connection_callback.forget();
        on_new_message.forget();
        on_disconnected_from.forget();

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

        let handle_message = |msg: Message| match msg {
            Message::Binary(data) => {
                if !send(socket_addr_to_id(&addr), &*data) {
                    return true;
                }
                return false;
            }
            Message::Close => {
                close(socket_addr_to_id(&addr));
                return true;
            }
        };

        let mut closed = false;
        while !closed {
            #[cfg(target_arch = "wasm32")]
            {
                if let Ok(msg) = clientbound_rx.try_recv() {
                    closed = handle_message(msg);
                } else {
                    crate::transport::mugon::common::yield_to_browser().await;
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                if let Some(msg) = clientbound_rx.recv().await {
                    closed = handle_message(msg);
                } else {
                    closed = true;
                }
            }
        }
        close(socket_addr_to_id(&addr));
        clientbound_tx_map.lock().unwrap().remove(&addr);
        let _ = status_tx
            .try_send(ServerIoEvent::ClientDisconnected(addr))
            .unwrap();
    }
}

struct MugonServerSocketSender {
    server_addr: SocketAddr,
    addr_to_clientbound_tx: ClientBoundTxMap,
}

impl PacketSender for MugonServerSocketSender {
    fn send(&mut self, payload: &[u8], address: &SocketAddr) -> LightyearResult<()> {
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
            Err(Error::Io(std::io::Error::other(format!(
                "unable to find channel for client: {}",
                address
            ))))
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
        match self.serverbound_rx.try_recv() {
            Ok((addr, msg)) => match msg {
                Message::Binary(buf) => {
                    self.buffer[..buf.len()].copy_from_slice(&buf);
                    Ok(Some((&mut self.buffer[..buf.len()], addr)))
                }
                Message::Close => {
                    debug!("Mugon connection with {} closed", socket_addr_to_id(&addr));
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
