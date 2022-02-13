use itertools::Itertools;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::io::{Error, Write};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex};
use stunclient::StunClient;

use std::time::Duration;

use crate::processor::AudioChunk;

#[derive(Serialize, Deserialize, Debug)]
pub enum ProtocolMessage {
    AudioChunk(AudioChunk),
    IdentityDeclaration(PeerIdentity),
    PeerDiscovery(Vec<PeerIdentity>),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PeerIdentity {
    pub canonical_name: String,
    display_name: Option<String>,
    addresses: Vec<String>,
}

impl PeerIdentity {
    pub fn new(canonical_name: &str) -> PeerIdentity {
        PeerIdentity {
            canonical_name: canonical_name.to_string(),
            display_name: None,
            addresses: Vec::new(),
        }
    }
}

pub struct PeerState {
    _identity: PeerIdentity,
    _sockets: HashMap<String, Vec<UdpSocket>>,
}

impl ProtocolMessage {
    pub async fn write_to_stream<W>(&self, mut stream: &mut W) -> Result<(), Error>
    where
        W: Write,
    {
        let serialized = bincode::serialize(self).unwrap();
        if zstd::stream::copy_encode(&serialized[..], &mut stream, 1).is_ok() {}
        Ok(())
    }
    pub async fn read_from_stream(stream: &mut Vec<u8>) -> Result<ProtocolMessage, Vec<u8>> {
        let mut data_buffer = Vec::new();
        if zstd::stream::copy_decode(&stream[..], &mut data_buffer).is_ok() {}
        let protocol_message = bincode::deserialize(&data_buffer[..]).expect("Protocol violation");
        Ok(protocol_message)
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub enum ConnectMessage {
    Ping(String),
    Pong(String),
}

pub struct ConnectionManager {
    pub identity: String,
    pub peers: Arc<Mutex<HashMap<String, PeerState>>>,
    pub addresses: HashSet<SocketAddr>,
    pub client: reqwest::Client,
}

pub fn socket_addr(string: String) -> SocketAddr {
    string
        .to_socket_addrs()
        .expect("Invalid peer address")
        .collect::<Vec<SocketAddr>>()
        .get(0)
        .unwrap()
        .to_owned()
}

impl ConnectionManager {
    pub fn new(own_name: &str, client: reqwest::Client) -> ConnectionManager {
        let peers = Arc::new(Mutex::new(HashMap::new()));

        // manager.add_peer(own_name);
        ConnectionManager {
            identity: own_name.to_string(),
            peers,
            addresses: HashSet::new(),
            client,
        }
    }
    pub fn peer_list(&self) -> Vec<String> {
        let peers_guard = self.peers.lock().unwrap();
        peers_guard.keys().into_iter().cloned().collect_vec()
    }
    pub async fn add_peer(&self, canonical_name: &str) -> Vec<String> {
        let identity = PeerIdentity::new(canonical_name);
        {
            let mut peers_guard = self.peers.lock().unwrap();
            peers_guard.insert(
                canonical_name.to_string(),
                PeerState {
                    _identity: identity.clone(),
                    _sockets: HashMap::new(),
                },
            );
        }
        let url = format!("http://{}/addresses", canonical_name);
        loop {
            match self.client.get(&url).send().await {
                Ok(response) => {
                    let response: Result<Value, reqwest::Error> = response.json().await;
                    match response {
                        Ok(addresses_json) => {
                            let addresses: Vec<String> = addresses_json
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|x| x.as_str().unwrap().to_string())
                                .collect_vec();
                            return addresses;
                        }
                        Err(_e) => {}
                    }
                }
                Err(_e) => {}
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    }
    pub fn create_server_socket(&mut self, port: u16) -> UdpSocket {
        let local_addr: SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();
        let udp = UdpSocket::bind(local_addr).unwrap();
        let stun_client = StunClient::with_google_stun_server();
        let external_addr = stun_client.query_external_address(&udp).unwrap();
        self.addresses.insert(external_addr);
        udp
    }
    fn _establish_socket(&mut self, address: SocketAddr) -> Result<UdpSocket, Error> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_read_timeout(Some(Duration::from_millis(1000)))?;
        let ping_message = ConnectMessage::Ping(self.identity.clone());
        let serialized_ping_message = bincode::serialize(&ping_message).unwrap();
        socket.send_to(&serialized_ping_message[..], address)?;
        let mut buf = Vec::new();
        match socket.recv_from(&mut buf) {
            Ok((_len, _src)) => Ok(socket),
            Err(e) => Err(e),
        }
    }
    // pub fn peer_socket(&mut self, peer_name: String) -> Option<UdpSocket> {
    //     let peer = self.peers.get(&peer_name).unwrap();
    //     for address in peer.identity.addresses.clone() {
    //         match self.establish_socket(socket_addr(address)) {
    //             Ok(socket) => { return Some(socket) },
    //             Err(_) => {},
    //         }
    //     }
    //     None
    // }
}
