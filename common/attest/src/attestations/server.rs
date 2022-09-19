use super::query::Tips;
use crate::{control::query::Outcome, globals::Globals};
use attest_database::connection::MsgDB;
use attest_messages::Envelope;
use attest_util::{AbstractResult, INFER_UNIT};
use axum::{
    extract::{
        ws::{Message, WebSocket},
        ConnectInfo, WebSocketUpgrade,
    },
    http::Response,
    http::StatusCode,
    routing::{get, post},
    Extension, Json, Router,
};
use futures::{Future, FutureExt, Sink, Stream};
use sapio_bitcoin::secp256k1::Secp256k1;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt::Display, io::Read, net::SocketAddr, pin::Pin, sync::Arc};
use tokio::{net::TcpStream, spawn};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tower_http::trace::TraceLayer;
use tracing::{info, trace, warn};
pub mod tungstenite_client_adaptor;

pub async fn get_newest_tip_handler(
    Extension(db): Extension<MsgDB>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<(Response<()>, Json<Vec<Envelope>>), (StatusCode, &'static str)> {
    let handle = db.get_handle().await;
    trace!(from=?addr, method="GET /newest_tips");
    info!(from=?addr, method="GET /newest_tips");
    let r = handle
        .get_tips_for_all_users()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, ""))?;

    trace!(from=?addr, method="GET /newest_tips", response=?r);
    Ok((
        Response::builder()
            .status(200)
            .header("Access-Control-Allow-Origin", "*")
            .body(())
            .expect("Response<()> should always be valid"),
        Json(r),
    ))
}
pub async fn get_tip_handler(
    Extension(db): Extension<MsgDB>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(mut tips): Json<Tips>,
) -> Result<(Response<()>, Json<Vec<Envelope>>), (StatusCode, &'static str)> {
    let handle = db.get_handle().await;
    trace!(from=?addr, method="GET /tips",?tips);
    // runs in O(N) usually since the slice should already be sorted
    tips.tips.sort_unstable();
    tips.tips.dedup();
    let r = handle
        .messages_by_hash(tips.tips.iter())
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, ""))?;

    trace!(from=?addr, method="GET /tips", response=?r);

    Ok((
        Response::builder()
            .status(200)
            .header("Access-Control-Allow-Origin", "*")
            .body(())
            .expect("Response<()> should always be valid"),
        Json(r),
    ))
}
pub async fn post_message(
    Extension(db): Extension<MsgDB>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(envelopes): Json<Vec<Envelope>>,
) -> Result<(Response<()>, Json<Vec<Outcome>>), (StatusCode, &'static str)> {
    let mut authed = Vec::with_capacity(envelopes.len());
    for envelope in envelopes {
        tracing::info!(method="POST /msg", from=?addr, envelope=?envelope.canonicalized_hash_ref(), "Envelope Received" );
        tracing::trace!(method="POST /msg", from=?addr, envelope=?envelope, "Envelope Received" );
        let envelope = envelope.self_authenticate(&Secp256k1::new()).map_err(|_| {
            tracing::debug!("Invalid Message From Peer");
            (
                StatusCode::UNAUTHORIZED,
                "Envelope not valid. Only valid data should be sent.",
            )
        })?;
        tracing::trace!("Verified Signatures");
        authed.push(envelope);
    }
    let mut outcomes = Vec::with_capacity(authed.len());
    {
        let mut locked = db.get_handle().await;
        for envelope in authed {
            tracing::trace!("Inserting Into Database");
            match locked.try_insert_authenticated_envelope(envelope) {
                Ok(i) => match i {
                    Ok(()) => {
                        outcomes.push(Outcome { success: true });
                    }
                    Err(fail) => {
                        outcomes.push(Outcome { success: false });
                        tracing::debug!(?fail, "Inserting Into Database Failed");
                    }
                },
                Err(err) => {
                    outcomes.push(Outcome { success: false });
                    tracing::debug!(?err, "Inserting Into Database Failed");
                }
            }
        }
    }
    Ok((
        Response::builder()
            .status(200)
            .header("Access-Control-Allow-Origin", "*")
            .body(())
            .expect("Response<()> should always be valid"),
        Json(outcomes),
    ))
}

#[derive(Clone)]
struct GlobalSocketState;

async fn handle_socket(
    ws: WebSocketUpgrade,
    Extension(g): Extension<GlobalSocketState>,
    Extension(db): Extension<MsgDB>,
) -> axum::response::Response {
    ws.on_upgrade(|w| handle_socket_symmetric_server(w, g, db))
}
async fn handle_socket_symmetric_server(
    mut socket: WebSocket,
    g: GlobalSocketState,
    db: MsgDB,
) -> () {
    handle_socket_symmetric(socket, g, db).await;
}

#[derive(Serialize, Deserialize, Debug)]
pub enum AttestSocketProtocol {
    Request(u64, AttestRequest),
    Response(u64, AttestResponse),
}
#[derive(Serialize, Deserialize, Debug)]
pub struct AuthenticationCookie {
    secret: [u8; 32],
    service_claim: (String, u16),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum AttestRequest {
    LatestTips,
    SpecificTips(Tips),
    Post(Vec<Envelope>),
}
#[derive(Serialize, Deserialize, Debug)]
pub enum AttestResponse {
    LatestTips(Vec<Envelope>),
    SpecificTips(Vec<Envelope>),
    PostResult(Vec<Outcome>),
}
#[derive(PartialEq, Eq)]
pub struct ResponseCode(u64);
impl AttestResponse {
    fn response_code_of(&self) -> ResponseCode {
        ResponseCode(match self {
            AttestResponse::LatestTips(_) => 0,
            AttestResponse::SpecificTips(_) => 1,
            AttestResponse::PostResult(_) => 2,
        })
    }
    fn to_protocol_and_log(self, seq: u64) -> Result<Message, serde_json::Error> {
        let msg = &AttestSocketProtocol::Response(seq, self);
        trace!(?msg, seq, "Sending Response");
        Ok(Message::Binary(serde_json::to_vec(msg)?))
    }
}

trait WebSocketFunctionality
where
    Self: Stream<Item = Result<Message, axum::Error>>
        + Sink<Message, Error = axum::Error>
        + Send
        + 'static,
{
    /// Receive another message.
    ///
    /// Returns `None` if the stream has closed.
    fn t_recv<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Option<Result<Message, axum::Error>>> + Send + 'a>>;

    /// Send a message.
    fn t_send<'a>(
        &'a mut self,
        msg: Message,
    ) -> Pin<Box<dyn Future<Output = Result<(), axum::Error>> + Send + 'a>>;

    /// Gracefully close this WebSocket.
    fn t_close(self) -> Pin<Box<dyn Future<Output = Result<(), axum::Error>> + Send >>;
}

impl WebSocketFunctionality for WebSocket {
    fn t_recv<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Option<Result<Message, axum::Error>>> + Send + 'a>> {
        Box::pin(self.recv())
    }

    fn t_send<'a>(
        &'a mut self,
        msg: Message,
    ) -> Pin<Box<dyn Future<Output = Result<(), axum::Error>> + Send + 'a>> {
        Box::pin(self.send(msg))
    }

    fn t_close(self) -> Pin<Box<dyn Future<Output = Result<(), axum::Error>> + Send >> {
        Box::pin(self.close())
    }
}

#[derive(Debug)]
pub enum AttestProtocolError {
    JsonError(String),
}
unsafe impl Send for AttestProtocolError {}
unsafe impl Sync for AttestProtocolError {}
impl Display for AttestProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}
impl From<serde_json::Error> for AttestProtocolError {
    fn from(e: serde_json::Error) -> Self {
        AttestProtocolError::JsonError(e.to_string())
    }
}
impl std::error::Error for AttestProtocolError {}

async fn handle_socket_symmetric<W: WebSocketFunctionality>(
    mut socket: W,
    g: GlobalSocketState,
    db: MsgDB,
) -> Result<(), AttestProtocolError> {
    let inflight_requests: BTreeMap<u64, ResponseCode> = Default::default();

    while let Some(msg) = socket.t_recv().await {
        match msg {
            Ok(m) => match m {
                Message::Text(t) => break,
                Message::Binary(b) => {
                    let a: AttestSocketProtocol = serde_json::from_slice(&b[..])?;
                    match a {
                        AttestSocketProtocol::Request(seq, m) => match m {
                            AttestRequest::LatestTips => {
                                let r = {
                                    let handle = db.get_handle().await;
                                    info!(method = "WS Latest Tips");
                                    handle.get_tips_for_all_users()
                                };
                                if let Ok(v) = r {
                                    if socket
                                        .t_send(
                                            AttestResponse::LatestTips(v)
                                                .to_protocol_and_log(seq)?,
                                        )
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                } else {
                                    warn!("Database Error, Disconnecting");
                                    break;
                                }
                            }
                            AttestRequest::SpecificTips(mut tips) => {
                                // runs in O(N) usually since the slice should already be sorted
                                tips.tips.sort_unstable();
                                tips.tips.dedup();
                                trace!(method = "GET /tips", ?tips);
                                let all_tips = {
                                    let handle = db.get_handle().await;
                                    if let Ok(r) = handle.messages_by_hash(tips.tips.iter()) {
                                        r
                                    } else {
                                        break;
                                    }
                                };

                                if socket
                                    .t_send(
                                        AttestResponse::SpecificTips(all_tips)
                                            .to_protocol_and_log(seq)?,
                                    )
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            AttestRequest::Post(envelopes) => {
                                let mut authed = Vec::with_capacity(envelopes.len());
                                for envelope in envelopes {
                                    tracing::info!(method="POST /msg",  envelope=?envelope.canonicalized_hash_ref(), "Envelope Received" );
                                    tracing::trace!(method="POST /msg",  envelope=?envelope, "Envelope Received" );
                                    if let Ok(valid_envelope) =
                                        envelope.self_authenticate(&Secp256k1::new())
                                    {
                                        authed.push(valid_envelope);
                                    } else {
                                        tracing::debug!("Invalid Message From Peer");
                                        break;
                                    }
                                }
                                let mut outcomes = Vec::with_capacity(authed.len());
                                {
                                    let mut locked = db.get_handle().await;
                                    for envelope in authed {
                                        tracing::trace!("Inserting Into Database");
                                        match locked.try_insert_authenticated_envelope(envelope) {
                                            Ok(i) => match i {
                                                Ok(()) => {
                                                    outcomes.push(Outcome { success: true });
                                                }
                                                Err(fail) => {
                                                    outcomes.push(Outcome { success: false });
                                                    tracing::debug!(
                                                        ?fail,
                                                        "Inserting Into Database Failed"
                                                    );
                                                }
                                            },
                                            Err(err) => {
                                                outcomes.push(Outcome { success: false });
                                                tracing::debug!(
                                                    ?err,
                                                    "Inserting Into Database Failed"
                                                );
                                            }
                                        }
                                    }
                                }
                                if socket
                                    .t_send(
                                        AttestResponse::PostResult(outcomes)
                                            .to_protocol_and_log(seq)?,
                                    )
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        },
                        AttestSocketProtocol::Response(seq, r) => {
                            if let Some(k) = inflight_requests.get(&seq) {
                                if r.response_code_of() != *k {
                                    break;
                                }
                                match r {
                                    AttestResponse::LatestTips(tips) => todo!(),
                                    AttestResponse::SpecificTips(tips) => todo!(),
                                    AttestResponse::PostResult(outcomes) => todo!(),
                                }
                            } else {
                                break;
                            }
                        }
                    }
                }
                Message::Ping(p) | Message::Pong(p) => {}
                Message::Close(c) => break,
            },
            Err(e) => break,
        }
    }
    socket.t_close().await;
    Ok(())
}

pub async fn run(g: Arc<Globals>, db: MsgDB) -> tokio::task::JoinHandle<AbstractResult<()>> {
    tokio::spawn(async move {
        tracing::debug!("Starting Task for Attestation Server");
        // build our application with a route
        let app = Router::new()
            // `POST /msg` goes to `msg`
            .route("/msg", post(post_message))
            .route("/tips", get(get_tip_handler))
            .route("/newest_tips", get(get_newest_tip_handler))
            .route("/socket", get(handle_socket))
            .layer(Extension(db))
            .layer(Extension(GlobalSocketState))
            .layer(TraceLayer::new_for_http());

        // run our app with hyper
        // `axum::Server` is a re-export of `hyper::Server`
        let addr = SocketAddr::from(([127, 0, 0, 1], g.config.attestation_port));
        tracing::debug!("Attestation Server Listening on {}", addr);
        axum::Server::bind(&addr)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .unwrap();
        INFER_UNIT
    })
}
