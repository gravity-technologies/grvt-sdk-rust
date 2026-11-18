//! In-process fake gateway that speaks the Grvt WS protocol closely enough for tests.
//!
//! - subscribe -> ack (latest_sequence_number = last pushed, num_snapshots 1) then one snapshot frame
//! - unsubscribe -> ack
//! - any other method -> `{"result": {}}` unless the method is in `ignore_rpcs`
//! - `push` sends a delta to every connection; `drop_all` closes every connection
//! - a connection on `/ws/lite` speaks lite: incoming frames are renamed back to full names with the
//!   contract's lite names (a key that is not a lite name of its type is recorded in `lite_errors`), outgoing
//!   ones renamed to lite. `received` always holds full names; `received_lite` the frames as sent.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use grvt::Environment;
use serde_json::{json, Value as Json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use crate::lite_names::LiteNames;

enum Cmd {
    Send(String),
    Close(u16),
}

#[derive(Default)]
pub struct State {
    pub received: Vec<Json>,
    /// Frames from lite connections as they arrived.
    pub received_lite: Vec<Json>,
    /// Lite frames that used a key the contract does not have for their type.
    pub lite_errors: Vec<String>,
    /// Paths of the WebSocket handshakes.
    pub paths: Vec<String>,
    pub handshakes: Vec<HashMap<String, String>>,
    pub connect_count: usize,
    pub latest: HashMap<String, u64>,
    pub ignore_subscribes: usize,
    pub ignore_rpcs: HashSet<String>,
    pub reject_handshake: Option<u16>,
    /// When set, the next subscribe ack reports it as the latest sequence number.
    pub ack_latest: Option<u64>,
    conns: Vec<mpsc::UnboundedSender<Cmd>>,
}

#[derive(Clone)]
pub struct FakeGateway {
    pub state: Arc<Mutex<State>>,
    pub url: String,
}

impl FakeGateway {
    pub async fn start() -> FakeGateway {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws/full", listener.local_addr().unwrap());
        let gw = FakeGateway {
            state: Arc::default(),
            url,
        };
        let st = gw.state.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                tokio::spawn(serve(tcp, st.clone()));
            }
        });
        gw
    }

    /// REST goes to a FakeHttp in tests; only the WS URLs matter here.
    pub fn env(&self) -> Environment {
        Environment {
            name: "fake".into(),
            md_url: "http://fake".into(),
            td_url: "http://fake".into(),
            edge_url: "http://fake".into(),
            md_ws_url: self.url.clone(),
            td_ws_url: self.url.clone(),
            chain_id: 326,
            ws_max_frame_bytes: None,
        }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    pub fn subscribes(&self) -> Vec<Json> {
        self.with(|s| {
            s.received
                .iter()
                .filter(|r| r["method"] == "subscribe")
                .cloned()
                .collect()
        })
    }

    pub fn calls(&self, method: &str) -> usize {
        self.with(|s| s.received.iter().filter(|r| r["method"] == method).count())
    }

    pub fn connect_count(&self) -> usize {
        self.with(|s| s.connect_count)
    }

    /// Send a delta to every connection. Defaults: seq = last + 1, prev = last.
    pub fn push(&self, stream: &str, selector: &str, seq: Option<u64>, prev: Option<u64>) {
        self.with(|s| {
            let last = *s.latest.get(selector).unwrap_or(&100);
            let seq = seq.unwrap_or(last + 1);
            let prev = prev.unwrap_or(last);
            s.latest.insert(selector.into(), seq);
            let frame = json!({"stream": stream, "selector": selector, "sequence_number": seq.to_string(),
                               "prev_sequence_number": prev.to_string(), "feed": crate::fixtures::feed(stream)});
            for c in &s.conns {
                let _ = c.send(Cmd::Send(frame.to_string()));
            }
        });
    }

    pub fn send_raw(&self, frame: &str) {
        self.with(|s| {
            for c in &s.conns {
                let _ = c.send(Cmd::Send(frame.into()));
            }
        });
    }

    pub fn drop_all(&self) {
        self.with(|s| {
            for c in s.conns.drain(..) {
                let _ = c.send(Cmd::Close(1001));
            }
        });
    }
}

async fn serve(tcp: tokio::net::TcpStream, st: Arc<Mutex<State>>) {
    let st2 = st.clone();
    let lite = Arc::new(AtomicBool::new(false));
    let lite2 = lite.clone();
    #[allow(clippy::result_large_err)] // the callback type is tungstenite's
    let callback = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        lite2.store(req.uri().path().ends_with("/ws/lite"), Ordering::Relaxed);
        let headers = req
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
            .collect();
        let mut s = st2.lock().unwrap();
        s.handshakes.push(headers);
        s.paths.push(req.uri().path().to_owned());
        if let Some(code) = s.reject_handshake {
            let mut r = ErrorResponse::new(Some("rejected".into()));
            *r.status_mut() = StatusCode::from_u16(code).unwrap();
            return Err(r);
        }
        Ok(resp)
    };
    let Ok(ws) = tokio_tungstenite::accept_hdr_async(tcp, callback).await else {
        return;
    };
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel();
    st.lock().unwrap().connect_count += 1;
    st.lock().unwrap().conns.push(tx.clone());
    loop {
        tokio::select! {
            msg = stream.next() => {
                let Some(Ok(Message::Text(text))) = msg else {
                    if matches!(msg, Some(Ok(Message::Ping(_)) | Ok(Message::Pong(_)))) { continue }
                    return;
                };
                let mut data: Json = serde_json::from_str(&text).unwrap();
                if lite.load(Ordering::Relaxed) {
                    st.lock().unwrap().received_lite.push(data.clone());
                    match request_from_lite(&data) {
                        Ok(full) => data = full,
                        Err(e) => {
                            st.lock().unwrap().lite_errors.push(e);
                            continue;
                        }
                    }
                }
                let replies = handle(&st, &data);
                for r in replies {
                    let _ = tx.send(Cmd::Send(r));
                }
            }
            cmd = rx.recv() => match cmd {
                Some(Cmd::Send(t)) => {
                    let t = if lite.load(Ordering::Relaxed) { to_lite_frame(&t) } else { t };
                    if sink.send(Message::text(t)).await.is_err() { return }
                }
                Some(Cmd::Close(code)) => {
                    let _ = sink.send(Message::Close(Some(CloseFrame { code: CloseCode::from(code), reason: "bye".into() }))).await;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    return;
                }
                None => return,
            }
        }
    }
}

fn handle(st: &Mutex<State>, data: &Json) -> Vec<String> {
    let mut s = st.lock().unwrap();
    s.received.push(data.clone());
    let method = data["method"].as_str().unwrap_or("");
    let id = data["id"].clone();
    match method {
        "subscribe" => {
            if s.ignore_subscribes > 0 {
                s.ignore_subscribes -= 1;
                return vec![];
            }
            let stream = data["params"]["stream"].as_str().unwrap().to_owned();
            let sel = data["params"]["selectors"][0].as_str().unwrap().to_owned();
            let mut latest = *s.latest.entry(sel.clone()).or_insert(100);
            if let Some(ack) = s.ack_latest.take() {
                latest = ack;
                s.latest.insert(sel.clone(), ack);
            }
            vec![
                json!({"jsonrpc": "2.0", "id": id, "method": "subscribe", "result": {
                    "stream": stream, "subs": [sel], "unsubs": [], "num_snapshots": [1],
                    "first_sequence_number": [(latest + 1).to_string()], "latest_sequence_number": [latest.to_string()]}})
                .to_string(),
                json!({"stream": stream, "selector": sel, "sequence_number": "0", "prev_sequence_number":"0", "feed": crate::fixtures::feed(&stream)}).to_string(),
            ]
        }
        "unsubscribe" => vec![json!({"jsonrpc": "2.0", "id": id, "method": method,
            "result": {"stream": data["params"]["stream"], "unsubs": data["params"]["selectors"]}})
        .to_string()],
        m if s.ignore_rpcs.contains(m) => vec![],
        m => vec![json!({"jsonrpc": "2.0", "id": id, "method": m, "result": crate::fixtures::response(m)}).to_string()],
    }
}

fn names() -> &'static LiteNames {
    static NAMES: OnceLock<LiteNames> = OnceLock::new();
    NAMES.get_or_init(|| LiteNames::new(crate::fixtures::contract()))
}

/// The contract type of an RPC's request or response, by WS method.
fn rpc_type(method: &str, key: &str) -> Option<String> {
    crate::fixtures::contract()["rpcs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["ws_method"] == method)
        .map(|r| r[key].as_str().unwrap().to_owned())
}

/// A lite JSON-RPC request with full names, or which key is not lite.
fn request_from_lite(data: &Json) -> Result<Json, String> {
    let mut full = names().to_full("JSONRPCRequest", data)?;
    let method = full["method"].as_str().unwrap_or("").to_owned();
    let params_type = match method.as_str() {
        "subscribe" => Some("WSSubscribeParams".to_owned()),
        "unsubscribe" => Some("WSUnsubscribeParams".to_owned()),
        m => rpc_type(m, "request"),
    }
    .ok_or_else(|| format!("unknown method {method:?}"))?;
    full["params"] = names().to_full(&params_type, &full["params"])?;
    Ok(full)
}

/// A frame the fake built with full names, in lite: a JSON-RPC reply (its result typed by `method`) or a
/// stream frame (typed by its channel). Anything else goes out as is.
fn to_lite_frame(text: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Json>(text) else {
        return text.to_owned();
    };
    if v.get("jsonrpc").is_some() {
        let method = v["method"].as_str().unwrap_or("").to_owned();
        let result = v.as_object_mut().unwrap().remove("result");
        let mut out = names().to_lite_lenient("JSONRPCResponse", &v);
        if let Some(result) = result {
            let ty = match method.as_str() {
                "subscribe" => Some("WSSubscribeResult".to_owned()),
                "unsubscribe" => Some("WSUnsubscribeResult".to_owned()),
                m => rpc_type(m, "response"),
            };
            out["r"] = match ty {
                Some(ty) => names().to_lite_lenient(&ty, &result),
                None => result,
            };
        }
        return out.to_string();
    }
    if let Some(channel) = v.get("stream").and_then(Json::as_str) {
        let feed_type = crate::fixtures::contract()["streams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["channel"] == channel)
            .map(|s| s["feed_type"].as_str().unwrap().to_owned());
        if let Some(ty) = feed_type {
            return names().to_lite_lenient(&ty, &v).to_string();
        }
    }
    text.to_owned()
}

pub async fn wait_until(what: &str, mut pred: impl FnMut() -> bool) {
    for _ in 0..500 {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not met in time: {what}");
}
