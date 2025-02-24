#![cfg(feature = "mugon")]

use crate::client::io::transport::{ClientTransportBuilder, ClientTransportEnum};
use crate::client::io::{ClientIoEvent, ClientIoEventReceiver, ClientNetworkEventSender};
use crate::transport::error::{Error, Result};
use crate::transport::io::IoState;
use crate::transport::mugon::common::{socket_addr_to_id, ReceiveResponse};
use crate::transport::{
    BoxedReceiver, BoxedSender, PacketReceiver, PacketSender, Transport, LOCAL_SOCKET, MTU,
};
use async_compat::Compat;
use bevy::tasks::IoTaskPool;
use js_sys::Promise;
use serde_wasm_bindgen::from_value;
use std::net::SocketAddr;
use std::rc::Rc;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot::{Receiver, Sender};
use tracing::{debug, info};
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = window, js_name = conenctToMugonSocket)]
    fn connect() -> Promise; // bool

    #[wasm_bindgen(js_namespace = window, js_name = closeMugonSocket)]
    fn close(id: u64);

    #[wasm_bindgen(js_namespace = window, js_name = sendFromMugonSocket)]
    fn send(to_id: u64, value: &[u8]) -> bool;

    #[wasm_bindgen(js_namespace = window, js_name = receiveFromMugonSocket)]
    fn receive(from_id: u64) -> Promise; // Option<(Vec<u8>, bool)>
}

pub(crate) struct MugonClientSocketBuilder {
    pub(crate) server_addr: SocketAddr,
    pub(crate) local_addr: SocketAddr,
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
        // TODO: This can exhaust all available memory unless there is some other way to limit the amount of in-flight data in place
        let (to_server_sender, mut to_server_receiver) = mpsc::unbounded_channel::<Vec<u8>>();
        let (from_server_sender, from_server_receiver) = mpsc::unbounded_channel::<Vec<u8>>();
        // channels used to cancel the task
        let (close_tx, close_rx) = async_channel::bounded(1);
        // channels used to check the status of the io task
        let (status_tx, status_rx) = async_channel::bounded(1);

        let local_id = socket_addr_to_id(&self.local_addr);
        let server_id = socket_addr_to_id(&self.server_addr);

        let (send0, recv0) = tokio::sync::oneshot::channel::<bool>();
        let (send1, recv1) = tokio::sync::oneshot::channel::<bool>();

        IoTaskPool::get()
            .spawn(Compat::new(async move {
                info!("Starting client mugon client task");
                if let Ok(js_value) = JsFuture::from(connect()).await {
                    if let Some(connected) = js_value.as_bool() {
                        if connected {
                            status_tx.send(ClientIoEvent::Connected).await.unwrap();
                        }

                        let _ = send0.send(connected);
                        let _ = send1.send(connected);
                    } else {
                        let _ = send0.send(false);
                        let _ = send1.send(false);
                    }
                }
            }))
            .detach();

        IoTaskPool::get()
            .spawn(Compat::new(async move {
                loop {
                    tokio::select! {
                        Ok(event) = close_rx.recv() => {
                            match event {
                                ClientIoEvent::Disconnected(e) => {
                                    debug!("Stopping mugon io task. Reason: {:?}", e);
                                    return;
                                }
                                _ => {}
                            }
                        }
                        Ok(js_value) = JsFuture::from(receive(local_id)) => {
                             if let Ok(response) = from_value::<ReceiveResponse>(js_value) {
                                if response.closed {
                                    let _ = status_tx.send(ClientIoEvent::Disconnected(std::io::Error::other("mugon connection was closed by the server or lost").into())).await;
                                    debug!("Stopping mugon io task. Connection was dropped");
                                    return;
                                } else {
                                    let _ = from_server_sender.send(response.data);
                                };
                            }
                        }
                    }
                }
            }))
            .detach();
        IoTaskPool::get()
            .spawn(Compat::new(async move {
                loop {
                    tokio::select! {
                        recv = to_server_receiver.recv() => {
                            if let Some(msg) = recv {
                                if !send(server_id, msg.as_slice()) {
                                    let _ = status_tx.send(ClientIoEvent::Disconnected(std::io::Error::other("mugon connection was lost").into())).await;
                                    return;
                                }
                            } else {
                                return;
                            }
                        }
                    }
                }
            }))
            .detach();

        let sender = MugonClientSocketSender {
            serverbound_tx: to_server_sender,
        };
        let receiver = MugonClientSocketReceiver {
            server_addr: self.server_addr,
            clientbound_rx: from_server_receiver,
            buffer: [0; MTU],
        };

        Ok((
            ClientTransportEnum::Mugon(MugonClientSocket { receiver, sender }),
            IoState::Connecting,
            Some(ClientIoEventReceiver(status_rx)),
            Some(ClientNetworkEventSender(close_tx)),
        ))
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
