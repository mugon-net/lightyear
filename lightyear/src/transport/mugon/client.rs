#![cfg(feature = "mugon")]

use crate::client::io::transport::{ClientTransportBuilder, ClientTransportEnum};
use crate::client::io::{ClientIoEvent, ClientIoEventReceiver, ClientNetworkEventSender};
use crate::transport::error::Error::NotConnected;
use crate::transport::error::{Error, Result as LightyearResult};
use crate::transport::io::IoState;
use crate::transport::mugon::common::socket_addr_to_id;
use crate::transport::{
    BoxedReceiver, BoxedSender, PacketReceiver, PacketSender, Transport, LOCAL_SOCKET, MTU,
};
use async_compat::Compat;
use bevy::tasks::IoTaskPool;
use js_sys::{Promise, Uint8Array};
use serde_wasm_bindgen::from_value;
use std::net::SocketAddr;
use std::rc::Rc;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot::{Receiver, Sender};
use tracing::{debug, info, warn};
use wasm_bindgen::prelude::{wasm_bindgen, Closure};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    // TODO Also disconnect / status callback?
    #[wasm_bindgen(js_namespace = window, js_name = connectAndRegisterCallbacks)]
    fn connect_and_register_callbacks(connected_callback: &JsValue, receive_callback: &JsValue);

    #[wasm_bindgen(js_namespace = window, js_name = sendFromMugonSocket)]
    fn send(to_id: u64, value: &[u8]) -> bool;

    #[wasm_bindgen(js_namespace = window, js_name = closeMugonSocket)]
    fn close(id: u64);
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
        let (from_server_sender, from_server_receiver) = mpsc::unbounded_channel::<Vec<u8>>();

        // channel used to cancel the io task and check if it was cancelled
        let (close_tx, close_rx) = async_channel::bounded(1);
        let close_rx_for_send_task = close_rx.clone();

        // channel used to send/check the status of the io task
        let (status_tx_from_connect_task, status_rx) = async_channel::bounded(1);
        let status_tx_from_send_task = status_tx_from_connect_task.clone();

        // channel used to signal from the connect task to the send task, if the connection init was successful
        let (send_connected_event, recv_connected_event) = tokio::sync::oneshot::channel::<bool>();

        let local_id = socket_addr_to_id(&self.local_addr);
        let server_id = socket_addr_to_id(&self.server_addr);

        let connected_callback = Closure::wrap(Box::new(move |success: bool| async {
            if success {
                status_tx_from_connect_task
                    .send(ClientIoEvent::Connected)
                    .await
                    .unwrap();
                send_connected_event.send(true).unwrap();
            } else {
                status_tx_from_connect_task
                    .send(ClientIoEvent::Disconnected(NotConnected))
                    .await
                    .unwrap();
                send_connected_event.send(false).unwrap();
            }
        }));
        let receive_callback = Closure::wrap(Box::new(move |_: u64, data: Vec<u8>| {
            let _ = from_server_sender.send(data);
        })) as Box<dyn FnMut(u64, Vec<u8>)>;

        connect_and_register_callbacks(
            &connected_callback.as_ref().unchecked_ref(),
            &receive_callback.as_ref().unchecked_ref(),
        );

        // Leaking closures to js, so they continue to function after connect call has returned
        connected_callback.forget();
        receive_callback.forget();

        // Task for sending outgoing packets
        wasm_bindgen_futures::spawn_local(async move {
            tokio::select! {
                Ok(success) = recv_connected_event => {
                    if success {
                        debug!("Starting mugon client send task");
                    } else {
                        debug!("Stopping mugon receive task. Reason: Mugon client failed to connect");
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
                    }
            }
            loop {
                debug!("Client waiting for send");
                tokio::select! {
                    Ok(event) = close_rx_for_send_task.recv() => {
                        match event {
                            ClientIoEvent::Disconnected(e) => {
                                debug!("Stopping mugon client send task. Reason: {:?}", e);
                                return;
                            }
                            _ => {}
                        }
                    },
                    recv = to_server_receiver.recv() => {
                        if let Some(msg) = recv {
                            // info!("Client sending to id {} message: {:?}", server_id, msg);
                            debug!("Client sending");
                            if !send(server_id, msg.as_slice()) {
                                let _ = status_tx_from_send_task.send(ClientIoEvent::Disconnected(std::io::Error::other("mugon connection was lost").into())).await;
                                return;
                            }
                            debug!("Client sent");
                            drop(msg);
                        } else {
                            return;
                        }
                    }
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
    fn send(&mut self, payload: &[u8], address: &SocketAddr) -> LightyearResult<()> {
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
    fn recv(&mut self) -> LightyearResult<Option<(&mut [u8], SocketAddr)>> {
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
