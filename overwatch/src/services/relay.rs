// std
use std::any::Any;
use std::fmt::Debug;
// crates
use thiserror::Error;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::oneshot;
use tracing::{error, instrument};
// internal
use crate::overwatch::commands::{OverwatchCommand, RelayCommand, ReplyChannel};
use crate::overwatch::handle::OverwatchHandle;
use crate::services::{ServiceCore, ServiceId};

#[derive(Error, Debug)]
pub enum RelayError {
    #[error("error requesting relay to {to} service")]
    InvalidRequest { to: ServiceId },
    #[error("couldn't relay message")]
    Send,
    #[error("relay is already connected")]
    AlreadyConnected,
    #[error("service relay is disconnected")]
    Disconnected,
    #[error("service {service_id} is not available")]
    Unavailable { service_id: ServiceId },
    #[error("invalid message with type id [{type_id}] for service {service_id}")]
    InvalidMessage {
        type_id: String,
        service_id: &'static str,
    },
    #[error("receiver failed due to {0:?}")]
    Receiver(Box<dyn Debug + Send + Sync>),
}

/// Message wrapper type
pub type AnyMessage = Box<dyn Any + Send + 'static>;

#[derive(Debug, Clone)]
pub struct NoMessage;

impl RelayMessage for NoMessage {}

/// Result type when creating a relay connection
pub type RelayResult = Result<AnyMessage, RelayError>;

/// Marker type for relay messages
/// Notice that it is bound to 'static.
pub trait RelayMessage: 'static {}

enum RelayState<M> {
    Disconnected,
    Connected(OutboundRelay<M>),
}

impl<M> Clone for RelayState<M> {
    fn clone(&self) -> Self {
        match self {
            RelayState::Disconnected => RelayState::Disconnected,
            RelayState::Connected(outbound) => RelayState::Connected(outbound.clone()),
        }
    }
}

/// Channel receiver of a relay connection
pub struct InboundRelay<M> {
    receiver: Receiver<M>,
    _stats: (), // placeholder
}

/// Channel sender of a relay connection
pub struct OutboundRelay<M> {
    sender: Sender<M>,
    _stats: (), // placeholder
}

pub struct Relay<S: ServiceCore> {
    state: RelayState<S::Message>,
    overwatch_handle: OverwatchHandle,
}

impl<S: ServiceCore> Clone for Relay<S> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            overwatch_handle: self.overwatch_handle.clone(),
        }
    }
}

// TODO: make buffer_size const?
/// Relay channel builder
pub fn relay<M>(buffer_size: usize) -> (InboundRelay<M>, OutboundRelay<M>) {
    let (sender, receiver) = channel(buffer_size);
    (
        InboundRelay {
            receiver,
            _stats: (),
        },
        OutboundRelay { sender, _stats: () },
    )
}

impl<M> InboundRelay<M> {
    /// Receive a message from the relay connections
    pub async fn recv(&mut self) -> Option<M> {
        self.receiver.recv().await
    }
}

impl<M> OutboundRelay<M> {
    /// Send a message to the relay connection
    pub async fn send(&mut self, message: M) -> Result<(), (RelayError, M)> {
        self.sender
            .send(message)
            .await
            .map_err(|e| (RelayError::Send, e.0))
    }
}

impl<M> Clone for OutboundRelay<M> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            _stats: (),
        }
    }
}

impl<S: ServiceCore> Relay<S> {
    pub fn new(overwatch_handle: OverwatchHandle) -> Self {
        Self {
            state: RelayState::Disconnected,
            overwatch_handle,
        }
    }

    #[instrument(skip(self), err(Debug))]
    pub async fn connect(&mut self) -> Result<(), RelayError> {
        if let RelayState::Disconnected = self.state {
            let (reply, receiver) = oneshot::channel();
            self.request_relay(reply).await;
            self.handle_relay_response(receiver).await
        } else {
            Err(RelayError::AlreadyConnected)
        }
    }

    #[instrument(skip(self), err(Debug))]
    pub fn disconnect(&mut self) -> Result<(), RelayError> {
        self.state = RelayState::Disconnected;
        Ok(())
    }

    #[instrument(skip_all, err(Debug))]
    pub async fn send(&mut self, message: S::Message) -> Result<(), RelayError> {
        // TODO: we could make a retry system and/or add timeouts
        if let RelayState::Connected(outbound_relay) = &mut self.state {
            outbound_relay
                .send(message)
                .await
                .map_err(|(e, _message)| e)
        } else {
            Err(RelayError::Disconnected)
        }
    }

    async fn request_relay(&mut self, reply: oneshot::Sender<RelayResult>) {
        let relay_command = OverwatchCommand::Relay(RelayCommand {
            service_id: S::SERVICE_ID,
            reply_channel: ReplyChannel(reply),
        });
        self.overwatch_handle.send(relay_command).await;
    }

    #[instrument(skip_all, err(Debug))]
    async fn handle_relay_response(
        &mut self,
        receiver: oneshot::Receiver<RelayResult>,
    ) -> Result<(), RelayError> {
        let response = receiver.await;
        match response {
            Ok(Ok(message)) => match message.downcast::<OutboundRelay<S::Message>>() {
                Ok(channel) => {
                    self.state = RelayState::Connected(*channel);
                    Ok(())
                }
                Err(m) => Err(RelayError::InvalidMessage {
                    type_id: format!("{:?}", m.type_id()),
                    service_id: S::SERVICE_ID,
                }),
            },
            Ok(Err(e)) => Err(e),
            Err(e) => Err(RelayError::Receiver(Box::new(e))),
        }
    }
}
