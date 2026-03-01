use bincode::Options;
use n3ds_remote_play_common::InputState;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::select;
use tracing::{debug, error, info, trace, warn};

/// The input mapper receives input state packets over UDP and sends them to the
/// correct remote play connection task for further processing.
///
/// Because UDP is connectionless, the input mapper keeps track of the mapping
/// between client address and remote play connection task and forwards packets
/// accordingly.
pub struct InputMapper {
    udp_socket: Arc<UdpSocket>,
    udp_buffer: Vec<u8>,
    input_map: HashMap<SocketAddr, tokio::sync::watch::Sender<InputState>>,
    msg_sender: tokio::sync::mpsc::Sender<InputMapperMessage>,
    msg_receiver: tokio::sync::mpsc::Receiver<InputMapperMessage>,
    remove_sender: tokio::sync::mpsc::UnboundedSender<SocketAddr>,
    remove_receiver: tokio::sync::mpsc::UnboundedReceiver<SocketAddr>,
}

enum InputMapperMessage {
    Register {
        addr: SocketAddr,
        result_sender: tokio::sync::oneshot::Sender<
            anyhow::Result<(tokio::sync::watch::Receiver<InputState>, InputMapperGuard)>,
        >,
    },
    Shutdown,
}

/// A handle to the input mapper task that allows registering new clients and
/// shutting down the task.
#[derive(Clone)]
pub struct InputMapperHandle {
    msg_sender: tokio::sync::mpsc::Sender<InputMapperMessage>,
}

/// A guard that keeps the client registered in the input mapper as long as it is
/// alive.
pub struct InputMapperGuard {
    socket_addr: SocketAddr,
    remove_sender: tokio::sync::mpsc::UnboundedSender<SocketAddr>,
}

impl Drop for InputMapperGuard {
    fn drop(&mut self) {
        self.remove_sender.send(self.socket_addr).ok();
    }
}

impl InputMapper {
    pub fn new(udp_socket: Arc<UdpSocket>) -> Self {
        let buffer = vec![0; 1024];
        let (msg_sender, msg_receiver) = tokio::sync::mpsc::channel(8);
        let (remove_sender, remove_receiver) = tokio::sync::mpsc::unbounded_channel();

        Self {
            udp_socket,
            udp_buffer: buffer,
            input_map: HashMap::new(),
            msg_sender,
            msg_receiver,
            remove_sender,
            remove_receiver,
        }
    }

    /// Create a handle to the input mapper that can be used to send messages to it.
    pub fn handle(&self) -> InputMapperHandle {
        InputMapperHandle {
            msg_sender: self.msg_sender.clone(),
        }
    }

    /// Run the input mapper task. This will run until a shutdown message is received.
    pub async fn run(mut self) {
        loop {
            select! {
                biased;

                Some(addr) = self.remove_receiver.recv() => self.remove_input(addr),

                Some(msg) = self.msg_receiver.recv() => match msg {
                    InputMapperMessage::Register { addr, result_sender } => {
                        let result = self.register_input(addr).await;
                        if result_sender.send(result).is_err() {
                            warn!("Register result receiver dropped for [{addr}]");
                        }
                    },
                    InputMapperMessage::Shutdown => {
                        debug!("Input mapper task exiting");
                        break;
                    },
                },

                packet_result = self.udp_socket.recv_from(&mut self.udp_buffer) => self.handle_packet(packet_result),
            }
        }

        info!("Input mapper task exited");
    }

    async fn register_input(
        &mut self,
        addr: SocketAddr,
    ) -> anyhow::Result<(tokio::sync::watch::Receiver<InputState>, InputMapperGuard)> {
        if self.input_map.contains_key(&addr) {
            anyhow::bail!("Address [{addr}] is already registered");
        }

        let (input_sender, input_receiver) = tokio::sync::watch::channel(InputState::default());
        self.input_map.insert(addr, input_sender);
        debug!("Registered input for [{addr}]");

        let guard = InputMapperGuard {
            socket_addr: addr,
            remove_sender: self.remove_sender.clone(),
        };
        Ok((input_receiver, guard))
    }

    fn remove_input(&mut self, addr: SocketAddr) {
        if self.input_map.remove(&addr).is_some() {
            debug!("Removed input for [{addr}]");
        } else {
            warn!("Attempted to remove non-existent input for [{addr}]");
        }
    }

    fn handle_packet(&mut self, packet_result: std::io::Result<(usize, SocketAddr)>) {
        let (size, src_addr) = match packet_result {
            Ok(p) => p,
            Err(e) => {
                error!("Error while receiving UDP packet: {e}");
                return;
            }
        };
        trace!("Received UDP input packet of size {size} from [{src_addr}]");

        // Look up the input channel for this client
        let Some(sender) = self.input_map.get(&src_addr) else {
            warn!("Received UDP input packet from unknown address [{src_addr}]");
            return;
        };

        // Deserialize the input state
        let bincode_options = bincode::DefaultOptions::new();
        let input_state = match bincode_options.deserialize::<InputState>(&self.udp_buffer[..size])
        {
            Ok(state) => state,
            Err(e) => {
                error!("Error while deserializing input state from [{src_addr}]: {e}");
                return;
            }
        };

        // Send the input state to the corresponding client handler
        if sender.send(input_state).is_err() {
            error!("The receiver for [{src_addr}] was dropped, removing from input map");
            self.remove_input(src_addr);
        }
    }
}

impl InputMapperHandle {
    /// Register a new input channel.
    /// Returns the input state receiver and a guard that will automatically unregister the channel when dropped.
    pub async fn register_input(
        &self,
        addr: SocketAddr,
    ) -> anyhow::Result<(tokio::sync::watch::Receiver<InputState>, InputMapperGuard)> {
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        let message = InputMapperMessage::Register {
            addr,
            result_sender,
        };

        if let Err(e) = self.msg_sender.send(message).await {
            anyhow::bail!("Failed to register for input packets: {e}");
        }

        match result_receiver.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(e)) => {
                anyhow::bail!("Failed to create input mapping: {e}");
            }
            Err(e) => {
                anyhow::bail!("Failed to receive input guard: {e}");
            }
        }
    }

    /// Send a shutdown message to the input mapper task, causing it to exit.
    pub async fn shutdown(&self) {
        self.msg_sender
            .send(InputMapperMessage::Shutdown)
            .await
            .ok();
    }
}
