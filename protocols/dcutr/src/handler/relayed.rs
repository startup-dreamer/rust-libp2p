// Copyright 2021 Protocol Labs.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! [`ConnectionHandler`] handling relayed connection potentially upgraded to a direct connection.

use crate::behaviour_impl::MAX_NUMBER_OF_UPGRADE_ATTEMPTS;
use crate::protocol;
use either::Either;
use futures::future;
use futures::future::{BoxFuture, FutureExt};
use libp2p_core::multiaddr::Multiaddr;
use libp2p_core::upgrade::DeniedUpgrade;
use libp2p_core::ConnectedPoint;
use libp2p_swarm::handler::{
    ConnectionEvent, DialUpgradeError, FullyNegotiatedInbound, FullyNegotiatedOutbound,
    ListenUpgradeError,
};
use libp2p_swarm::{
    ConnectionHandler, ConnectionHandlerEvent, KeepAlive, StreamUpgradeError, SubstreamProtocol,
};
use std::collections::VecDeque;
use std::task::{Context, Poll};

#[derive(Debug)]
pub enum Command {
    Connect,
}

#[derive(Debug)]
pub enum Event {
    InboundConnectRequest {
        remote_addr: Multiaddr,
    },
    InboundNegotiationFailed {
        error: StreamUpgradeError<void::Void>,
    },
    InboundConnectNegotiated(Vec<Multiaddr>),
    OutboundNegotiationFailed {
        error: StreamUpgradeError<void::Void>,
    },
    OutboundConnectNegotiated {
        remote_addrs: Vec<Multiaddr>,
    },
}

pub struct Handler {
    endpoint: ConnectedPoint,
    /// A pending fatal error that results in the connection being closed.
    pending_error: Option<
        StreamUpgradeError<
            Either<protocol::inbound::UpgradeError, protocol::outbound::UpgradeError>,
        >,
    >,
    /// Queue of events to return when polled.
    queued_events: VecDeque<
        ConnectionHandlerEvent<
            <Self as ConnectionHandler>::OutboundProtocol,
            <Self as ConnectionHandler>::OutboundOpenInfo,
            <Self as ConnectionHandler>::ToBehaviour,
            <Self as ConnectionHandler>::Error,
        >,
    >,
    /// Inbound connect, accepted by the behaviour, pending completion.
    inbound_connect:
        Option<BoxFuture<'static, Result<Vec<Multiaddr>, protocol::inbound::UpgradeError>>>,

    /// The addresses we will send to the other party for hole-punching attempts.
    holepunch_candidates: Vec<Multiaddr>,

    attempts: u8,
}

impl Handler {
    pub fn new(endpoint: ConnectedPoint, holepunch_candidates: Vec<Multiaddr>) -> Self {
        Self {
            endpoint,
            pending_error: Default::default(),
            queued_events: Default::default(),
            inbound_connect: Default::default(),
            holepunch_candidates,
            attempts: 0,
        }
    }

    fn on_fully_negotiated_inbound(
        &mut self,
        FullyNegotiatedInbound {
            protocol: output, ..
        }: FullyNegotiatedInbound<
            <Self as ConnectionHandler>::InboundProtocol,
            <Self as ConnectionHandler>::InboundOpenInfo,
        >,
    ) {
        match output {
            future::Either::Left(inbound_connect) => {
                if self
                    .inbound_connect
                    .replace(
                        inbound_connect
                            .accept(self.holepunch_candidates.clone())
                            .boxed(),
                    )
                    .is_some()
                {
                    log::warn!(
                        "New inbound connect stream while still upgrading previous one. \
                         Replacing previous with new.",
                    );
                }
                let remote_addr = match &self.endpoint {
                    ConnectedPoint::Dialer { address, role_override: _ } => address.clone(),
                    ConnectedPoint::Listener { ..} => unreachable!("`<Handler as ConnectionHandler>::listen_protocol` denies all incoming substreams as a listener."),
                };
                self.queued_events
                    .push_back(ConnectionHandlerEvent::NotifyBehaviour(
                        Event::InboundConnectRequest { remote_addr },
                    ));
                self.attempts += 1;
            }
            // A connection listener denies all incoming substreams, thus none can ever be fully negotiated.
            future::Either::Right(output) => void::unreachable(output),
        }
    }

    fn on_fully_negotiated_outbound(
        &mut self,
        FullyNegotiatedOutbound {
            protocol: protocol::outbound::Connect { obs_addrs },
            ..
        }: FullyNegotiatedOutbound<
            <Self as ConnectionHandler>::OutboundProtocol,
            <Self as ConnectionHandler>::OutboundOpenInfo,
        >,
    ) {
        assert!(
            self.endpoint.is_listener(),
            "A connection dialer never initiates a connection upgrade."
        );
        self.queued_events
            .push_back(ConnectionHandlerEvent::NotifyBehaviour(
                Event::OutboundConnectNegotiated {
                    remote_addrs: obs_addrs,
                },
            ));
    }

    fn on_listen_upgrade_error(
        &mut self,
        ListenUpgradeError { error, .. }: ListenUpgradeError<
            <Self as ConnectionHandler>::InboundOpenInfo,
            <Self as ConnectionHandler>::InboundProtocol,
        >,
    ) {
        self.pending_error = Some(StreamUpgradeError::Apply(match error {
            Either::Left(e) => Either::Left(e),
            Either::Right(v) => void::unreachable(v),
        }));
    }

    fn on_dial_upgrade_error(
        &mut self,
        DialUpgradeError { error, .. }: DialUpgradeError<
            <Self as ConnectionHandler>::OutboundOpenInfo,
            <Self as ConnectionHandler>::OutboundProtocol,
        >,
    ) {
        match error {
            StreamUpgradeError::Timeout => {
                self.queued_events
                    .push_back(ConnectionHandlerEvent::NotifyBehaviour(
                        Event::OutboundNegotiationFailed {
                            error: StreamUpgradeError::Timeout,
                        },
                    ));
            }
            StreamUpgradeError::NegotiationFailed => {
                // The remote merely doesn't support the DCUtR protocol.
                // This is no reason to close the connection, which may
                // successfully communicate with other protocols already.
                self.queued_events
                    .push_back(ConnectionHandlerEvent::NotifyBehaviour(
                        Event::OutboundNegotiationFailed {
                            error: StreamUpgradeError::NegotiationFailed,
                        },
                    ));
            }
            _ => {
                // Anything else is considered a fatal error or misbehaviour of
                // the remote peer and results in closing the connection.
                self.pending_error = Some(error.map_upgrade_err(Either::Right));
            }
        }
    }
}

impl ConnectionHandler for Handler {
    type FromBehaviour = Command;
    type ToBehaviour = Event;
    type Error = StreamUpgradeError<
        Either<protocol::inbound::UpgradeError, protocol::outbound::UpgradeError>,
    >;
    type InboundProtocol = Either<protocol::inbound::Upgrade, DeniedUpgrade>;
    type OutboundProtocol = protocol::outbound::Upgrade;
    type OutboundOpenInfo = ();
    type InboundOpenInfo = ();

    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
        match self.endpoint {
            ConnectedPoint::Dialer { .. } => {
                SubstreamProtocol::new(Either::Left(protocol::inbound::Upgrade {}), ())
            }
            ConnectedPoint::Listener { .. } => {
                // By the protocol specification the listening side of a relayed connection
                // initiates the _direct connection upgrade_. In other words the listening side of
                // the relayed connection opens a substream to the dialing side. (Connection roles
                // and substream roles are reversed.) The listening side on a relayed connection
                // never expects incoming substreams, hence the denied upgrade below.
                SubstreamProtocol::new(Either::Right(DeniedUpgrade), ())
            }
        }
    }

    fn on_behaviour_event(&mut self, event: Self::FromBehaviour) {
        match event {
            Command::Connect => {
                self.queued_events
                    .push_back(ConnectionHandlerEvent::OutboundSubstreamRequest {
                        protocol: SubstreamProtocol::new(
                            protocol::outbound::Upgrade::new(self.holepunch_candidates.clone()),
                            (),
                        ),
                    });
                self.attempts += 1;
            }
        }
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        if !self.queued_events.is_empty() {
            return KeepAlive::Yes;
        }

        if self.inbound_connect.is_some() {
            return KeepAlive::Yes;
        }

        if self.attempts < MAX_NUMBER_OF_UPGRADE_ATTEMPTS {
            return KeepAlive::Yes;
        }

        KeepAlive::No
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<
        ConnectionHandlerEvent<
            Self::OutboundProtocol,
            Self::OutboundOpenInfo,
            Self::ToBehaviour,
            Self::Error,
        >,
    > {
        // Check for a pending (fatal) error.
        if let Some(err) = self.pending_error.take() {
            // The handler will not be polled again by the `Swarm`.
            return Poll::Ready(ConnectionHandlerEvent::Close(err));
        }

        // Return queued events.
        if let Some(event) = self.queued_events.pop_front() {
            return Poll::Ready(event);
        }

        if let Some(Poll::Ready(result)) = self.inbound_connect.as_mut().map(|f| f.poll_unpin(cx)) {
            self.inbound_connect = None;
            match result {
                Ok(addresses) => {
                    return Poll::Ready(ConnectionHandlerEvent::NotifyBehaviour(
                        Event::InboundConnectNegotiated(addresses),
                    ));
                }
                Err(e) => {
                    return Poll::Ready(ConnectionHandlerEvent::Close(StreamUpgradeError::Apply(
                        Either::Left(e),
                    )))
                }
            }
        }

        Poll::Pending
    }

    fn on_connection_event(
        &mut self,
        event: ConnectionEvent<
            Self::InboundProtocol,
            Self::OutboundProtocol,
            Self::InboundOpenInfo,
            Self::OutboundOpenInfo,
        >,
    ) {
        match event {
            ConnectionEvent::FullyNegotiatedInbound(fully_negotiated_inbound) => {
                self.on_fully_negotiated_inbound(fully_negotiated_inbound)
            }
            ConnectionEvent::FullyNegotiatedOutbound(fully_negotiated_outbound) => {
                self.on_fully_negotiated_outbound(fully_negotiated_outbound)
            }
            ConnectionEvent::ListenUpgradeError(listen_upgrade_error) => {
                self.on_listen_upgrade_error(listen_upgrade_error)
            }
            ConnectionEvent::DialUpgradeError(dial_upgrade_error) => {
                self.on_dial_upgrade_error(dial_upgrade_error)
            }
            ConnectionEvent::AddressChange(_)
            | ConnectionEvent::LocalProtocolsChange(_)
            | ConnectionEvent::RemoteProtocolsChange(_) => {}
        }
    }
}
