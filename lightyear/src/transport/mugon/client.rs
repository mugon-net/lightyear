#![cfg(feature = "mugon")]

use crate::client::io::transport::{ClientTransportBuilder, ClientTransportEnum};
use crate::client::io::{ClientIoEvent, ClientIoEventReceiver, ClientNetworkEventSender};
use crate::server::io::ServerIoEvent;
use crate::transport::error::Error::NotConnected;
use crate::transport::error::{Error, Result as LightyearResult};
use crate::transport::io::IoState;
use crate::transport::mugon::common::{id_to_socket_addr, socket_addr_to_id};
use crate::transport::{
    BoxedReceiver, BoxedSender, PacketReceiver, PacketSender, Transport, LOCAL_SOCKET, MTU,
};
use async_compat::Compat;
use bevy::tasks::IoTaskPool;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot::{Receiver, Sender};
use tracing::{debug, error, info, warn};
use wasm_bindgen::prelude::{wasm_bindgen, Closure};
use wasm_bindgen::{JsCast, JsValue};

#[wasm_bindgen]
extern "C" {
    // TODO Also disconnect / status callback?
    #[wasm_bindgen(js_namespace = window, js_name = registerCallbacks)]
    fn register_callbacks(
        on_new_connection_callback: &JsValue,
        on_new_message: &JsValue,
        on_disconnected_from: &JsValue,
    );

    #[wasm_bindgen(js_namespace = window, js_name = sendFromMugonSocket)]
    fn send(to_id: u64, value: &[u8]) -> bool;

    #[wasm_bindgen(js_namespace = window, js_name = closeMugonSocket)]
    fn close(id: u64);
}

enum Message {
    Binary(Vec<u8>),
    Close,
}

pub(crate) struct MugonClientSocketBuilder {
    pub(crate) server_addr: SocketAddr,
    pub(crate) local_addr: SocketAddr,
}

impl ClientTransportBuilder for MugonClientSocketBuilder {
    fn connect(
        self,
    ) -> LightyearResult<(
        ClientTransportEnum,
        IoState,
        Option<ClientIoEventReceiver>,
        Option<ClientNetworkEventSender>,
    )> {
        // channels used to pass messages from/to the rest of the lightyear framework
        // TODO: This can exhaust all available memory unless there is some other way to limit the amount of in-flight data in place
        let (to_server_sender, mut to_server_receiver) = mpsc::unbounded_channel::<Vec<u8>>();
        let (from_server_sender, from_server_receiver) = mpsc::unbounded_channel::<Message>();

        // channel used to cancel the io task and check if it was cancelled
        let (close_tx, close_rx) = async_channel::unbounded();
        let close_rx_for_send_task = close_rx.clone();

        // channel used to send/check the status of the io task
        let (status_tx_from_connect_task, status_rx) = async_channel::unbounded();
        let status_tx_from_send_task = status_tx_from_connect_task.clone();
        let status_tx_from_disconnect_callback = status_tx_from_connect_task.clone();

        // channel used to signal from the connect task to the send task, if the connection init was successful
        let (send_connected_event, recv_connected_event) = async_channel::unbounded();

        let local_id = socket_addr_to_id(&self.local_addr);
        let server_id = socket_addr_to_id(&self.server_addr);

        let on_new_connection_callback: Closure<dyn FnMut(bool)> =
            Closure::new(move |success: bool| {
                let status_tx_from_connect_task = status_tx_from_connect_task.clone();
                let send_connected_event = send_connected_event.clone();
                if success {
                    status_tx_from_connect_task
                        .try_send(ClientIoEvent::Connected)
                        .unwrap();
                    send_connected_event.try_send(success).unwrap();
                } else {
                    status_tx_from_connect_task
                        .try_send(ClientIoEvent::Disconnected(NotConnected))
                        .unwrap();
                    send_connected_event.try_send(success).unwrap();
                }
            });
        let on_new_message: Closure<dyn FnMut(u64, Option<Vec<u8>>)> =
            Closure::new(move |_: u64, data: Option<Vec<u8>>| {
                let _ = from_server_sender
                    .send(data.map_or_else(|| Message::Close, |d| Message::Binary(d)))
                    .unwrap();
            });

        let on_disconnected_from: Closure<dyn FnMut(u64)> = Closure::new(move |id: u64| {
            status_tx_from_disconnect_callback
                .try_send(ClientIoEvent::Disconnected(Error::UserRequest))
                .unwrap_or_else(|e| error!("receive disconnected from socket: {:?}", e));
        });

        register_callbacks(
            &on_new_connection_callback.as_ref().unchecked_ref(),
            &on_new_message.as_ref().unchecked_ref(),
            &on_disconnected_from.as_ref().unchecked_ref(),
        );

        // Leaking closures to js, so they continue to function after connect call has returned
        on_new_connection_callback.forget();
        on_new_message.forget();
        on_disconnected_from.forget();

        // Task for sending outgoing packets
        wasm_bindgen_futures::spawn_local(async move {
            let mut connected = false;
            while !connected {
                #[cfg(target_arch = "wasm32")]
                {
                    if let Ok(event) = close_rx_for_send_task.try_recv() {
                        match event {
                            ClientIoEvent::Disconnected(e) => {
                                debug!("Stopping mugon client send task. Reason: {:?}", e);
                                return;
                            }
                            _ => {}
                        }
                    }
                    if let Ok(success) = recv_connected_event.try_recv() {
                        if success {
                            connected = true;
                        } else {
                            return;
                        }
                    }
                    crate::transport::mugon::common::yield_to_browser().await;
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    tokio::select! {
                        Ok(success) = recv_connected_event.recv() => {
                            if success {
                                connected = true;
                            } else {
                                return;
                            }
                        },
                        Ok(event) = close_rx_for_send_task.recv() => {
                                match event {
                                    ClientIoEvent::Disconnected(e) => {
                                        debug!("Stopping mugon receive task. Reason: {:?}", e);
                                        return;
                                    }
                                    _ => {}
                                }
                        },
                    }
                }
            }
            loop {
                if let Ok(msg) = to_server_receiver.try_recv() {
                    if !send(server_id, msg.as_slice()) {
                        let _ = status_tx_from_send_task
                            .try_send(ClientIoEvent::Disconnected(
                                std::io::Error::other("mugon connection was lost").into(),
                            ))
                            .unwrap();
                        return;
                    }
                } else {
                    if let Ok(event) = close_rx_for_send_task.try_recv() {
                        match event {
                            ClientIoEvent::Disconnected(e) => {
                                debug!("Stopping mugon client send task. Reason: {:?}", e);
                                return;
                            }
                            _ => {}
                        }
                    }
                    crate::transport::mugon::common::yield_to_browser().await;
                }
            }
        });

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
            IoState::Connected,
            None,
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
    fn send(&mut self, payload: &[u8], address: &SocketAddr) -> LightyearResult<()> {
        self.serverbound_tx.send(payload.to_vec()).map_err(|e| {
            std::io::Error::other(format!("unable to send message to server: {:?}", e)).into()
        })
    }
}

struct MugonClientSocketReceiver {
    buffer: [u8; MTU],
    server_addr: SocketAddr,
    clientbound_rx: UnboundedReceiver<Message>,
}

impl PacketReceiver for MugonClientSocketReceiver {
    fn recv(&mut self) -> LightyearResult<Option<(&mut [u8], SocketAddr)>> {
        match self.clientbound_rx.try_recv() {
            Ok(msg) => match msg {
                Message::Binary(msg) => {
                    self.buffer[..msg.len()].copy_from_slice(&msg);
                    Ok(Some((&mut self.buffer[..msg.len()], self.server_addr)))
                }
                Message::Close => {
                    debug!("Mugon connection with server closed");
                    Ok(None)
                }
            },
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
