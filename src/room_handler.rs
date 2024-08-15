use iroh::{
    client::{Doc, Iroh},
    node::Node,
    sync::{
        store::{DownloadPolicy, FilterKind, Query},
        AuthorId,
    },
    ticket::DocTicket,
};
use std::path::PathBuf;

use iroh::rpc_protocol::ProviderService;
use std::str::FromStr;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::connection_manager::AugmentedInfo;

const IROH_KEY_INFO: &'static str = "info";
const IROH_KEY_HEARTBEAT: &'static str = "heartbeat";
const IROH_KEY_LIST: [&'static str; 2] = [IROH_KEY_INFO, IROH_KEY_HEARTBEAT];

const IROH_VALUE_HEARTBEAT: &'static str = "alive";

/// Find peer connection info on the Iroh document room_ticket
/// and send it over the conn_info_tx channel.
pub async fn start_room_connection(
    room_ticket: &str,
    connection_info: veq::veq::ConnectionInfo,
    display_name: Option<String>,
    iroh_path: &PathBuf,
    conn_info_tx: mpsc::UnboundedSender<AugmentedInfo>,
    cancellation_token: CancellationToken,
) -> anyhow::Result<()> {
    let iroh_node = Node::persistent(iroh_path).await?.spawn().await?;
    let iroh_client = iroh_node.client().clone();
    let author_id = if let Ok(Some(author_id)) = iroh_client.authors.list().await?.try_next().await
    {
        // Reuse existing author ID.
        author_id
    } else {
        // Create new author ID if no existing one available.
        iroh_client.authors.create().await?
    };
    log::debug!("Author ID: {author_id}");
    let doc_ticket = DocTicket::from_str(room_ticket)?;
    log::debug!("Room ticket decoded: {:?}", doc_ticket);

    let doc = iroh_client.docs.import(doc_ticket.clone()).await?;
    // Download values for only those keys which are needed.
    doc.set_download_policy(DownloadPolicy::NothingExcept(
        IROH_KEY_LIST
            .iter()
            .map(|key| FilterKind::Exact((*key).into()))
            .collect(),
    ))
    .await?;

    // Write own info to document.
    let info = AugmentedInfo {
        connection_info,
        display_name: display_name.clone().unwrap_or(author_id.to_string()),
    };
    let json = serde_json::to_string(&info)?;
    doc.set_bytes(author_id, IROH_KEY_INFO, json).await?;

    // Start background tasks which should not close until Insanity does.
    tokio::spawn(async move {
        tokio::select! {
            res = handle_iroh_events(iroh_client, &doc, conn_info_tx) => {
                log::error!("Iroh event handler shutdown unexpectedly: {:?}.", res);
            },
            // res = send_iroh_heartbeat(author_id, &doc) => {
            //     log::error!("Iroh heartbeat sender shutdown unexpectedly: {:?}.", res);
            // },
            res = iroh_node => {
                log::error!("Iroh node shutdown unexpectedly: {:?}.", res);
            },
            _ = cancellation_token.cancelled() => {
                log::debug!("Iroh-related tasks shutdown.");
            }
        }
    });
    Ok(())
}

async fn handle_iroh_events<C: quic_rpc::ServiceConnection<ProviderService>>(
    client: Iroh<C>,
    doc: &Doc<C>,
    conn_info_tx: mpsc::UnboundedSender<AugmentedInfo>,
) {
    loop {
        log::debug!("starting loop of handle Iroh events.");
        let Ok(mut event_stream) = doc.subscribe().await else {
            log::debug!("Failed to subscribe to Iroh document event stream.");
            continue;
        };
        let query = Query::key_exact(IROH_KEY_INFO).build();
        while let Ok(_event) = event_stream.try_next().await {
            // log::debug!("Reading Iroh event.");
            // let Some(event) = event else {
            //     continue;
            // };
            // match event {
            //     LiveEvent::InsertLocal { .. }
            //     | LiveEvent::ContentReady { .. }
            //     | LiveEvent::InsertRemote {
            //         content_status: ContentStatus::Complete,
            //         ..
            //     } => {

            let Ok(mut entry_stream) = doc.get_many(query.clone()).await else {
                log::debug!("Failed to get Iroh entry stream.");
                continue;
            };
            while let Ok(Some(entry)) = entry_stream.try_next().await {
                let Ok(content) = entry.content_bytes(&client).await else {
                    log::debug!("Could not read contents of Iroh entry.");
                    continue;
                };
                let Ok(info) = serde_json::from_slice::<AugmentedInfo>(&content) else {
                    log::debug!("Failed to parse contents of Iroh entry into AugmentedInfo.");
                    continue;
                };
                // log::debug!("Got info: {:?}", info);
                if let Err(e) = conn_info_tx.send(info) {
                    log::debug!("Failed to send received connection info: {:?}", e);
                }
            }
            // }
            // _ => {}
            // }
        }
    }
}

async fn send_iroh_heartbeat<C: quic_rpc::ServiceConnection<ProviderService>>(
    author_id: AuthorId,
    doc: &Doc<C>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        interval.tick().await;
        log::debug!("Writing heartbeat.");
        if let Err(e) = doc
            .set_bytes(author_id, IROH_KEY_HEARTBEAT, IROH_VALUE_HEARTBEAT)
            .await
        {
            log::debug!("Sending Iroh heartbeat failed: {:?}", e);
        }
    }
}
