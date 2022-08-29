use std::pin::Pin;

use super::*;
use crate::attestations::client::AttestationClient;
use crate::attestations::client::NotifyOnDrop;
use crate::attestations::query::Tips;
use attest_database::db_handle::insert::SqliteFail;
use attest_messages::CanonicalEnvelopeHash;
use attest_messages::Envelope;
use attest_util::now;
use attest_util::INFER_UNIT;
use futures::Future;
use tokio;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Notify;

use tracing::info;
use tracing::trace;
use tracing::warn;

pub(crate) async fn fetch_from_peer<C: Verification + 'static>(
    config: Arc<Config>,
    secp: Arc<Secp256k1<C>>,
    client: AttestationClient,
    service: (String, u16),
    conn: MsgDB,
    allow_unsolicited_tips: bool,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    let (request_tips, tips_to_resolve) =
        tokio::sync::mpsc::unbounded_channel::<Vec<CanonicalEnvelopeHash>>();
    let (envelopes_to_process, next_envelope) = tokio::sync::mpsc::unbounded_channel();

    // Spins in a loop getting the latest tips from a peer and emitting to
    // envelopes_to_process
    let mut latest_tip_fetcher = latest_tip_fetcher(
        config.clone(),
        client.clone(),
        service.clone(),
        envelopes_to_process.clone(),
    );
    // Reads from next_envelope, processes results, and then requests to resolve unknown tips
    let mut envelope_processor = envelope_processor(
        config.clone(),
        service.clone(),
        conn,
        secp,
        next_envelope,
        request_tips,
        allow_unsolicited_tips,
    );
    // fetches unknown envelopes
    let mut missing_envelope_fetcher = missing_envelope_fetcher(
        config.clone(),
        client.clone(),
        service.clone(),
        envelopes_to_process.clone(),
        tips_to_resolve,
    );
    let _: () = tokio::select! {
        a = &mut envelope_processor => {
            warn!(?service, task="FETCH", subtask="Envelope Processor", event="SHUTDOWN", err=?a);
            latest_tip_fetcher.abort();
            missing_envelope_fetcher.abort();
            a??
        }
        a = &mut latest_tip_fetcher => {
            warn!(?service, task="FETCH", subtask="Latest Tip Fetcher", event="SHUTDOWN", err=?a);
            envelope_processor.abort();
            missing_envelope_fetcher.abort();
            a??
        }
        a = &mut missing_envelope_fetcher => {
            warn!(?service, task="FETCH", subtask="Missing Envelope Fetcher", event="SHUTDOWN", err=?a);
            envelope_processor.abort();
            latest_tip_fetcher.abort();
            a??
        }
    };
    // if any of the above selected, shut down this peer.
    envelope_processor.abort();
    latest_tip_fetcher.abort();
    missing_envelope_fetcher.abort();

    INFER_UNIT
}

/// enevelope processor verifies an envelope and then forwards any unknown tips
/// to the missing_envelope_fetcher.
pub(crate) fn envelope_processor<C: Verification + 'static>(
    _config: Arc<Config>,
    service: (String, u16),
    conn: MsgDB,
    secp: Arc<Secp256k1<C>>,
    mut next_envelope: tokio::sync::mpsc::UnboundedReceiver<(Vec<Envelope>, NotifyOnDrop)>,
    request_tips: UnboundedSender<Vec<CanonicalEnvelopeHash>>,
    allow_unsolicited_tips: bool,
) -> JoinHandle<Result<(), Box<dyn Error + Send + Sync>>> {
    let envelope_processor = {
        tokio::spawn(async move {
            while let Some((resp, cancel_inflight)) = next_envelope.recv().await {
                // Prefer to process envelopes
                handle_envelope(
                    service.clone(),
                    resp,
                    secp.as_ref(),
                    &conn,
                    &request_tips,
                    allow_unsolicited_tips,
                    cancel_inflight,
                )
                .await?;
            }
            INFER_UNIT
        })
    };
    envelope_processor
}
async fn handle_envelope<C: Verification + 'static>(
    service: (String, u16),
    resp: Vec<Envelope>,
    secp: &Secp256k1<C>,
    conn: &MsgDB,
    request_tips: &UnboundedSender<Vec<CanonicalEnvelopeHash>>,
    allow_unsolicited_tips: bool,
    cancel_inflight: NotifyOnDrop,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut all_tips = Vec::new();
    for envelope in resp {
        tracing::debug!(height = envelope.header().height(),
                        hash = ?envelope.canonicalized_hash_ref(),
                        genesis = ?envelope.get_genesis_hash(),
                        "Processing this envelope");
        tracing::trace!(?envelope, "Processing this envelope");
        match envelope.self_authenticate(secp) {
            Ok(authentic) => {
                tracing::debug!("Authentic Tip: {:?}", authentic);
                let handle = conn.get_handle().await;
                if authentic.inner_ref().header().ancestors().is_none()
                    && authentic.inner_ref().header().height() == 0
                {
                    let new_name = format!("user-{}", now());
                    match handle.insert_user_by_genesis_envelope(new_name, authentic)? {
                        Ok(key) => {
                            trace!(key, ?service, "Created New Genesis From Peer");
                        }
                        Err((SqliteFail::SqliteConstraintUnique, msg)) => {
                            trace!("Already Have this Chain");
                        }
                        Err(e) => {
                            warn!(err=?e, "Other SQL Error");
                            Err(format!("{:?}", e))?;
                        }
                    }
                } else {
                    match handle.try_insert_authenticated_envelope(authentic.clone())? {
                        Ok(_) => {}
                        // This means that a conststraint, most likely that the
                        // genesis header must be known, was not allowed
                        Err((SqliteFail::SqliteConstraintCheck, msg)) => {
                            // try fetching the missing tip
                            if allow_unsolicited_tips {
                                all_tips.push(envelope.get_genesis_hash());
                            }
                        }
                        // This means that the constraint that the user ID was known
                        // was hit, so we need to attempt inserting as a genesis
                        // envelope
                        Err((SqliteFail::SqliteConstraintNotNull, msg)) => {
                            if allow_unsolicited_tips {
                                debug!(
                                    hash = ?authentic.inner_ref().canonicalized_hash_ref(),
                                    ?msg,
                                    "unsolicited tip received",
                                );
                                trace!(envelope=?authentic);
                                all_tips.push(envelope.get_genesis_hash());
                            }
                        }
                        _ => {}
                    }
                }
                // safe to reuse since it is authentic still..
                all_tips.extend(envelope.header().tips().iter().map(|(_, _, v)| v.clone()));
                all_tips.extend(envelope.header().ancestors().iter().map(|a| a.prev_msg()));
            }
            Err(_) => {
                // TODO: Ban peer?
                tracing::warn!(hash=?envelope.canonicalized_hash_ref(), "Message Validation Failed");
                tracing::trace!(?envelope, "Message Validation Failed");
            }
        }
    }
    all_tips.sort_unstable();
    all_tips.dedup();
    let unknown_dep_tips = conn
        .get_handle()
        .await
        .message_not_exists_it(all_tips.iter())?;
    trace!(?all_tips, ?unknown_dep_tips);
    if !unknown_dep_tips.is_empty() {
        request_tips.send(unknown_dep_tips)?;
    }
    Ok(())
}

/// latest_tip_fetcher periodically (randomly) pings a hidden service for it's
/// latest tips
pub(crate) fn latest_tip_fetcher(
    config: Arc<Config>,
    client: AttestationClient,
    service: (String, u16),
    envelopes_to_process: tokio::sync::mpsc::UnboundedSender<(Vec<Envelope>, NotifyOnDrop)>,
) -> JoinHandle<Result<(), Box<dyn Error + Send + Sync>>> {
    tokio::spawn(async move {
        loop {
            let sp = tracing::debug_span!(
                "Fetching Latest Tips",
                ?service,
                task = "FETCH",
                subtask = "latest_tip_fetcher",
            );
            let _ = sp.enter();
            let (url, port) = &service;
            let resp: Vec<Envelope> = client.get_latest_tips(url, *port).await?;
            envelopes_to_process.send((resp, NotifyOnDrop::empty()))?;
            config.peer_service.timer_override.tip_fetch_delay().await;
        }
        // INFER_UNIT
    })
}

/// missing_envelope_fetcher ingests a Vec<Hash> and queries a service for the envelope
/// of those hashes, then sends those envelopers for processing.
pub(crate) fn missing_envelope_fetcher(
    _config: Arc<Config>,
    client: AttestationClient,
    service: (String, u16),
    envelopes_to_process: tokio::sync::mpsc::UnboundedSender<(Vec<Envelope>, NotifyOnDrop)>,
    mut tips_to_resolve: tokio::sync::mpsc::UnboundedReceiver<Vec<CanonicalEnvelopeHash>>,
) -> JoinHandle<Result<(), Box<dyn Error + Send + Sync>>> {
    tokio::spawn(async move {
        let (url, port) = &service;
        loop {
            info!(?service, "waiting for tips to fetch");
            if let Some(tips) = tips_to_resolve.recv().await {
                info!(?service, n = tips.len(), "got tips to fetch");
                let (resp, remove_inflight) =
                    client.get_tips(Tips { tips }, url, *port, true).await?;
                info!(?service, n = resp.len(), "got tips in response");
                envelopes_to_process.send((resp, remove_inflight))?;
            } else {
                info!("Terminating Tip Resolver");
                break;
            }
        }
        INFER_UNIT
    })
}
