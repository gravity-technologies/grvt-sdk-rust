//! One managed WebSocket per gateway ("md" market data, "td" trading).
//!
//! What a socket does:
//! - connects lazily, with the session cookie + X-Grvt-Account-Id on the handshake when logged in
//! - keeps the desired subscriptions and re-sends them after every reconnect
//! - JSON-RPC requests: uint32 id -> oneshot, per-request timeout
//! - reconnects with full-jitter backoff; stops for good on an auth failure
//! - detects sequence gaps and re-subscribes the same selector for a fresh snapshot; escalates to
//!   a reconnect if that snapshot does not come within `resync_timeout`
//! - the read loop parses each frame once (`request::parse_ws_message`), routes it, and enqueues the
//!   frame with the position of its payload; the consumer decodes only the payload. The read loop never
//!   waits for a consumer: each subscription has a bounded queue.
//!
//! One task owns the connection. Everything else talks to it through an unbounded channel of
//! outgoing frames tagged with the connection epoch, so a frame queued for a dead connection is
//! never sent on the next one.

use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::ops::Range;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::{SinkExt, Stream, StreamExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};

use super::auth::Session;
use super::error::{Error, Result};
use super::hooks::{Direction, Event, GapRange, Hooks};
use super::lite::Encoding;
use super::log::{debug_enabled, redact};
use super::protocol::{Backoff, Pending, RequestTable, Sequencer, Verdict};
use super::request::{decode_frame_as, ws_result, MessageKind, SubscribeAck, WsMessage};
use super::spec::{FeedFrame, StreamSpec};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Client ping interval. The server pings every 5s too.
pub const HEARTBEAT: Duration = Duration::from_secs(10);
/// No frame at all for this long: the connection is dead.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_INCOMING: usize = 64 << 20;
/// A connection that stayed up this long resets the reconnect backoff.
const STABLE_AFTER: Duration = Duration::from_secs(10);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Debug)]
enum Conn {
    Down,
    Up(u64),
    Fatal(Error),
    Closed,
}

enum Out {
    Frame {
        epoch: u64,
        text: String,
        rid: Option<u32>,
        dispatch: Option<Arc<AtomicU8>>,
        deadline: Option<Instant>,
    },
    Reconnect(String),
    Close,
}

// A cancelled waiter removes its pending entry and prevents a queued frame from starting.
struct PendingRequest<'a> {
    shared: &'a Shared,
    rid: u32,
    dispatch: Arc<AtomicU8>,
}
impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        let _ = self
            .dispatch
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire);
        lock(&self.shared.state).requests.pop(self.rid);
    }
}

fn begin_send(dispatch: Option<Arc<AtomicU8>>, deadline: Option<Instant>) -> bool {
    if deadline.is_some_and(|d| Instant::now() >= d) {
        return false;
    }
    dispatch.is_none_or(|state| {
        state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    })
}

enum Waiter {
    Rpc(oneshot::Sender<Result<String>>),
    Subscribe(Arc<Sub>),
    Ignore,
}

#[derive(Default)]
struct State {
    requests: RequestTable<Waiter>,
    /// The desired subscriptions, re-sent on every connect.
    subs: HashMap<u64, Arc<Sub>>,
    /// stream -> selector as the server echoes it -> subscription.
    by_key: HashMap<String, HashMap<String, Arc<Sub>>>,
    epoch: Option<u64>,
    closing: bool,
    session_gen: u64,
}

struct Shared {
    name: &'static str,
    url: String,
    session: Option<Arc<Session>>,
    hooks: Hooks,
    max_frame_bytes: Option<usize>,
    request_timeout: Duration,
    resync_timeout_ms: AtomicU64,
    encoding: Encoding,
    state: Mutex<State>,
    out_tx: mpsc::UnboundedSender<Out>,
    out_rx: Mutex<Option<mpsc::UnboundedReceiver<Out>>>,
    up: watch::Sender<Conn>,
    task: Mutex<Option<JoinHandle<()>>>,
    next_sub_id: AtomicU64,
}

/// A managed socket. Cheap to clone.
#[derive(Clone)]
pub struct WsSocket {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for WsSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WsSocket({}, {})", self.shared.name, self.shared.url)
    }
}

pub struct WsOptions {
    pub max_frame_bytes: Option<usize>,
    pub request_timeout: Duration,
    pub resync_timeout: Duration,
    /// Field names of every frame on this socket; `url` must serve that encoding.
    pub encoding: Encoding,
}

impl WsSocket {
    pub fn new(
        name: &'static str,
        url: String,
        session: Option<Arc<Session>>,
        hooks: Hooks,
        opts: WsOptions,
    ) -> WsSocket {
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (up, _) = watch::channel(Conn::Down);
        WsSocket {
            shared: Arc::new(Shared {
                name,
                url,
                session,
                hooks,
                max_frame_bytes: opts.max_frame_bytes,
                request_timeout: opts.request_timeout,
                resync_timeout_ms: AtomicU64::new(opts.resync_timeout.as_millis() as u64),
                encoding: opts.encoding,
                state: Mutex::default(),
                out_tx,
                out_rx: Mutex::new(Some(out_rx)),
                up,
                task: Mutex::new(None),
                next_sub_id: AtomicU64::new(1),
            }),
        }
    }

    pub fn name(&self) -> &'static str {
        self.shared.name
    }

    /// How long to wait for the snapshot after a gap re-subscribe before reconnecting.
    pub fn set_resync_timeout(&self, d: Duration) {
        self.shared
            .resync_timeout_ms
            .store(d.as_millis() as u64, Ordering::Relaxed);
    }

    /// Ask the socket to drop the connection and reconnect (subscriptions re-sync from snapshots).
    pub fn force_reconnect(&self, reason: &str) {
        let _ = self.shared.out_tx.send(Out::Reconnect(reason.to_owned()));
    }

    /// Send one JSON-RPC request and return the raw `result` JSON. `params_json` and the result are in the
    /// socket's encoding (`WsOptions::encoding`).
    pub async fn request(
        &self,
        method: &str,
        params_json: &[u8],
        write: bool,
        timeout: Option<Duration>,
        client_order_ids: Vec<u64>,
    ) -> Result<String> {
        self.request_tracked(
            method,
            params_json,
            write,
            timeout,
            client_order_ids,
            (Arc::new(AtomicU8::new(0)), None),
        )
        .await
    }

    pub(crate) async fn request_tracked(
        &self,
        method: &str,
        params_json: &[u8],
        write: bool,
        timeout: Option<Duration>,
        client_order_ids: Vec<u64>,
        tracking: (Arc<AtomicU8>, Option<Instant>),
    ) -> Result<String> {
        let (dispatch, deadline) = tracking;
        let sh = &self.shared;
        sh.start();
        let epoch = sh.wait_up(CONNECT_TIMEOUT).await?; // failure here: nothing was sent
        let (tx, mut rx) = oneshot::channel();
        // kept for a write whose reply races its timeout (see `unanswered`)
        let ids = if write { client_order_ids.clone() } else { Vec::new() };
        let rid = {
            let mut st = lock(&sh.state);
            if st.epoch != Some(epoch) {
                return Err(Error::transport("socket reconnecting", Some(method), false));
            }
            let rid = st
                .requests
                .add(Pending {
                    method: method.to_owned(),
                    write,
                    client_order_ids,
                    waiter: Waiter::Rpc(tx),
                })
                .ok_or_else(|| Error::transport("request table full", Some(method), false))?;
            let text = sh.encoding.jsonrpc_frame(method, params_json, rid);
            if let Some(limit) = sh.max_frame_bytes {
                if text.len() > limit {
                    // TODO: fall back to REST once the runtime can tell the RPC is safe to reroute.
                    st.requests.pop(rid);
                    return Err(Error::FrameTooLarge {
                        size: text.len(),
                        limit,
                    });
                }
            }
            sh.enqueue(epoch, text, Some(rid), Some(dispatch.clone()), deadline);
            rid
        };
        let _pending = PendingRequest {
            shared: sh,
            rid,
            dispatch,
        };
        let wait = timeout.unwrap_or(sh.request_timeout);
        match tokio::time::timeout(wait, &mut rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(Error::Closed),
            Err(_) => {
                let reason = format!("no reply within {wait:?}");
                let failed = lock(&sh.state).requests.timed_out(rid, &reason);
                match failed {
                    Some(failed) => Err(failed.error),
                    // The reader took the entry at the last moment and is handing over its answer (every
                    // path that takes an entry sends or drops the waiter right after).
                    None => match rx.await {
                        Ok(res) => res,
                        Err(_) => Err(unanswered(method, write, reason, ids)),
                    },
                }
            }
        }
    }

    pub(crate) async fn subscribe<F>(
        &self,
        spec: &'static StreamSpec,
        selector: String,
        capacity: usize,
    ) -> Result<Subscription<F>> {
        let sh = &self.shared;
        let sub = Arc::new(Sub {
            id: sh.next_sub_id.fetch_add(1, Ordering::Relaxed),
            spec,
            selector,
            capacity: capacity.max(1),
            inner: Mutex::default(),
        });
        let subscription = Subscription {
            sub: sub.clone(),
            socket: self.clone(),
            _f: PhantomData,
        };
        let (tx, rx) = oneshot::channel();
        lock(&sub.inner).ack = Some(tx);
        lock(&sh.state).subs.insert(sub.id, sub.clone());
        sh.start();
        let epoch = sh.wait_up(CONNECT_TIMEOUT).await?;
        {
            let mut st = lock(&sh.state);
            // A fresh connect re-sends every desired sub; only send if that did not already happen.
            let sent = lock(&sub.inner).sent_epoch == Some(epoch);
            if !sent && st.epoch == Some(epoch) {
                sh.subscribe_locked(&mut st, &sub, epoch);
            }
        }
        match tokio::time::timeout(sh.request_timeout, rx).await {
            Ok(Ok(Ok(()))) => Ok(subscription),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(Error::Closed),
            Err(_) => Err(Error::transport(
                format!("no subscribe reply within {:?}", sh.request_timeout),
                Some(&format!("subscribe {}", spec.stream)),
                true,
            )),
        }
    }

    pub(crate) async fn close(&self) {
        let sh = &self.shared;
        lock(&sh.state).closing = true;
        let _ = sh.out_tx.send(Out::Close);
        let task = lock(&sh.task).take();
        if let Some(t) = task {
            if tokio::time::timeout(Duration::from_secs(2), t).await.is_err() {
                tracing::debug!(target: "grvt", "{} socket did not close in time", sh.name);
            }
        }
        sh.fail_pending("client closed");
        let subs: Vec<_> = lock(&sh.state).subs.drain().map(|(_, s)| s).collect();
        for s in subs {
            s.end();
        }
        sh.up.send_replace(Conn::Closed);
    }

    pub(crate) fn abort(&self) {
        if let Some(t) = lock(&self.shared.task).take() {
            t.abort();
        }
    }
}

/// The error for a request whose waiter went away without an answer: a write keeps its client order ids.
fn unanswered(method: &str, write: bool, reason: String, client_order_ids: Vec<u64>) -> Error {
    if write {
        let cause = Error::transport(reason, Some(method), true);
        let mut error = Error::order_state_unknown(method, cause.to_string(), client_order_ids);
        if let Error::OrderStateUnknown { source, .. } = &mut error {
            *source = Some(Box::new(cause));
        }
        error
    } else {
        Error::transport(reason, Some(method), true)
    }
}

impl Shared {
    fn start(self: &Arc<Self>) {
        let mut task = lock(&self.task);
        if task.is_some() || lock(&self.state).closing {
            return;
        }
        if let Some(rx) = lock(&self.out_rx).take() {
            *task = Some(tokio::spawn(supervise(self.clone(), rx)));
        }
    }

    async fn wait_up(&self, timeout: Duration) -> Result<u64> {
        let mut rx = self.up.subscribe();
        let wait = async {
            loop {
                match &*rx.borrow_and_update() {
                    Conn::Up(e) => return Ok(*e),
                    Conn::Fatal(err) => return Err(err.clone()),
                    Conn::Closed => return Err(Error::Closed),
                    Conn::Down => {}
                }
                if rx.changed().await.is_err() {
                    return Err(Error::Closed);
                }
            }
        };
        tokio::time::timeout(timeout, wait).await.unwrap_or_else(|_| {
            Err(Error::transport(
                format!("{} socket not connected after {timeout:?}", self.name),
                None,
                false,
            ))
        })
    }

    fn enqueue(
        &self,
        epoch: u64,
        text: String,
        rid: Option<u32>,
        dispatch: Option<Arc<AtomicU8>>,
        deadline: Option<Instant>,
    ) {
        if self.hooks.on_event.is_some() {
            let raw: Arc<[u8]> = Arc::from(text.as_bytes());
            self.hooks.emit(|| Event::Frame {
                socket: self.name,
                direction: Direction::Out,
                raw,
            });
        }
        let _ = self.out_tx.send(Out::Frame {
            epoch,
            text,
            rid,
            dispatch,
            deadline,
        });
    }

    /// Register and queue a subscribe for `sub` on connection `epoch`. Caller holds the state lock.
    fn subscribe_locked(&self, st: &mut State, sub: &Arc<Sub>, epoch: u64) {
        {
            let mut si = lock(&sub.inner);
            si.seq.reset(); // drop every frame until the new subscribe reply arrives
            si.sent_epoch = Some(epoch);
        }
        let Some(rid) = st
            .requests
            .add(Pending::new("subscribe", false, Waiter::Subscribe(sub.clone())))
        else {
            return;
        };
        let text = self.encoding.subscribe_frame(sub.spec, &sub.selector, rid);
        self.enqueue(epoch, text, Some(rid), None, None);
    }

    fn unsubscribe(&self, sub: &Sub) {
        let mut st = lock(&self.state);
        st.subs.remove(&sub.id);
        let key = lock(&sub.inner).key.take();
        if let Some((stream, sel)) = key {
            if let Some(m) = st.by_key.get_mut(&stream) {
                if m.get(&sel).is_some_and(|s| s.id == sub.id) {
                    m.remove(&sel);
                }
            }
        }
        let Some(epoch) = st.epoch else { return };
        if let Some(rid) = st.requests.add(Pending::new("unsubscribe", false, Waiter::Ignore)) {
            let text = self.encoding.unsubscribe_frame(sub.spec, &sub.selector, rid);
            self.enqueue(epoch, text, Some(rid), None, None);
        }
    }

    async fn connect(
        &self,
    ) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>> {
        let mut req = self
            .url
            .as_str()
            .into_client_request()
            .map_err(|e| Error::Config(format!("bad ws url {}: {e}", self.url)))?;
        if let Some(s) = self.session.as_ref().filter(|s| s.enabled()) {
            s.ensure().await?;
            for (k, v) in s.headers() {
                if let (Ok(k), Ok(v)) = (HeaderName::from_str(&k), HeaderValue::from_str(&v)) {
                    req.headers_mut().insert(k, v);
                }
            }
            lock(&self.state).session_gen = s.generation();
        }
        let cfg = WebSocketConfig::default()
            .max_message_size(Some(MAX_INCOMING))
            .max_frame_size(Some(MAX_INCOMING));
        match tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async_with_config(req, Some(cfg), true),
        )
        .await
        {
            Err(_) => Err(Error::transport(
                format!("connect timeout after {CONNECT_TIMEOUT:?}"),
                None,
                false,
            )),
            Ok(Ok((ws, _))) => Ok(ws),
            Ok(Err(tungstenite::Error::Http(resp))) if matches!(resp.status().as_u16(), 401 | 403) => {
                // Rejected handshake (e.g. sticky-session check): retrying will not help.
                Err(Error::Api {
                    code: 1000,
                    message: format!("websocket handshake rejected: {}", resp.status()),
                    status: Some(resp.status().as_u16()),
                    rpc: None,
                    retryable: false,
                })
            }
            Ok(Err(e)) => Err(Error::transport(format!("connect failed: {e}"), None, false)),
        }
    }

    fn go_fatal(&self, err: Error) {
        self.up.send_replace(Conn::Fatal(err.clone()));
        self.hooks.emit(|| Event::Error {
            socket: Some(self.name),
            fatal: true,
            error: err.to_string(),
        });
        let subs: Vec<_> = lock(&self.state).subs.values().cloned().collect();
        for s in subs {
            s.fail(err.clone(), true);
        }
    }

    /// Fail everything still waiting for a reply. Writes become OrderStateUnknown.
    fn fail_pending(&self, reason: &str) {
        let failed = lock(&self.state).requests.connection_lost(reason);
        for f in failed {
            if let Waiter::Rpc(tx) = f.request.waiter {
                let _ = tx.send(Err(f.error));
            }
        }
    }

    /// A queued frame that was never written: its request provably did not leave the process.
    fn fail_unsent(&self, rid: Option<u32>, reason: &str) {
        let Some(rid) = rid else { return };
        let failed = lock(&self.state).requests.not_sent(rid, reason);
        if let Some(f) = failed {
            if let Waiter::Rpc(tx) = f.request.waiter {
                let _ = tx.send(Err(f.error));
            }
        }
    }

    // -- incoming (runs on the socket task; never waits) ----------------------------------------

    fn on_frame(&self, data: Bytes) {
        if self.hooks.on_event.is_some() {
            let raw: Arc<[u8]> = Arc::from(&data[..]);
            self.hooks.emit(|| Event::Frame {
                socket: self.name,
                direction: Direction::In,
                raw,
            });
        }
        if debug_enabled() {
            tracing::debug!(target: "grvt", "{} <- {}", self.name, redact(&data, 2000));
        }
        // The one parse of this frame: everything below routes from it.
        let msg = match self.encoding.parse_ws_message(&data) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(target: "grvt", "{} undecodable frame dropped: {e}", self.name);
                return;
            }
        };
        match msg.kind {
            MessageKind::Feed => {
                let (Some(stream), Some(selector)) = (msg.stream.as_deref(), msg.selector.as_deref()) else {
                    return;
                };
                let sub = lock(&self.state)
                    .by_key
                    .get(stream)
                    .and_then(|m| m.get(selector))
                    .cloned();
                let Some(sub) = sub else { return };
                let feed = msg.feed.map(|f| {
                    let start = f.as_ptr() as usize - data.as_ptr() as usize;
                    start..start + f.len()
                });
                let (seq, prev) = (msg.sequence_number, msg.prev_sequence_number);
                drop(msg);
                self.on_feed(&sub, seq, prev, data, feed);
            }
            MessageKind::Reply => {
                let id = msg.id.unwrap_or(0);
                let Some(p) = lock(&self.state).requests.pop(id) else {
                    tracing::debug!(target: "grvt", "{} reply for unknown id {id} (late or not ours)", self.name);
                    return;
                };
                match (ws_result(&msg, &p.method), p.waiter) {
                    (Ok(result), Waiter::Rpc(tx)) => {
                        let _ = tx.send(Ok(result.to_owned()));
                    }
                    (Err(err), Waiter::Rpc(tx)) => {
                        let _ = tx.send(Err(err));
                    }
                    (Ok(result), Waiter::Subscribe(sub)) => {
                        let ack = self
                            .encoding
                            .parse_subscribe_result(result, &sub.selector)
                            .unwrap_or_else(|_| SubscribeAck {
                                selector: sub.selector.clone(),
                                latest_sequence_number: 0,
                                num_snapshots: 0,
                            });
                        self.on_subscribe_reply(&sub, ack);
                    }
                    (Err(err), Waiter::Subscribe(sub)) => sub.fail(err, false),
                    (_, Waiter::Ignore) => {}
                }
            }
            MessageKind::Other => {}
        }
    }

    fn on_subscribe_reply(&self, sub: &Arc<Sub>, ack: SubscribeAck) {
        // Runs before the snapshot frames that follow the ack.
        let SubscribeAck {
            selector: canonical,
            latest_sequence_number: latest,
            num_snapshots: snaps,
        } = ack;
        {
            let mut st = lock(&self.state);
            if !st.subs.contains_key(&sub.id) {
                return; // unsubscribed meanwhile
            }
            let old = lock(&sub.inner)
                .key
                .replace((sub.spec.stream.to_owned(), canonical.clone()));
            if let Some((s, sel)) = old {
                if sel != canonical {
                    if let Some(m) = st.by_key.get_mut(&s) {
                        m.remove(&sel);
                    }
                }
            }
            st.by_key
                .entry(sub.spec.stream.to_owned())
                .or_default()
                .insert(canonical.clone(), sub.clone());
        }
        let ack = {
            let mut si = lock(&sub.inner);
            si.seq.on_ack(latest, snaps);
            si.selector = Arc::from(canonical.as_str());
            si.ack.take()
        };
        if let Some(ack) = ack {
            let _ = ack.send(Ok(()));
        }
        self.hooks.emit(|| Event::Subscribe {
            socket: self.name,
            stream: sub.spec.stream.into(),
            selector: canonical,
            latest_sequence_number: latest,
            num_snapshots: snaps,
        });
    }

    fn on_feed(&self, sub: &Arc<Sub>, seq: Option<u64>, prev: Option<u64>, data: Bytes, feed: Option<Range<usize>>) {
        let mut si = lock(&sub.inner);
        let frame = Queued {
            selector: si.selector.clone(),
            sequence_number: seq,
            prev_sequence_number: prev,
            data,
            feed,
            encoding: self.encoding,
        };
        match si.seq.on_frame(seq, prev) {
            Verdict::Deliver => {
                if !sub.push(&mut si, frame) {
                    drop(si);
                    self.resync(sub, "overflow");
                }
            }
            Verdict::Drop => {}
            Verdict::Gap => {
                let expected = si.seq.last;
                self.hooks.emit(|| Event::Gap {
                    gap: GapRange {
                        after_sequence: Some(expected),
                        through_sequence: if sub.spec.snapshot_feed {
                            prev.filter(|p| *p >= expected)
                        } else {
                            None
                        },
                        reason: "sequence_mismatch",
                    },
                    socket: self.name,
                    stream: sub.spec.stream.into(),
                    selector: sub.selector.clone(),
                    expected_prev: expected,
                    prev,
                    seq,
                });
                if sub.spec.snapshot_feed {
                    // every frame is complete; just move on
                    si.seq.last = seq.unwrap_or(si.seq.last);
                    sub.push(&mut si, frame);
                } else {
                    drop(si);
                    self.resync(sub, "gap");
                }
            }
        }
    }

    fn resync(&self, sub: &Arc<Sub>, why: &str) {
        if why == "overflow" {
            self.hooks.emit(|| Event::Gap {
                socket: self.name,
                stream: sub.spec.stream.into(),
                selector: sub.selector.clone(),
                expected_prev: 0,
                prev: None,
                seq: None,
                gap: GapRange {
                    after_sequence: None,
                    through_sequence: None,
                    reason: "overflow",
                },
            });
        }
        tracing::debug!(target: "grvt", "{} re-subscribing {} {} after {why}", self.name, sub.spec.stream, sub.selector);
        lock(&sub.inner).seq.resync_started = Some(Instant::now());
        let mut st = lock(&self.state);
        if let Some(epoch) = st.epoch {
            self.subscribe_locked(&mut st, sub, epoch);
        }
    }

    /// Why the connection should be dropped now, if at all.
    fn watchdog(&self) -> Option<String> {
        let limit = Duration::from_millis(self.resync_timeout_ms.load(Ordering::Relaxed));
        let now = Instant::now();
        let st = lock(&self.state);
        for sub in st.subs.values() {
            let mut si = lock(&sub.inner);
            if si.seq.resync_overdue(now, limit) {
                si.seq.resync_started = None;
                return Some(format!("no snapshot within {limit:?} of re-subscribe"));
            }
        }
        if let Some(s) = self.session.as_ref().filter(|s| s.enabled()) {
            // Break-before-make: wait for a moment with no writes in flight, then
            // reconnect with the new cookie; subscriptions re-sync from snapshots.
            if s.generation() != st.session_gen && st.requests.writes_in_flight() == 0 {
                return Some("session rotated".into());
            }
        }
        None
    }
}

async fn supervise(sh: Arc<Shared>, mut out_rx: mpsc::UnboundedReceiver<Out>) {
    let mut backoff = Backoff::default();
    let mut connected_once = false;
    let mut epoch = 0u64;
    loop {
        if lock(&sh.state).closing {
            return;
        }
        let ws = match sh.connect().await {
            Ok(ws) => ws,
            Err(e) => {
                if matches!(e, Error::Api { retryable: false, .. } | Error::Config(_)) {
                    sh.go_fatal(e);
                    return;
                }
                let delay = backoff.next_delay();
                tracing::debug!(target: "grvt", "{} connect failed ({e}); retry in {delay:?}", sh.name);
                sh.hooks.emit(|| Event::Retry {
                    socket: Some(sh.name),
                    rpc: None,
                    attempt: backoff.attempt,
                    delay,
                    error: e.to_string(),
                });
                if sleep_or_close(&sh, &mut out_rx, delay).await {
                    return;
                }
                continue;
            }
        };
        epoch += 1;
        {
            let mut st = lock(&sh.state);
            st.epoch = Some(epoch);
            let subs: Vec<_> = st.subs.values().cloned().collect();
            for sub in subs {
                sh.subscribe_locked(&mut st, &sub, epoch);
            }
        }
        sh.up.send_replace(Conn::Up(epoch));
        if connected_once {
            sh.hooks.emit(|| Event::Reconnect { socket: sh.name });
        } else {
            sh.hooks.emit(|| Event::Connect { socket: sh.name });
        }
        connected_once = true;

        let up_since = Instant::now();
        let (code, reason) = run(&sh, ws, &mut out_rx, epoch).await;
        if up_since.elapsed() > STABLE_AFTER {
            backoff.reset();
        }

        lock(&sh.state).epoch = None;
        sh.up.send_replace(Conn::Down);
        while let Ok(out) = out_rx.try_recv() {
            match out {
                Out::Frame { rid, .. } => sh.fail_unsent(rid, "socket closed before send"),
                Out::Close => lock(&sh.state).closing = true,
                Out::Reconnect(_) => {}
            }
        }
        sh.fail_pending(&format!(
            "socket closed ({reason}, code {})",
            code.map_or("-".into(), |c| c.to_string())
        ));
        let subs: Vec<_> = lock(&sh.state).subs.values().cloned().collect();
        for s in subs {
            let last = {
                let mut inner = lock(&s.inner);
                let last = inner.seq.last;
                inner.seq.reset();
                last
            };
            if !lock(&sh.state).closing {
                sh.hooks.emit(|| Event::Gap {
                    socket: sh.name,
                    stream: s.spec.stream.into(),
                    selector: s.selector.clone(),
                    expected_prev: 0,
                    prev: None,
                    seq: None,
                    gap: GapRange {
                        after_sequence: (last != 0).then_some(last),
                        through_sequence: None,
                        reason: "disconnect",
                    },
                });
            }
        }
        sh.hooks.emit(|| Event::Disconnect {
            socket: sh.name,
            code,
            reason: reason.clone(),
        });
        if lock(&sh.state).closing {
            return;
        }
        if sleep_or_close(&sh, &mut out_rx, backoff.next_delay()).await {
            return;
        }
    }
}

/// Sleep, but return true at once if a Close arrives. Other queued frames are failed as unsent.
async fn sleep_or_close(sh: &Shared, out_rx: &mut mpsc::UnboundedReceiver<Out>, delay: Duration) -> bool {
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return lock(&sh.state).closing,
            out = out_rx.recv() => match out {
                Some(Out::Close) | None => return true,
                Some(Out::Frame { rid, .. }) => sh.fail_unsent(rid, "socket not connected"),
                Some(Out::Reconnect(_)) => {}
            }
        }
    }
}

type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Drive one connection until it ends. Returns the close code and why it ended.
async fn run(
    sh: &Shared,
    ws: WsStream,
    out_rx: &mut mpsc::UnboundedReceiver<Out>,
    epoch: u64,
) -> (Option<u16>, String) {
    let (mut sink, mut stream) = ws.split();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut ping = tokio::time::interval(HEARTBEAT);
    ping.tick().await;
    let mut last_rx = Instant::now();
    loop {
        tokio::select! {
            msg = stream.next() => {
                last_rx = Instant::now();
                match msg {
                    Some(Ok(Message::Text(t))) => sh.on_frame(Bytes::from(t)),
                    Some(Ok(Message::Binary(b))) => sh.on_frame(b),
                    Some(Ok(Message::Close(frame))) => {
                        let code = frame.as_ref().map(|f| u16::from(f.code));
                        let reason = if code == Some(1008) { "slow consumer (server 1008)".into() } else { "closed by server".into() };
                        return (code, reason);
                    }
                    Some(Ok(_)) => {} // ping/pong: tungstenite answers pings itself
                    Some(Err(e)) => return (None, format!("read error: {e}")),
                    None => return (None, "closed".into()),
                }
            }
            out = out_rx.recv() => match out {
                Some(Out::Frame { epoch: e, text, rid, dispatch, deadline }) => {
                    if e != epoch {
                        sh.fail_unsent(rid, "socket reconnected before send");
                        continue;
                    }
                    if debug_enabled() {
                        tracing::debug!(target: "grvt", "{} -> {}", sh.name, redact(text.as_bytes(), 2000));
                    }
                    if !begin_send(dispatch, deadline) { sh.fail_unsent(rid, "call cancelled or deadline exceeded before send"); continue; }
                    if let Err(err) = sink.send(Message::text(text)).await {
                        // may or may not have left: the pending table fails it as sent
                        return (None, format!("send failed: {err}"));
                    }
                }
                Some(Out::Reconnect(reason)) => {
                    let _ = sink.close().await;
                    return (None, reason);
                }
                Some(Out::Close) | None => {
                    lock(&sh.state).closing = true;
                    let _ = sink.close().await;
                    return (Some(1000), "client closed".into());
                }
            },
            _ = tick.tick() => {
                if let Some(reason) = sh.watchdog() {
                    let _ = sink.close().await;
                    return (None, reason);
                }
                if last_rx.elapsed() > IDLE_TIMEOUT {
                    return (None, format!("no frames for {IDLE_TIMEOUT:?}"));
                }
            }
            _ = ping.tick() => {
                if let Err(err) = sink.send(Message::Ping(Bytes::new())).await {
                    return (None, format!("ping failed: {err}"));
                }
            }
        }
    }
}

// -- subscriptions ------------------------------------------------------------------------------

/// One delivered frame waiting for its consumer: the frame as received, where its payload is, and the
/// routing fields from the read loop's parse.
struct Queued {
    data: Bytes,
    feed: Option<Range<usize>>,
    selector: Arc<str>,
    sequence_number: Option<u64>,
    prev_sequence_number: Option<u64>,
    encoding: Encoding,
}

impl Queued {
    /// Decode the payload; the routing fields are not parsed again.
    fn decode<F: FeedFrame>(&self, stream: &'static str) -> Result<F> {
        let feed = match &self.feed {
            Some(r) => Some(std::str::from_utf8(&self.data[r.clone()]).map_err(|e| Error::Decode {
                rpc: stream.into(),
                message: e.to_string(),
            })?),
            None => None,
        };
        let msg = WsMessage {
            kind: MessageKind::Feed,
            id: None,
            stream: Some(stream.into()),
            selector: Some((*self.selector).into()),
            sequence_number: self.sequence_number,
            prev_sequence_number: self.prev_sequence_number,
            result: None,
            error: None,
            malformed_error: false,
            feed,
            text: &self.data,
            encoding: self.encoding,
        };
        decode_frame_as(stream, &msg)
    }
}

#[derive(Default)]
struct SubInner {
    queue: VecDeque<Queued>,
    /// The server's form of the selector (from the subscribe reply), as frames carry it.
    selector: Arc<str>,
    error: Option<Error>,
    closed: bool,
    waker: Option<Waker>,
    seq: Sequencer,
    key: Option<(String, String)>,
    ack: Option<oneshot::Sender<Result<()>>>,
    sent_epoch: Option<u64>,
    dropped: u64,
}

struct Sub {
    id: u64,
    spec: &'static StreamSpec,
    selector: String,
    capacity: usize,
    inner: Mutex<SubInner>,
}

impl Sub {
    /// Returns false when the consumer overflowed a stream that may not drop items.
    fn push(&self, si: &mut SubInner, raw: Queued) -> bool {
        if si.queue.len() >= self.capacity {
            if self.spec.conflate {
                si.queue.pop_front();
                si.dropped += 1;
            } else {
                si.queue.clear();
                si.error = Some(Error::StreamOverflow {
                    stream: self.spec.stream.into(),
                    selector: self.selector.clone(),
                    capacity: self.capacity,
                });
                wake(si);
                return false;
            }
        }
        si.queue.push_back(raw);
        wake(si);
        true
    }

    fn fail(&self, err: Error, end: bool) {
        let mut si = lock(&self.inner);
        if let Some(ack) = si.ack.take() {
            let _ = ack.send(Err(err)); // the waiting subscribe returns it
        } else {
            si.error = Some(err);
        }
        si.closed |= end;
        wake(&mut si);
    }

    fn end(&self) {
        let mut si = lock(&self.inner);
        si.closed = true;
        if let Some(ack) = si.ack.take() {
            let _ = ack.send(Err(Error::Closed));
        }
        wake(&mut si);
    }
}

fn wake(si: &mut SubInner) {
    if let Some(w) = si.waker.take() {
        w.wake();
    }
}

/// A live subscription: a `Stream` of decoded feed frames. Dropping it unsubscribes.
///
/// After a gap, re-subscription requests a fresh snapshot where supported. Older frames can remain
/// queued. A snapshot does not recover missed private events.
pub struct Subscription<F> {
    sub: Arc<Sub>,
    socket: WsSocket,
    _f: PhantomData<fn() -> F>,
}

impl<F> std::fmt::Debug for Subscription<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Subscription({} {})", self.sub.spec.stream, self.sub.selector)
    }
}

impl<F> Subscription<F> {
    pub fn stream_name(&self) -> &'static str {
        self.sub.spec.stream
    }

    pub fn selector(&self) -> &str {
        &self.sub.selector
    }

    /// Items dropped because the consumer was slow (conflating streams only).
    pub fn dropped(&self) -> u64 {
        lock(&self.sub.inner).dropped
    }

    /// Unsubscribe. Dropping the subscription does the same.
    pub fn close(self) {}
}

impl<F> Drop for Subscription<F> {
    fn drop(&mut self) {
        {
            let mut si = lock(&self.sub.inner);
            si.closed = true;
            si.error = None;
        }
        self.socket.shared.unsubscribe(&self.sub);
    }
}

impl<F: FeedFrame> Stream for Subscription<F> {
    type Item = Result<F>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut si = lock(&self.sub.inner);
        if let Some(e) = si.error.take() {
            return Poll::Ready(Some(Err(e)));
        }
        if let Some(frame) = si.queue.pop_front() {
            drop(si);
            return Poll::Ready(Some(frame.decode(self.sub.spec.stream)));
        }
        if si.closed {
            return Poll::Ready(None);
        }
        si.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelling_pending_request_removes_entry_and_skips_queued_frame() {
        for started in [false, true] {
            let socket = WsSocket::new(
                "td",
                "ws://localhost".into(),
                None,
                Hooks::default(),
                WsOptions {
                    max_frame_bytes: None,
                    request_timeout: Duration::from_secs(5),
                    resync_timeout: Duration::from_secs(5),
                    encoding: Encoding::Full,
                },
            );
            let (tx, rx) = oneshot::channel();
            let rid = lock(&socket.shared.state)
                .requests
                .add(Pending::new("write", true, Waiter::Rpc(tx)))
                .unwrap();
            let dispatch = Arc::new(AtomicU8::new(0));
            if started {
                assert!(begin_send(Some(dispatch.clone()), None));
            }
            drop(PendingRequest {
                shared: &socket.shared,
                rid,
                dispatch: dispatch.clone(),
            });
            assert_eq!(lock(&socket.shared.state).requests.len(), 0);
            assert!(rx.await.is_err());
            assert_eq!(dispatch.load(Ordering::Acquire), if started { 1 } else { 2 });
            assert!(!begin_send(Some(dispatch), None));
        }
    }

    #[test]
    fn expired_queued_frame_cannot_start() {
        let dispatch = Arc::new(AtomicU8::new(0));
        assert!(!begin_send(Some(dispatch.clone()), Some(Instant::now())));
        assert_eq!(dispatch.load(Ordering::Acquire), 0);
    }

    #[test]
    fn a_lost_write_keeps_its_ids() {
        let e = unanswered("v1/create_order", true, "no reply".into(), vec![4, 5]);
        assert!(matches!(e, Error::OrderStateUnknown { client_order_ids, .. } if client_order_ids == [4, 5]));
        assert!(matches!(
            unanswered("v1/instrument", false, "no reply".into(), Vec::new()),
            Error::Transport { sent: true, .. }
        ));
    }

    #[test]
    fn a_queued_frame_without_payload_does_not_decode() {
        let frame = Queued {
            data: Bytes::from_static(br#"{"stream":"v1.book.d","selector":"X","sequence_number":"1"}"#),
            feed: None,
            selector: Arc::from("X"),
            sequence_number: Some(1),
            prev_sequence_number: Some(0),
            encoding: Encoding::Full,
        };
        let out: Result<crate::models::WSOrderbookLevelsFeedDataV1> = frame.decode("v1.book.d");
        assert!(matches!(out, Err(Error::Decode { rpc, message }) if rpc == "v1.book.d" && message.contains("feed")));
    }
}
