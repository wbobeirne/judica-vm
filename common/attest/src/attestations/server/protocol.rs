use self::authentication_handshake::MessageExt;

use super::super::query::Tips;
use super::generic_websocket::WebSocketFunctionality;
use crate::attestations::client::AnySender;
use crate::attestations::client::ProtocolReceiver;
use crate::control::query::Outcome;
use crate::globals::Globals;
use attest_database::connection::MsgDB;
use attest_messages::Envelope;
use axum::extract::ws::Message;
use sapio_bitcoin::hashes::sha256;
use sapio_bitcoin::hashes::Hash;
use sapio_bitcoin::secp256k1::Secp256k1;
use serde::Deserialize;
use serde::Serialize;
use std;
use std::collections::BTreeMap;
use std::fmt::Display;
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::protocol::Role;
use tracing;
use tracing::info;
use tracing::trace;
use tracing::warn;

#[derive(Serialize, Deserialize, Debug)]
pub enum AttestSocketProtocol {
    Request(u64, AttestRequest),
    Response(u64, AttestResponse),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AuthenticationCookie {
    pub(crate) secret: [u8; 32],
    pub(crate) service_claim: (String, u16),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Post {
    pub(crate) envelopes: Vec<Envelope>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct LatestTips {}

#[derive(Serialize, Deserialize, Debug)]
pub struct SpecificTips {
    pub tips: Tips,
}
#[derive(Serialize, Deserialize, Debug)]
pub enum AttestRequest {
    LatestTips(LatestTips),
    SpecificTips(SpecificTips),
    Post(Post),
}

impl From<LatestTips> for AttestRequest {
    fn from(l: LatestTips) -> Self {
        AttestRequest::LatestTips(l)
    }
}
impl From<Post> for AttestRequest {
    fn from(l: Post) -> Self {
        AttestRequest::Post(l)
    }
}
impl From<SpecificTips> for AttestRequest {
    fn from(l: SpecificTips) -> Self {
        AttestRequest::SpecificTips(l)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct LatestTipsResponse(pub Vec<Envelope>);
#[derive(Serialize, Deserialize, Debug)]
pub struct SpecificTipsResponse(pub Vec<Envelope>);
#[derive(Serialize, Deserialize, Debug)]
pub struct PostResponse(pub Vec<Outcome>);

#[derive(Serialize, Deserialize, Debug)]
pub enum AttestResponse {
    LatestTips(LatestTipsResponse),
    SpecificTips(SpecificTipsResponse),
    Post(PostResponse),
}

#[derive(PartialEq, Eq, Debug)]
pub struct ResponseCode(u64);

impl AttestRequest {
    pub(crate) fn response_code_of(&self) -> ResponseCode {
        ResponseCode(match self {
            AttestRequest::LatestTips(_) => 0,
            AttestRequest::SpecificTips(_) => 1,
            AttestRequest::Post(_) => 2,
        })
    }
    pub(crate) fn into_protocol_and_log(self, seq: u64) -> Result<Message, serde_json::Error> {
        let msg = &AttestSocketProtocol::Request(seq, self);
        trace!(?msg, seq, "Sending Request");
        Ok(Message::Text(serde_json::to_string(msg)?))
    }
}
impl AttestResponse {
    pub(crate) fn response_code_of(&self) -> ResponseCode {
        ResponseCode(match self {
            AttestResponse::LatestTips(_) => 0,
            AttestResponse::SpecificTips(_) => 1,
            AttestResponse::Post(_) => 2,
        })
    }
    pub(crate) fn into_protocol_and_log(self, seq: u64) -> Result<Message, serde_json::Error> {
        let msg = &AttestSocketProtocol::Response(seq, self);
        trace!(?msg, seq, "Sending Response");
        Ok(Message::Text(serde_json::to_string(msg)?))
    }
}

#[derive(Debug)]
pub enum AttestProtocolError {
    JsonError(String),
    ReqwetError(String),
    SocketError(axum::Error),
    HostnameUnknown,
    NonZeroSync,
    IncorrectMessageOwned(String),
    CookieMissMatch,
    TimedOut,
    SocketClosed,
    FailedToAuthenticate,
    AlreadyConnected,
    InvalidSetup,
    DatabaseError,
    ResponseTypeIncorrect,
    UnrequestedResponse,
    InvalidChallengeHashString,
}

unsafe impl Send for AttestProtocolError {}

unsafe impl Sync for AttestProtocolError {}

impl Display for AttestProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl From<axum::Error> for AttestProtocolError {
    fn from(e: axum::Error) -> Self {
        AttestProtocolError::SocketError(e)
    }
}
impl From<reqwest::Error> for AttestProtocolError {
    fn from(e: reqwest::Error) -> Self {
        AttestProtocolError::ReqwetError(e.to_string())
    }
}
impl From<serde_json::Error> for AttestProtocolError {
    fn from(e: serde_json::Error) -> Self {
        AttestProtocolError::JsonError(e.to_string())
    }
}

impl std::error::Error for AttestProtocolError {}

type ServiceIDBuilder = (String, u16);
type Challenge = sha256::Hash;
type Timeout = i64;
type Secret = [u8; 32];
type ChallengeResponse = (oneshot::Sender<Secret>, Timeout);

#[derive(Clone, Default)]
pub struct GlobalSocketState {
    cookies: Arc<Mutex<BTreeMap<Challenge, ChallengeResponse>>>,
}

impl GlobalSocketState {
    pub async fn expect_a_cookie(&self, challenge: Challenge) -> oneshot::Receiver<Secret> {
        let (tx, rx) = oneshot::channel();
        let mut cookiejar = self.cookies.lock().await;
        if cookiejar.len() > 100 {
            trace!("Garbage Collecting Authentication Challenges");
            let stale = attest_util::now() - 1000 * 20;
            cookiejar.retain(|_k, x| x.1 > stale);
            if cookiejar.len() > 100 {
                if let Some(unstale_challenge) = cookiejar.keys().cloned().next() {
                    cookiejar.remove(&unstale_challenge);
                }
            }
        }

        trace!(challenge = ?challenge, "New Authentication Challenge");
        let _e = cookiejar.insert(challenge, (tx, attest_util::now()));
        rx
    }
    pub async fn add_a_cookie(&self, cookie: Secret) {
        let k = sha256::Hash::hash(&cookie);
        trace!(protocol="handshake", challenge =?k, secret = ?cookie, "Resolved Authentication Challenge");
        let mut cookiejar = self.cookies.lock().await;
        trace!(cookiejar=?cookiejar, looking_for=?k, "CookieJar");
        if let Some(f) = cookiejar.remove(&k) {
            if f.0.send(cookie).is_err() {
                trace!(protocol="handshake", challenge =?k, secret = ?cookie, "Cookie Could Not Be Sent");
            } else {
                trace!(protocol="handshake", challenge =?k, secret = ?cookie, "Cookie Forwarded to Application");
            }
        } else {
            trace!(protocol="handshake", challenge =?k, secret = ?cookie, "Cookie Not Found");
        }
    }
}

pub mod authentication_handshake;

struct ResponseRouter {
    code: ResponseCode,
    sender: AnySender,
}
// Only allow 10 outstanding messages
pub const MAX_MESSAGE_DEFECIT: i64 = 10;

pub async fn run_protocol<W: WebSocketFunctionality>(
    g: Arc<Globals>,
    socket: W,
    mut gss: GlobalSocketState,
    mut db: MsgDB,
    role: Role,
    new_request: Option<ProtocolReceiver>,
) -> Result<&'static str, AttestProtocolError> {
    let (
        mut socket,
        ProtocolReceiver {
            mut latest_tips,
            mut specific_tips,
            mut post,
        },
    ) = authentication_handshake::handshake_protocol(g, socket, &mut gss, role, new_request)
        .await?;
    let mut inflight_requests: BTreeMap<u64, ResponseRouter> = Default::default();
    let mut seq = 0;
    let mut defecit = 0;
    loop {
        seq += 1;
        trace!(seq, "waiting for request from peer or internal");
        tokio::select! {
            msg = socket.t_recv() => {
                if let Some(Ok(msg)) = msg {
                    handle_message_from_peer(
                        &mut defecit,
                        &mut socket,
                        &mut gss,
                        &mut db,
                        &mut inflight_requests,
                        role,
                        msg,
                    )
                    .await?;
                } else {
                    trace!(seq, ?role, "socket quit: TCP Socket is Disconnected");
                    return Ok("Peer Disconnected from us");
                }
            }
            Some((request, chan)) = post.recv(), if defecit < MAX_MESSAGE_DEFECIT => {
                handle_internal_request(
                    &mut defecit,
                    &mut socket,
                    &mut inflight_requests,
                    seq,
                    request,
                    chan,
                )
                .await?;
            }
            Some((request, chan)) = specific_tips.recv(), if defecit < MAX_MESSAGE_DEFECIT => {
                handle_internal_request(
                    &mut defecit,
                    &mut socket,
                    &mut inflight_requests,
                    seq,
                    request,
                    chan,
                )
                .await?;
            }
            Some((request, chan)) = latest_tips.recv(), if defecit < MAX_MESSAGE_DEFECIT => {
                handle_internal_request(
                    &mut defecit,
                    &mut socket,
                    &mut inflight_requests,
                    seq,
                    request,
                    chan,
                )
                .await?;
            }
            else => {
                trace!(seq, ?role, "socket quit: Internal Connection Dropped");
                socket.t_close().await.ok();
                return Ok("Exiting...");
            }
        }
    }
}
async fn handle_internal_request<W, IChan, IReq>(
    defecit: &mut i64,
    socket: &mut W,
    inflight_requests: &mut BTreeMap<u64, ResponseRouter>,
    seq: u64,
    msg: IReq,
    response_chan: IChan,
) -> Result<(), AttestProtocolError>
where
    W: WebSocketFunctionality,
    IChan: Into<AnySender>,
    IReq: Into<AttestRequest>,
    W: WebSocketFunctionality,
{
    let msg = msg.into();
    trace!(code=?msg.response_code_of(), seq, "new internal request");
    inflight_requests.insert(
        seq,
        ResponseRouter {
            code: msg.response_code_of(),
            sender: response_chan.into(),
        },
    );
    *defecit += 1;
    socket.t_send(msg.into_protocol_and_log(seq)?).await?;

    Ok(())
}

async fn handle_message_from_peer<W: WebSocketFunctionality>(
    defecit: &mut i64,
    socket: &mut W,
    _gss: &mut GlobalSocketState,
    db: &mut MsgDB,
    inflight_requests: &mut BTreeMap<u64, ResponseRouter>,
    _role: Role,
    msg: Message,
) -> Result<(), AttestProtocolError> {
    let a: AttestSocketProtocol = msg
        .only_text("as a json encoded messages")
        .and_then(|s| Ok(serde_json::from_str(&s)?))?;
    match a {
        AttestSocketProtocol::Request(seq, m) => {
            trace!(request=?m, seq, "Processing Request...");
            match m {
                AttestRequest::LatestTips(LatestTips {}) => {
                    fetch_latest_tips(db, socket, seq).await
                }
                AttestRequest::SpecificTips(SpecificTips { tips }) => {
                    fetch_specific_tips(tips, db, socket, seq).await
                }
                AttestRequest::Post(Post { envelopes }) => {
                    post_envelope(envelopes, db, socket, seq).await
                }
            }
        }
        AttestSocketProtocol::Response(seq, r) => {
            *defecit -= 1;
            trace!(response=?r, seq, "Routing Response...");
            if let Some(k) = inflight_requests.remove(&seq) {
                if r.response_code_of() != k.code {
                    return Err(AttestProtocolError::ResponseTypeIncorrect);
                }
                // we don't care if the sender dropped
                match (k.sender, r) {
                    (AnySender::LatestTips(s), AttestResponse::LatestTips(m)) => s.send(m).ok(),
                    (AnySender::Post(s), AttestResponse::Post(m)) => s.send(m).ok(),
                    (AnySender::SpecificTips(s), AttestResponse::SpecificTips(m)) => s.send(m).ok(),
                    _ => {
                        warn!("Message Mismatch");
                        return Err(AttestProtocolError::ResponseTypeIncorrect);
                    }
                }
                .map_or_else(
                    || trace!("Did not send response to oneshot::reciever, closed"),
                    |_| trace!("Successfully sent response to oneshot::reciever"),
                );
                Ok(())
            } else {
                Err(AttestProtocolError::UnrequestedResponse)
            }
        }
    }
}

async fn post_envelope<W>(
    envelopes: Vec<Envelope>,
    db: &mut MsgDB,
    socket: &mut W,
    seq: u64,
) -> Result<(), AttestProtocolError>
where
    W: WebSocketFunctionality,
{
    info!(method = "POST", item = "/envelope/new");
    let mut authed = Vec::with_capacity(envelopes.len());
    for envelope in envelopes {
        info!(method="POST /msg",  envelope=?envelope.canonicalized_hash_ref(), "Envelope Received" );
        trace!(method="POST /msg",  envelope=?envelope, "Envelope Received" );
        if let Ok(valid_envelope) = envelope.self_authenticate(&Secp256k1::new()) {
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
            trace!("Inserting Into Database");
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
    if socket
        .t_send(AttestResponse::Post(PostResponse(outcomes)).into_protocol_and_log(seq)?)
        .await
        .is_err()
    {
        return Err(AttestProtocolError::SocketClosed);
    }
    Ok(())
}

async fn fetch_specific_tips<W>(
    mut tips: Tips,
    db: &mut MsgDB,
    socket: &mut W,
    seq: u64,
) -> Result<(), AttestProtocolError>
where
    W: WebSocketFunctionality,
{
    info!(method = "GET", item = "/specific_tips");
    // runs in O(N) usually since the slice should already be sorted
    tips.tips.sort_unstable();
    tips.tips.dedup();
    trace!(method = "GET /tips", ?tips);
    let all_tips = {
        let handle = db.get_handle().await;
        if let Ok(r) = handle.messages_by_hash(tips.tips.iter()) {
            r
        } else {
            return Err(AttestProtocolError::DatabaseError);
        }
    };
    if socket
        .t_send(
            AttestResponse::SpecificTips(SpecificTipsResponse(all_tips))
                .into_protocol_and_log(seq)?,
        )
        .await
        .is_err()
    {
        return Err(AttestProtocolError::SocketClosed);
    }
    Ok(())
}

async fn fetch_latest_tips<W>(
    db: &mut MsgDB,
    socket: &mut W,
    seq: u64,
) -> Result<(), AttestProtocolError>
where
    W: WebSocketFunctionality,
{
    info!(method = "GET", item = "/latest_tips");
    let r = {
        let handle = db.get_handle().await;
        handle.get_tips_for_all_users()
    };
    if let Ok(v) = r {
        let msg = AttestResponse::LatestTips(LatestTipsResponse(v)).into_protocol_and_log(seq)?;
        if socket.t_send(msg).await.is_err() {
            trace!(seq, "peer rejected message");
            Err(AttestProtocolError::SocketClosed)
        } else {
            info!(
                method = "GET",
                item = "/latest_tips",
                "fetched and sent successfully"
            );
            Ok(())
        }
    } else {
        warn!("Database Error, Disconnecting");
        Err(AttestProtocolError::DatabaseError)
    }
}
