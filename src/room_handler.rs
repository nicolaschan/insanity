use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::connection_manager::AugmentedInfo;

use baybridge::client::Actions;

use chacha20poly1305::{ChaCha20Poly1305, Nonce, aead::OsRng, aead::KeyInit};

use argon2::Argon2;

const ENCRYPTION_KEY_SALT: [u8; 22] = *b"insanityencryptionsalt";
const SIGNING_SALT: [u8; 23] = *b"insanityroomsigningsalt";

/// Find peer connection info on the Bay Bridge room
/// and send it over the conn_info_tx channel.
pub async fn start_room_connection(
    action: Actions,
    room_name: &str,
    connection_info: veq::veq::ConnectionInfo,
    display_name: Option<String>,
    conn_info_tx: mpsc::UnboundedSender<AugmentedInfo>,
    cancellation_token: CancellationToken,
) -> anyhow::Result<()> {

    // Set up room encryption cipher
    let argon = Argon2::default();
    let mut encryption_key = [0u8; 32];
    if let Err(e) = argon.hash_password_into(room_name.as_bytes(), &ENCRYPTION_KEY_SALT, &mut encryption_key) {
        anyhow::bail!(e);
    }
    let cipher = ChaCha20Poly1305::new(&encryption_key.into());




    // let mut signing_key_material = [0u8; 32];
    // if let Err(e) = argon.hash_password_into(room_name.as_bytes(), &SIGNING_SALT, &mut signing_key_material) {
    //     anyhow::bail!(e);
    // }




    // Write self to server.
    // TODO: handle default name better
    let info = AugmentedInfo {
        connection_info,
        display_name: display_name.clone().unwrap_or("missing_name".to_string()),
    };
    let json_info = serde_json::to_string(&info)?;



    let set_self_res = action.set(room_name.to_string(), json_info).await;
    // TODO: keep trying to write self and don't proceed otherwise.
    match set_self_res {
        Ok(()) => {}
        Err(e) => {
            log::debug!("Failed to write own info to room: {e}");
            return Err(e);
        }
    }

    // Start background task to read connections to the room.
    let room_name_string = room_name.to_string();
    tokio::spawn(async move {
        tokio::select! {
            _ = retrieve_peers(action, &room_name_string, conn_info_tx) => {
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
    room_name: &str,
    conn_info_tx: mpsc::UnboundedSender<AugmentedInfo>,
) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(1000));
    let me = action.whoami().await;
    loop {
        interval.tick().await;
        log::debug!("Interval tick on retrieve peers.");
        let nsr = action.namespace(room_name).await?;
        let mapping = nsr.mapping;
        for (person, info) in mapping {
            if me == person {
                continue;
            }
            let Ok(info) = serde_json::from_str::<AugmentedInfo>(&info) else {
                log::debug!("Failed to parse contents of response into AugmentedInfo.");
                continue;
            };
            log::debug!("Got info: {:?}", info);
            if let Err(e) = conn_info_tx.send(info) {
                log::debug!("Failed to send received connection info: {:?}", e);
            }
        }
    }
}
