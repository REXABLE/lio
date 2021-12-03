// Copyright (C) 2019-2021 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use snarkos::{Client, Data, Environment, Message};
use snarkos_snode::{ClientState, SynthNode};
use snarkos_storage::BlockLocators;
use snarkvm::{dpc::testnet2::Testnet2, traits::Network};

use pea2pea::{
    protocols::{Disconnect, Handshake, Reading, Writing},
    Config,
    Node as Pea2PeaNode,
    Pea2Pea,
};
use std::{
    convert::TryInto,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ops::Deref,
    time::Duration,
};
use tokio::task;
use tracing::*;

// Consts & aliases.
const MESSAGE_LENGTH_PREFIX_SIZE: usize = 4;
const PING_INTERVAL_SECS: u64 = 5;
const PEER_INTERVAL_SECS: u64 = 3;
const DESIRED_CONNECTIONS: usize = <Client<Testnet2>>::MINIMUM_NUMBER_OF_PEERS * 3;
const MESSAGE_VERSION: u32 = <Client<Testnet2>>::MESSAGE_VERSION;
const MAXIMUM_FORK_DEPTH: u32 = Testnet2::ALEO_MAXIMUM_FORK_DEPTH;

pub const MAXIMUM_NUMBER_OF_PEERS: usize = <Client<Testnet2>>::MAXIMUM_NUMBER_OF_PEERS;

type ClientMessage = Message<Testnet2, Client<Testnet2>>;
pub type ClientNonce = u64;

#[derive(Clone)]
pub struct TestNode(SynthNode);

impl Pea2Pea for TestNode {
    fn node(&self) -> &Pea2PeaNode {
        &self.0.node()
    }
}

impl Deref for TestNode {
    type Target = SynthNode;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl TestNode {
    /// Creates a default test node with the most basic network protocols enabled.
    pub async fn default() -> Self {
        let config = Config {
            listener_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            max_connections: MAXIMUM_NUMBER_OF_PEERS as u16,
            ..Default::default()
        };

        let pea2pea_node = Pea2PeaNode::new(Some(config)).await.unwrap();
        let client_state = Default::default();
        let node = TestNode(SynthNode::new(pea2pea_node, client_state));
        node.enable_disconnect();
        node.enable_handshake();
        node.enable_reading();
        node.enable_writing();
        node
    }

    /// Creates a test node using the given `Pea2Pea` node.
    pub fn new(node: Pea2PeaNode, state: ClientState) -> Self {
        TestNode(SynthNode::new(node, state))
    }

    /// Spawns a task dedicated to broadcasting Ping messages.
    pub fn send_pings(&self) {
        let node = self.clone();
        task::spawn(async move {
            let genesis = Testnet2::genesis_block();
            let ping_msg = ClientMessage::Ping(
                MESSAGE_VERSION,
                MAXIMUM_FORK_DEPTH,
                node.node_type(),
                node.state(),
                genesis.hash(),
                Data::Object(genesis.header().clone()),
            );

            loop {
                if node.node().num_connected() != 0 {
                    info!(parent: node.node().span(), "sending out Pings");
                    node.send_broadcast(ping_msg.clone());
                }
                tokio::time::sleep(Duration::from_secs(PING_INTERVAL_SECS)).await;
            }
        });
    }

    /// Spawns a task dedicated to peer maintenance.
    pub fn update_peers(&self) {
        let node = self.clone();
        task::spawn(async move {
            loop {
                let num_connections = node.node().num_connected() + node.node().num_connecting();
                if num_connections < DESIRED_CONNECTIONS && node.node().num_connected() != 0 {
                    info!(parent: node.node().span(), "I'd like to have {} more peers; asking peers for their peers", DESIRED_CONNECTIONS - num_connections);
                    node.send_broadcast(ClientMessage::PeerRequest);
                }
                tokio::time::sleep(Duration::from_secs(PEER_INTERVAL_SECS)).await;
            }
        });
    }

    /// Starts the usual periodic activities of a test node.
    pub fn run_periodic_tasks(&self) {
        self.send_pings();
        self.update_peers();
    }
}

/// Inbound message processing logic for the test nodes.
#[async_trait::async_trait]
impl Reading for TestNode {
    type Message = ClientMessage;

    fn read_message<R: io::Read>(&self, source: SocketAddr, reader: &mut R) -> io::Result<Option<Self::Message>> {
        // FIXME: use the maximum message size allowed by the protocol or (better) use streaming deserialization.
        let mut buf = [0u8; 8 * 1024];

        reader.read_exact(&mut buf[..MESSAGE_LENGTH_PREFIX_SIZE])?;
        let len = u32::from_le_bytes(buf[..MESSAGE_LENGTH_PREFIX_SIZE].try_into().unwrap()) as usize;

        if reader.read_exact(&mut buf[..len]).is_err() {
            return Ok(None);
        }

        match ClientMessage::deserialize(&buf[..len]) {
            Ok(msg) => {
                info!(parent: self.node().span(), "received a {} from {}", msg.name(), source);
                Ok(Some(msg))
            }
            Err(e) => {
                error!("a message from {} failed to deserialize: {}", source, e);
                Err(io::ErrorKind::InvalidData.into())
            }
        }
    }

    async fn process_message(&self, source: SocketAddr, message: Self::Message) -> io::Result<()> {
        match message {
            ClientMessage::BlockRequest(_start_block_height, _end_block_height) => {}
            ClientMessage::BlockResponse(_block) => {}
            ClientMessage::Disconnect => {}
            ClientMessage::PeerRequest => self.process_peer_request(source).await?,
            ClientMessage::PeerResponse(peer_ips) => self.process_peer_response(source, peer_ips).await?,
            ClientMessage::Ping(version, _fork_depth, _peer_type, _peer_state, _block_hash, block_header) => {
                // Deserialise the block header.
                let block_header = block_header.deserialize().await.unwrap();
                self.process_ping(source, version, block_header.height()).await?
            }
            ClientMessage::Pong(_is_fork, _block_locators) => {}
            ClientMessage::UnconfirmedBlock(_block_height, _block_hash, _block) => {}
            ClientMessage::UnconfirmedTransaction(_transaction) => {}
            _ => return Err(io::ErrorKind::InvalidData.into()), // Peer is not following the protocol.
        }

        Ok(())
    }
}

// Helper methods.
impl TestNode {
    async fn process_peer_request(&self, source: SocketAddr) -> io::Result<()> {
        let peers = self
            .state
            .peers
            .lock()
            .await
            .iter()
            .map(|peer| peer.listening_addr)
            .collect::<Vec<_>>();
        let msg = ClientMessage::PeerResponse(peers);
        info!(parent: self.node().span(), "sending a PeerResponse to {}", source);

        self.send_direct_message(source, msg)
    }

    async fn process_peer_response(&self, source: SocketAddr, peer_ips: Vec<SocketAddr>) -> io::Result<()> {
        let num_connections = self.node().num_connected() + self.node().num_connecting();
        let node = self.clone();
        task::spawn(async move {
            for peer_ip in peer_ips
                .into_iter()
                .filter(|addr| node.node().listening_addr().unwrap() != *addr)
                .take(DESIRED_CONNECTIONS.saturating_sub(num_connections))
            {
                if !node.node().is_connected(peer_ip) && !node.state.peers.lock().await.iter().any(|peer| peer.listening_addr == peer_ip) {
                    info!(parent: node.node().span(), "trying to connect to {}'s peer {}", source, peer_ip);
                    let _ = node.node().connect(peer_ip).await;
                }
            }
        });

        Ok(())
    }

    async fn process_ping(&self, source: SocketAddr, version: u32, block_height: u32) -> io::Result<()> {
        // Ensure the message protocol version is not outdated.
        if version < <Client<Testnet2>>::MESSAGE_VERSION {
            warn!(parent: self.node().span(), "dropping {} due to outdated version ({})", source, version);
            return Err(io::ErrorKind::InvalidData.into());
        }

        debug!(parent: self.node().span(), "peer {} is at height {}", source, block_height);

        let genesis = Testnet2::genesis_block();
        let msg = ClientMessage::Pong(
            None,
            Data::Object(BlockLocators::<Testnet2>::from(vec![(genesis.height(), (genesis.hash(), None))].into_iter().collect()).unwrap()),
        );

        info!(parent: self.node().span(), "sending a Pong to {}", source);

        self.send_direct_message(source, msg)
    }
}
