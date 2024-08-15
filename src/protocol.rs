use bincode::ErrorKind;
use serde::{Deserialize, Serialize};

use std::io::{Error, Write};

use crate::clerver::AudioFrame;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ProtocolMessage {
    AudioFrame(AudioFrame),
    IdentityDeclaration(PeerIdentity),
    PeerDiscovery(Vec<PeerIdentity>),
    ChatMessage(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PeerIdentity {
    pub canonical_name: String,
    display_name: Option<String>,
    addresses: Vec<String>,
}

impl PeerIdentity {
    pub fn new(canonical_name: String) -> PeerIdentity {
        PeerIdentity {
            canonical_name,
            display_name: None,
            addresses: Vec::new(),
        }
    }
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
