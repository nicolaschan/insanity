use bincode::ErrorKind;
use serde::{Deserialize, Serialize};

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::fmt::Display;
use std::io::{Error, Write};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;
use veq::veq::{ConnectionInfo, VeqSessionAlias, VeqSocket};

use crate::clerver::AudioFrame;
use crate::coordinator::AugmentedInfo;
use crate::session::UpdatablePendingSession;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ProtocolMessage {
    AudioFrame(AudioFrame),
    IdentityDeclaration(PeerIdentity),
    PeerDiscovery(Vec<PeerIdentity>),
    ChatMessage(String)
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
        if let Err(e) = std::io::copy(&mut &serialized[..], &mut stream) {
            log::error!("Error writing to stream: {:?}", e);
            return Err(e);
        }
        Ok(())
    }
    pub async fn read_from_stream(stream: &mut &[u8]) -> Result<ProtocolMessage, Box<ErrorKind>> {
        match bincode::deserialize(stream) {
            Ok(protocol_message) => Ok(protocol_message),
            Err(e) => {
                log::error!("Error deserializing protocol message: {:?}", e);
                Err(e)
            }
        }
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
impl Display for OnionAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl FromStr for OnionAddress {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(OnionAddress(s.to_string()))
    }
}

#[derive(Clone)]
pub struct OnionSidechannel {
    client: reqwest::Client,
    peer: OnionAddress,
    session_id: Arc<Mutex<Option<Uuid>>>,
}

impl OnionSidechannel {
    pub fn new(client: reqwest::Client, peer: OnionAddress) -> OnionSidechannel {
        OnionSidechannel {
            client,
            peer,
            session_id: Arc::new(Mutex::new(None)),
        }
    }
    pub async fn id_or_new(&mut self) -> Uuid {
        let mut guard = self.session_id.lock().await;
        match *guard {
            Some(id) => id,
            None => {
                let id = Uuid::new_v4();
                *guard = Some(id);
                id
            }
        }
    }
    pub async fn peer_info(&self) -> Result<AugmentedInfo, reqwest::Error> {
        let url = format!("http://{}/info", self.peer.0);
        let response = self.client.get(&url).send().await?;
        response.json().await
    }
}

pub struct ConnectionManager {
    pub conn_info: ConnectionInfo,
    pub peers: Arc<Mutex<HashMap<OnionAddress, ConnectionInfo>>>,
    pub sidechannels: Arc<Mutex<HashMap<OnionAddress, OnionSidechannel>>>,
    pub addresses: HashSet<SocketAddr>,
    pub client: reqwest::Client,
    pub own_address: OnionAddress,
    pub db: sled::Db,
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
    pub fn new(
        conn_info: ConnectionInfo,
        client: reqwest::Client,
        own_address: OnionAddress,
        db: sled::Db,
    ) -> ConnectionManager {
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
            db,
        }
    }
    pub async fn id_or_new(&self, peer: OnionAddress) -> Option<Uuid> {
        let mut sidechannel = {
            let mut sc_guard = self.sidechannels.lock().await;
            sc_guard.get_mut(&peer)?.clone()
        };
        Some(sidechannel.id_or_new().await)
    }
    pub async fn get_sidechannel(&self, peer: &OnionAddress) -> OnionSidechannel {
        let sidechannel = {
            let mut sc_guard = self.sidechannels.lock().await;
            sc_guard.get_mut(peer).map(|x| x.clone())
        };
        match sidechannel {
            Some(sc) => sc,
            None => {
                let sidechannel = OnionSidechannel::new(self.client.clone(), peer.clone());
                let mut sc_guard = self.sidechannels.lock().await;
                sc_guard.insert(peer.clone(), sidechannel.clone());
                sidechannel
            }
        }
    }
    pub async fn session(
        &self,
        socket: &mut VeqSocket,
        peer: &OnionAddress,
    ) -> Option<(VeqSessionAlias, AugmentedInfo)> {
        let mut sc = self.get_sidechannel(peer).await;
        let id = onion_addresses_to_uuid(&self.own_address, peer);

        let pending_session = UpdatablePendingSession::new(socket.clone());

        if let Ok(Some(cached_info_serialized)) = self.db.get(format!("peer-{}", peer)) {
            let cached_info: AugmentedInfo = bincode::deserialize(&cached_info_serialized[..]).unwrap();
            pending_session.update(id, cached_info).await;
        }

        loop {
            tokio::select! {
                tor_info = wait_for_peer_info(&mut sc) => {
                    pending_session.update(id, tor_info).await;
                }
                (session, info) = pending_session.session() => {
                    if let Err(e) = self.db.insert(format!("peer-{}", peer), bincode::serialize(&info).unwrap()) {
                        log::error!("Failed to cache peer info: {}", e);
                    }
                    return Some((session, info));
                }
            }
        }
    }
}

fn onion_addresses_to_uuid(addr1: &OnionAddress, addr2: &OnionAddress) -> Uuid {
    let lower_str = std::cmp::min(addr1.to_string(), addr2.to_string());
    let higher_str = std::cmp::max(addr1.to_string(), addr2.to_string());

    let mut hasher = Sha256::new();
    hasher.update(lower_str.as_bytes());
    hasher.update(higher_str.as_bytes());
    let result = hasher.finalize();
    let mut dest = [0u8; 16];
    dest.clone_from_slice(&result[0..16]);
    Uuid::from_bytes(dest)
}

async fn wait_for_peer_info(sidechannel: &mut OnionSidechannel) -> AugmentedInfo {
    loop {
        match sidechannel.peer_info().await {
            Ok(info) => return info,
            Err(_e) => {
                // println!("e {:?}", e);
            }
        }
        // tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
