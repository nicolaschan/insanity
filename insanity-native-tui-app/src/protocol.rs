use bincode::ErrorKind;
use insanity_core::audio::codec::EncodedChunk;
use serde::{Deserialize, Serialize};

use std::io::{Error, Write};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AudioFrame {
    pub sequence_number: u128,
    pub payload: Vec<u8>,
}

impl From<EncodedChunk> for AudioFrame {
    fn from(chunk: EncodedChunk) -> Self {
        AudioFrame {
            sequence_number: chunk.sequence_number,
            payload: chunk.payload,
        }
    }
}

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
        let Ok(serialized) = bincode::serialize(self) else {
            log::error!("Error serializing protocol message");
            return Err(Error::other("serialize protocol message"));
        };
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
