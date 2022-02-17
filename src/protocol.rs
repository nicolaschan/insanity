use serde::{Deserialize, Serialize};
use tokio::join;
use tokio::sync::Mutex;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::io::{Error, Write};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use veq::veq::{ConnectionInfo, VeqSession, VeqSocket, VeqSessionAlias};

use crate::clerver::AudioFrame;

#[derive(Serialize, Deserialize, Debug)]
pub enum ProtocolMessage {
    AudioFrame(AudioFrame),
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

#[derive(PartialEq, Eq, Hash, Clone, Serialize, Deserialize, Debug)]
pub struct OnionAddress(String);
impl OnionAddress {
    pub fn new(str: String) -> Option<OnionAddress> {
        Some(OnionAddress(str))
    }
}
impl FromStr for OnionAddress {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(OnionAddress(s.to_string()))
    }
}

pub struct OnionSidechannel {
    client: reqwest::Client,
    own: OnionAddress,
    peer: OnionAddress,
    session_id: Arc<Mutex<Option<Uuid>>>,
}

impl OnionSidechannel {
    pub fn new(client: reqwest::Client, own: OnionAddress, peer: OnionAddress) -> OnionSidechannel {
        OnionSidechannel { client, own, peer, session_id: Arc::new(Mutex::new(None)) }
    }
    async fn update_id(&mut self, id: Uuid) {
        let mut guard = self.session_id.lock().await;
        if guard.is_none() {
            *guard = Some(id);
        }
    }
    pub async fn id(&mut self) -> Result<Uuid, reqwest::Error> {
        let id = {
            let guard = self.session_id.lock().await;
            *guard
        };
        match id {
            Some(id) => Ok(id),
            None => {
                let url = format!("http://{}/id/{}", self.peer.0, self.own.0);
                let response = self.client.post(&url).send().await?;
                let id: Uuid = response.json().await?;
                self.update_id(id).await;
                Ok(id)
            }
        }
    }
    pub async fn id_or_new(&mut self) -> Uuid {
        let mut guard = self.session_id.lock().await;
        match *guard {
            Some(id) => id,
            None => {
                let id = Uuid::new_v4();
                *guard = Some(id);
                return id;
            }
        }
    }
    pub async fn peer_info(&self) -> Result<ConnectionInfo, reqwest::Error> {
        let url = format!("http://{}/info", self.peer.0);
        let response = self.client.get(&url).send().await?;
        response.json().await
    }
}

pub struct ConnectionManager {
    pub conn_info: ConnectionInfo,
    pub peers: Arc<Mutex<HashMap<OnionAddress, ConnectionInfo>>>,
    pub sidechannels: Arc<Mutex<HashMap<OnionAddress, Arc<Mutex<OnionSidechannel>>>>>,
    pub addresses: HashSet<SocketAddr>,
    pub client: reqwest::Client,
    pub own_address: OnionAddress,
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
    pub fn new(conn_info: ConnectionInfo, client: reqwest::Client, own_address: OnionAddress) -> ConnectionManager {
        let peers = Arc::new(Mutex::new(HashMap::new()));
        // tokio::spawn(async move {
        //     loop {
        //         println!("hi");
        //         tokio::time::sleep(Duration::from_millis(1000)).await;
        //     }
        // });
        ConnectionManager {
            conn_info,
            peers,
            sidechannels: Arc::new(Mutex::new(HashMap::new())),
            addresses: HashSet::new(),
            client,
            own_address,
        }
    }
    pub async fn id_or_new(&self, peer: OnionAddress) -> Option<Uuid> {
        let sidechannel = {
            let mut sc_guard = self.sidechannels.lock().await;
            sc_guard.get_mut(&peer)?.clone()
        };
        let mut guard = sidechannel.lock().await;
        Some(guard.id_or_new().await)
    }
    pub async fn get_sidechannel(&self, peer: &OnionAddress) -> Arc<Mutex<OnionSidechannel>> {
        let sidechannel = {
            let mut sc_guard = self.sidechannels.lock().await;
            sc_guard.get_mut(&peer).map(|x| x.clone())
        };
        println!("sc {:?}", peer);
        match sidechannel {
            Some(sc) => sc,
            None => {
                let sidechannel = Arc::new(Mutex::new(OnionSidechannel::new(self.client.clone(), self.own_address.clone(), peer.clone())));
                let mut sc_guard = self.sidechannels.lock().await;
                sc_guard.insert(peer.clone(), sidechannel.clone());
                sidechannel
            }
        }
    }
    pub async fn session(&self, socket: &mut VeqSocket, peer: &OnionAddress) -> Option<VeqSessionAlias> {
        let sc = self.get_sidechannel(peer).await;
        println!("wat {:?}", peer);
        let (id, info) = {
            let id = {
                let mut inner_id = None;
                loop {
                    println!("id {:?}", peer);
                    let mut guard = sc.lock().await;
                    match guard.id().await {
                        Ok(got_id) => {
                            inner_id = Some(got_id);
                            break;
                        },
                        Err(e) => { println!("e {:?}", e); }
                    };
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                }
                inner_id.unwrap()
            };
            let info = {
                let mut inner_peer_info = None;
                loop {
                    println!("info {:?}", peer);
                    let mut guard = sc.lock().await;
                    match guard.peer_info().await {
                        Ok(got_peer_info) => {
                            inner_peer_info = Some(got_peer_info);
                            break;
                        },
                        Err(e) => { println!("e {:?}", e); }
                    }
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                }
                inner_peer_info.unwrap()
            };
            (id, info) 
        };
        println!("connecting to {:?} with id {:?}", peer, id);
        Some(socket.connect(id, info).await)
    }
}
