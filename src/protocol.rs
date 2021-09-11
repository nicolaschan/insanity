use itertools::Itertools;
use quinn::{ReadToEndError, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stunclient::StunClient;
use std::io::Error;
use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex};

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
    pub fn new(canonical_name: &String) -> PeerIdentity {
        PeerIdentity {
            canonical_name: canonical_name.clone(),
            display_name: None,
            addresses: Vec::new(),
        }
    }
}

pub struct PeerState {
    identity: PeerIdentity,
    sockets: HashMap<String, Vec<UdpSocket>>,
}

impl ProtocolMessage {
    pub async fn write_to_stream(&self, stream: &mut SendStream) -> Result<(), Error> {
        let serialized = bincode::serialize(self).unwrap();
        let mut compressed: Vec<u8> = Vec::new();
        if zstd::stream::copy_encode(&serialized[..], &mut compressed, 1).is_ok() {}
        stream.write_all(&compressed).await?;
        Ok(())
    }
    pub async fn read_from_stream(stream: RecvStream) -> Result<ProtocolMessage, ReadToEndError> {
        let compressed= stream.read_to_end(usize::max_value()).await?;
        let mut serialized = Vec::new();
        if zstd::stream::copy_decode(&compressed[..], &mut serialized).is_ok() {}
        let protocol_message = bincode::deserialize(&serialized[..])
            .expect("Protocol violation");
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
            identity: own_name.to_string().clone(), peers,
            addresses: HashSet::new(),
            client,
        }
    }
    pub fn peer_list(&self) -> Vec<String> {
        let peers_guard = self.peers.lock().unwrap();
        peers_guard.keys().into_iter().cloned().collect_vec()
    }
    pub async fn add_peer(&self, canonical_name: &String) -> Vec<String> {
        let identity = PeerIdentity::new(canonical_name);
        let mut peers_guard = self.peers.lock().unwrap();
        peers_guard.insert(canonical_name.clone(), PeerState { 
            identity: identity.clone(), 
            sockets: HashMap::new(),
        });
        drop(peers_guard);
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
                        },
                        Err(_e) => {
                        }
                    }
                },
                Err(_e) => {
                }
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
    fn establish_socket(&mut self, address: SocketAddr) -> Result<UdpSocket, Error> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_read_timeout(Some(Duration::from_millis(1000)));
        let ping_message = ConnectMessage::Ping(self.identity.clone());
        let serialized_ping_message = bincode::serialize(&ping_message).unwrap();
        socket.send_to(&serialized_ping_message[..], address);
        let mut buf = Vec::new();
        match socket.recv_from(&mut buf) {
            Ok((_len, _src)) => {
                Ok(socket)
            },
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