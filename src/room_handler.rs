use std::convert::TryInto;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::connection_manager::AugmentedInfo;

use baybridge::client::Actions;

use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    ChaCha20Poly1305,
};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

use argon2::Argon2;

const ENCRYPTION_KEY_SALT: [u8; 20] = *b"tubechipchillipepper";
const FINGERPRINT_SALT: [u8; 16] = *b"fasteturtleplane";
const SIGNING_SALT: [u8; 19] = *b"openbinderbikezebra";

#[derive(serde::Serialize, serde::Deserialize)]
struct EncryptedValue {
    ciphertext: Vec<u8>,
    // TODO: Nonce is a fixed-length byte array. Do nicer serialization for it.
    nonce: Vec<u8>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SignedValue {
    msg: Vec<u8>,
    signature: Signature,
}

async fn action_set(
    action: &Actions,
    cipher: &ChaCha20Poly1305,
    signing_key: &SigningKey,
    key: String,
    value: &[u8],
) -> anyhow::Result<()> {
    // Encrypt value
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let Ok(ciphertext) = cipher.encrypt(&nonce, value) else {
        anyhow::bail!("Failed to encrypt value.");
    };

    // Add nonce
    let encrypted_value = bincode::serialize(&EncryptedValue {
        ciphertext,
        nonce: nonce.to_vec(),
    })?;

    // Sign (value, nonce)
    // TODO: one day baybridge should have verified namespaces, so this code should be pushed into the baybridge side,
    // and verification should happen on the server or something
    let signature = signing_key.sign(&encrypted_value);
    let signed_value = SignedValue {
        msg: encrypted_value,
        signature,
    };

    // Set to key
    let mut serialized_signed_value: &[u8] = &bincode::serialize(&signed_value)?;
    let encoded_signed_value = ecoji::encode_to_string(&mut serialized_signed_value)?;
    if let Err(e) = action.set(key, encoded_signed_value).await {
        anyhow::bail!("Failed to set value to baybridge with error: {e}");
    }

    Ok(())
}

async fn set_own_info(
    action: &Actions,
    cipher: &ChaCha20Poly1305,
    signing_key: &SigningKey,
    room_fingerprint: String,
    connection_info: veq::veq::ConnectionInfo,
    display_name: String,
) -> anyhow::Result<()> {
    let info = AugmentedInfo {
        connection_info,
        display_name,
    };
    let serialized_info: &[u8] = &bincode::serialize(&info)?;

    action_set(
        action,
        cipher,
        signing_key,
        room_fingerprint,
        serialized_info,
    )
    .await
}

fn verify_and_decrypt(
    cipher: &ChaCha20Poly1305,
    verifying_key: &VerifyingKey,
    info: String,
) -> anyhow::Result<AugmentedInfo> {
    // Deserialize to SignedValue
    let serialized_signed_value = ecoji::decode_to_vec(&mut info.as_bytes())?;
    let signed_value: SignedValue = bincode::deserialize(&serialized_signed_value)?;

    // Verify signature
    verifying_key.verify_strict(&signed_value.msg, &signed_value.signature)?;

    // Deserialize to EncryptedValue
    let encrypted_value: EncryptedValue = bincode::deserialize(&signed_value.msg)?;

    // Use nonce and cipher to decrypt
    let Ok(nonce): Result<[u8; 12], _> = encrypted_value.nonce.try_into() else {
        anyhow::bail!("Failed to get nonce out of (encrypted value, nonce) pair.");
    };
    let Ok(serialized_info) = cipher.decrypt(&nonce.into(), &*encrypted_value.ciphertext) else {
        anyhow::bail!("Failed to deserialize encrypted value.");
    };

    let info = bincode::deserialize(&serialized_info)?;
    Ok(info)
}

/// Find peer connection info on the Bay Bridge room
/// and send it over the conn_info_tx channel.
pub async fn start_room_connection(
    action: Actions,
    room_name: &str,
    connection_info: veq::veq::ConnectionInfo,
    display_name: Option<String>,
    conn_info_tx: mpsc::UnboundedSender<AugmentedInfo>,
    app_event_tx: Option<mpsc::UnboundedSender<insanity_tui::AppEvent>>,
    cancellation_token: CancellationToken,
) -> anyhow::Result<()> {
    let argon = Argon2::default();

    // Set up room encryption cipher
    let cipher = {
        let mut encryption_key = [0u8; 32];
        if let Err(e) = argon.hash_password_into(
            room_name.as_bytes(),
            &ENCRYPTION_KEY_SALT,
            &mut encryption_key,
        ) {
            anyhow::bail!(e);
        }
        ChaCha20Poly1305::new(&encryption_key.into())
    };

    // Make fingerprint (which will be used as key on baybridge) by hashing encryption_
    let room_fingerprint = {
        let mut fingerprint_material = [0u8; 32];
        if let Err(e) = argon.hash_password_into(
            room_name.as_bytes(),
            &FINGERPRINT_SALT,
            &mut fingerprint_material,
        ) {
            anyhow::bail!(e);
        }
        let fingerprint = blake3::hash(&fingerprint_material);
        fingerprint.to_string()
    };
    log::debug!("Room fingerprint: {room_fingerprint}");
    if let Some(app_event_tx) = app_event_tx.clone() {
        if let Err(e) = app_event_tx.send(insanity_tui::AppEvent::SetRoomFingerprint(
            room_fingerprint.clone(),
        )) {
            log::debug!("Failed to write room fingerprint to UI: {e}");
        }
    }

    let signing_key = {
        let mut signing_key_material = [0u8; 32];
        if let Err(e) = argon.hash_password_into(
            room_name.as_bytes(),
            &SIGNING_SALT,
            &mut signing_key_material,
        ) {
            anyhow::bail!(e);
        }
        ed25519_dalek::SigningKey::from_bytes(&signing_key_material)
    };

    // Write self to server.
    // TODO: handle default name better
    let display_name = display_name.clone().unwrap_or("missing_name".to_string());
    set_own_info(
        &action,
        &cipher,
        &signing_key,
        room_fingerprint.clone(),
        connection_info,
        display_name,
    )
    .await?;

    // Start background task to read connections to the room.
    tokio::spawn(async move {
        let verifying_key = signing_key.verifying_key();
        tokio::select! {
            _ = retrieve_peers(action, &cipher, &verifying_key, &room_fingerprint, conn_info_tx) => {
                log::error!("Retrieve peers loop failed");
            },
            _ = cancellation_token.cancelled() => {
                log::debug!("Baybridge-related tasks shutdown.");
            }
        }
    });
    Ok(())
}

async fn retrieve_peers(
    action: Actions,
    cipher: &ChaCha20Poly1305,
    verifying_key: &VerifyingKey,
    room_fingerprint: &str,
    conn_info_tx: mpsc::UnboundedSender<AugmentedInfo>,
) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(1000));
    let me = action.whoami().await;
    loop {
        interval.tick().await;
        log::debug!("Interval tick on retrieve peers.");
        let nsr = action.namespace(room_fingerprint).await?;
        let mapping = nsr.mapping;
        for (person, encrypted_info) in mapping {
            if me == person {
                continue;
            }
            let Ok(info) = verify_and_decrypt(cipher, verifying_key, encrypted_info) else {
                log::debug!("Failed to parse contents of response into AugmentedInfo.");
                continue;
            };
            // let Ok(info) = serde_json::from_str::<AugmentedInfo>(&info) else {
            //     log::debug!("Failed to parse contents of response into AugmentedInfo.");
            //     continue;
            // };
            log::debug!("Got info: {:?}", info);
            if let Err(e) = conn_info_tx.send(info) {
                log::debug!("Failed to send received connection info: {:?}", e);
            }
        }
    }
}
