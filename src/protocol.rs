use futures_util::{FutureExt, pin_mut};
use serde::{Deserialize, Serialize};
use tokio::select;
use tokio::task::JoinHandle;

use std::collections::{HashMap, HashSet};
use std::convert::{Infallible};
use std::fmt::Display;
use std::io::{Error, Write};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use uuid::Uuid;
use veq::veq::{ConnectionInfo, VeqSessionAlias, VeqSocket};
use sha2::{Digest, Sha256};

use crate::clerver::AudioFrame;
use crate::coordinator::AugmentedInfo;

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
    pub async fn read_from_stream(stream: &mut [u8]) -> Result<ProtocolMessage, Vec<u8>> {
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
    own: OnionAddress,
    peer: OnionAddress,
    session_id: Arc<Mutex<Option<Uuid>>>,
}

impl OnionSidechannel {
    pub fn new(client: reqwest::Client, own: OnionAddress, peer: OnionAddress) -> OnionSidechannel {
        OnionSidechannel {
            client,
            own,
            peer,
            session_id: Arc::new(Mutex::new(None)),
        }
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
                let sidechannel = OnionSidechannel::new(
                    self.client.clone(),
                    self.own_address.clone(),
                    peer.clone(),
                );
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

        // let mut socket_clone = socket.clone();
        // let peer_clone = peer.clone();
        // let cached_socket_handle: Option<JoinHandle<(VeqSessionAlias, AugmentedInfo)>> = {
        //     let cached_info = self.db.get(format!("peer-{}", peer_clone)).unwrap();
        //     // println!("cached_info {:?}", cached_info);
        //     if let Some(cached_info) = cached_info {
        //         let augmented_info: AugmentedInfo = bincode::deserialize(&cached_info).unwrap();
        //         Some(tokio::spawn(async move {
        //             (socket_clone.connect(id, augmented_info.conn_info.clone()).await, augmented_info)
        //         }))
        //     } else {
        //         None
        //     }
        // };
        let db_clone = self.db.clone();
        let mut socket_clone = socket.clone();
        let peer_clone = peer.clone();
        let info_handle = tokio::spawn(async move {
            let info = wait_for_peer_info(&mut sc).await;
            db_clone.insert(format!("peer-{}", peer_clone), bincode::serialize(&info).unwrap()).unwrap();
            (socket_clone.connect(id, info.conn_info.clone()).await, info)
        });

        info_handle.await.ok()
        // match cached_socket_handle {
        //     Some(handle) => {
        //         let handle_fused = handle.fuse();
        //         let info_fused = info_handle.fuse();
        //         pin_mut!(handle_fused, info_fused);
        //         select! {
        //             x = handle_fused => {
        //                 let (session, info) = x.unwrap();
        //                 Some((session, info))
        //             },
        //             y = info_fused => {
        //                 let (socket, info) = y.unwrap();
        //                 Some((socket, info))
        //             }
        //         }
        //     }
        //     None => {
        //         info_handle.await.ok()
        //     }
        // }
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
