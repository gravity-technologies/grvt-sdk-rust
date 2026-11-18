//! The async client runtime (feature `client`): `Config`, the `Runtime` every generated `Grvt` method
//! calls, the metadata cache, and the `RpcCall` / `StreamCall` builders. It does the I/O and drives the
//! core: requests, replies, signing and session state all go through the same functions an application
//! without the client uses.

use std::collections::HashMap;
use std::future::{Future, IntoFuture};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use serde::de::DeserializeOwned;
use tokio::sync::OnceCell;
use tokio::task::JoinHandle;

use crate::generated::behavior;

use super::auth::Session;
use super::clock::{ClientOrderIdProvider, Clock, NonceProvider, RandomNonce, SystemClock, TimeCounterClientOrderId};
use super::codec;
use super::env::Environment;
use super::error::{Error, Result};
use super::hooks::{Call, Event, Hooks, Via};
use super::http::{HttpTransport, ReqwestTransport};
use super::lite::{Encoding, LiteDecode};
use super::log;
use super::request::{prepare_request, stale_instruments, Request, RequestDefaults};
use super::rest;
use super::signing::{
    grvt_domain, verify_signature, CurrencyMeta, Domain, InstrumentMeta, Meta, PrepareContext, SignSpec, Signer,
    SigningNeeds,
};
use super::spec::{FeedFrame, Gateway, RpcSpec, StreamSpec};
use super::ws::{self, Subscription, WsSocket};
use crate::Timestamp;

/// How order-entry writes travel. Chosen once per client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OrderTransport {
    #[default]
    Rest,
    /// Over the trading WebSocket (lower latency). A drop or timeout is `Error::OrderStateUnknown`.
    Ws,
}

/// Client options. `Config::testnet().api_key("...").signer(...)`.
#[non_exhaustive]
pub struct Config {
    pub sub_account_id: Option<u64>,
    pub env: Environment,
    pub api_key: Option<String>,
    pub signer: Option<Arc<dyn Signer>>,
    pub order_transport: OrderTransport,
    /// JSON field names on the wire (default `Encoding::Lite`). Lite switches every gateway call and stream;
    /// edge login stays full. Models and errors use full names either way.
    pub encoding: Encoding,
    /// Check each order against its instrument's limits before signing (default true).
    pub check_orders: bool,
    pub clock: Arc<dyn Clock>,
    pub nonces: Arc<dyn NonceProvider>,
    pub client_order_ids: Arc<dyn ClientOrderIdProvider>,
    pub http: Option<Arc<dyn HttpTransport>>,
    pub hooks: Hooks,
    pub http_attempt_timeout: Duration,
    pub call_timeout: Duration,
    pub ws_request_timeout: Duration,
    /// Wait for a fresh snapshot after a gap re-subscribe; past it the socket reconnects.
    pub resync_timeout: Duration,
    /// Per-subscription queue bound.
    pub queue_size: usize,
}

impl Config {
    pub fn new(env: Environment) -> Config {
        Config {
            env,
            api_key: None,
            sub_account_id: None,
            signer: None,
            order_transport: OrderTransport::Rest,
            encoding: Encoding::Lite,
            check_orders: true,
            clock: Arc::new(SystemClock::new()),
            nonces: Arc::new(RandomNonce),
            client_order_ids: Arc::new(TimeCounterClientOrderId::default()),
            http: None,
            hooks: Hooks::default(),
            http_attempt_timeout: behavior::HTTP_ATTEMPT_TIMEOUT,
            call_timeout: behavior::CALL_TIMEOUT,
            ws_request_timeout: behavior::WS_REQUEST_TIMEOUT,
            resync_timeout: behavior::RESYNC_TIMEOUT,
            queue_size: behavior::QUEUE_SIZE,
        }
    }

    pub fn testnet() -> Config {
        Config::new(Environment::testnet())
    }

    pub fn prod() -> Config {
        Config::new(Environment::prod())
    }

    /// `"testnet"` or `"prod"`.
    pub fn named(env: &str) -> Result<Config> {
        Environment::named(env).map(Config::new)
    }

    /// Default for omitted request and stream account IDs. Explicit IDs win.
    pub fn sub_account_id(mut self, id: u64) -> Self {
        self.sub_account_id = Some(id);
        self
    }

    /// Logs in on the first authenticated call.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Signs orders, transfers and withdrawals.
    pub fn signer(mut self, signer: impl Signer + 'static) -> Self {
        self.signer = Some(Arc::new(signer));
        self
    }

    pub fn order_transport(mut self, t: OrderTransport) -> Self {
        self.order_transport = t;
        self
    }

    /// Short (lite) or full JSON field names on the wire, for every gateway call and stream (default lite).
    pub fn encoding(mut self, e: Encoding) -> Self {
        self.encoding = e;
        self
    }

    /// `false` skips the instrument-limit check before signing and leaves the limits to the exchange.
    pub fn check_orders(mut self, on: bool) -> Self {
        self.check_orders = on;
        self
    }

    pub fn clock(mut self, c: impl Clock + 'static) -> Self {
        self.clock = Arc::new(c);
        self
    }

    pub fn nonces(mut self, n: impl NonceProvider + 'static) -> Self {
        self.nonces = Arc::new(n);
        self
    }

    pub fn client_order_ids(mut self, p: impl ClientOrderIdProvider + 'static) -> Self {
        self.client_order_ids = Arc::new(p);
        self
    }

    /// HTTP plug-in (default: `ReqwestTransport`).
    pub fn http(mut self, t: impl HttpTransport + 'static) -> Self {
        self.http = Some(Arc::new(t));
        self
    }

    /// Runs before signing for every RPC; may change the request.
    pub fn before_send(mut self, f: impl Fn(&mut Call) + Send + Sync + 'static) -> Self {
        self.hooks.before_send = Some(Arc::new(f));
        self
    }

    /// Connection, stream and retry events. Must not block.
    pub fn on_event(mut self, f: impl Fn(&Event) + Send + Sync + 'static) -> Self {
        self.hooks.on_event = Some(Arc::new(f));
        self
    }

    pub fn validate(&self) -> Result<()> {
        self.env.validate()?;
        for g in [Gateway::MarketData, Gateway::Trading] {
            self.env.ws_url_for(g, self.encoding)?;
        }
        if self.call_timeout.is_zero()
            || self.http_attempt_timeout.is_zero()
            || self.ws_request_timeout.is_zero()
            || self.resync_timeout.is_zero()
        {
            return Err(Error::Config(
                "call, HTTP attempt, WS reply and resync timeouts must be positive".into(),
            ));
        }
        Ok(())
    }

    /// Bound one complete RPC, including preparation and retries (default 30 seconds).
    pub fn call_timeout(mut self, d: Duration) -> Self {
        self.call_timeout = d;
        self
    }

    /// Bound each HTTP attempt, including login (default 10 seconds).
    pub fn http_attempt_timeout(mut self, d: Duration) -> Self {
        self.http_attempt_timeout = d;
        self
    }

    pub fn ws_request_timeout(mut self, d: Duration) -> Self {
        self.ws_request_timeout = d;
        self
    }

    /// Bound the wait for a fresh snapshot after a gap re-subscribe before reconnecting (default 5 seconds).
    pub fn resync_timeout(mut self, d: Duration) -> Self {
        self.resync_timeout = d;
        self
    }

    pub fn queue_size(mut self, n: usize) -> Self {
        self.queue_size = n;
        self
    }
}

/// Metadata needed to sign (instrument hash, decimals, the session's main account). Loads once per
/// key; concurrent callers share one load; a failed load is not cached.
#[derive(Debug, Default)]
pub struct MetadataCache {
    instruments: Mutex<HashMap<String, Arc<OnceCell<InstrumentMeta>>>>,
    currencies: Mutex<HashMap<String, Arc<OnceCell<CurrencyMeta>>>>,
    main_account: OnceCell<String>,
}

fn cell<T>(m: &Mutex<HashMap<String, Arc<OnceCell<T>>>>, key: &str) -> Arc<OnceCell<T>> {
    let mut m = m.lock().unwrap_or_else(|e| e.into_inner());
    // A hit copies no key.
    if let Some(c) = m.get(key) {
        return c.clone();
    }
    m.entry(key.to_owned()).or_default().clone()
}

impl MetadataCache {
    pub async fn instrument<Fut>(&self, name: &str, load: impl FnOnce() -> Fut) -> Result<InstrumentMeta>
    where
        Fut: Future<Output = Result<InstrumentMeta>>,
    {
        cell(&self.instruments, name).get_or_try_init(load).await.cloned()
    }

    pub async fn currency<Fut>(&self, symbol: &str, load: impl FnOnce() -> Fut) -> Result<CurrencyMeta>
    where
        Fut: Future<Output = Result<CurrencyMeta>>,
    {
        cell(&self.currencies, symbol).get_or_try_init(load).await.cloned()
    }

    pub fn put_instrument(&self, name: &str, meta: InstrumentMeta) {
        let _ = cell(&self.instruments, name).set(meta);
    }

    /// Forget an instrument; the next call that needs it loads it again.
    pub fn drop_instrument(&self, name: &str) {
        self.instruments.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
    }

    pub fn put_currency(&self, symbol: &str, meta: CurrencyMeta) {
        let _ = cell(&self.currencies, symbol).set(meta);
    }

    /// The session's main account address (contract `signed.main_account`), loaded once.
    pub async fn main_account<Fut>(&self, load: impl FnOnce() -> Fut) -> Result<String>
    where
        Fut: Future<Output = Result<String>>,
    {
        self.main_account.get_or_try_init(load).await.cloned()
    }

    pub fn put_main_account(&self, address: &str) {
        let _ = self.main_account.set(address.to_owned());
    }
}

/// How the client loads signing metadata: generated (`client.rs`), one RPC each.
#[doc(hidden)]
pub struct MetadataLoaders {
    pub instrument: for<'r> fn(&'r Runtime, String) -> BoxFuture<'r, Result<InstrumentMeta>>,
    pub currency: for<'r> fn(&'r Runtime, String) -> BoxFuture<'r, Result<CurrencyMeta>>,
    pub main_account: for<'r> fn(&'r Runtime) -> BoxFuture<'r, Result<String>>,
}

impl std::fmt::Debug for MetadataLoaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MetadataLoaders")
    }
}

struct Inner {
    sub_account_id: Option<u64>,
    env: Environment,
    signer: Option<Arc<dyn Signer>>,
    order_transport: OrderTransport,
    encoding: Encoding,
    check_orders: bool,
    clock: Arc<dyn Clock>,
    nonces: Arc<dyn NonceProvider>,
    client_order_ids: Arc<dyn ClientOrderIdProvider>,
    http: Arc<dyn HttpTransport>,
    hooks: Hooks,
    http_attempt_timeout: Duration,
    call_timeout: Duration,
    queue_size: usize,
    metadata: MetadataCache,
    loaders: &'static MetadataLoaders,
    session: Arc<Session>,
    domain: Domain,
    md: WsSocket,
    td: WsSocket,
    rotation: Mutex<Option<JoinHandle<()>>>,
    session_retry_delay: Duration,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.md.abort();
        self.td.abort();
        if let Some(t) = self.rotation.lock().unwrap_or_else(|e| e.into_inner()).take() {
            t.abort();
        }
    }
}

#[cfg(test)]
impl Runtime {
    /// The session login timeout and the wait after a failed renewal, for the shared-behavior test.
    pub(crate) fn session_timings(&self) -> (Duration, Duration) {
        (self.0.session.http_attempt_timeout(), self.0.session_retry_delay)
    }
}

/// The one object generated clients hold. Same size however many RPCs or streams exist.
/// Cheap to clone. Needs a tokio runtime once it does I/O.
#[derive(Clone)]
pub struct Runtime(Arc<Inner>);

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("env", &self.0.env.name)
            .field("order_transport", &self.0.order_transport)
            .field("encoding", &self.0.encoding)
            .finish()
    }
}

impl Runtime {
    pub fn sub_account_id(&self) -> Option<u64> {
        self.0.sub_account_id
    }
    /// `loaders` are the generated metadata loaders.
    pub fn new(config: Config, loaders: &'static MetadataLoaders) -> Runtime {
        log::configure_from_env();
        let http: Arc<dyn HttpTransport> = config.http.unwrap_or_else(|| Arc::new(ReqwestTransport::default()));
        let env = config.env;
        let session = Arc::new(Session::new(
            config.api_key,
            env.clone(),
            http.clone(),
            config.http_attempt_timeout,
        ));
        let encoding = config.encoding;
        let socket = |g: Gateway| {
            WsSocket::new(
                g.short(),
                // `Config::validate` (run by `Grvt::new`) rejects a URL with no lite form; an unvalidated
                // config fails on connect instead.
                env.ws_url_for(g, encoding).unwrap_or_default(),
                Some(session.clone()),
                config.hooks.clone(),
                ws::WsOptions {
                    max_frame_bytes: env.ws_max_frame_bytes,
                    request_timeout: config.ws_request_timeout,
                    resync_timeout: config.resync_timeout,
                    encoding,
                },
            )
        };
        let (md, td) = (socket(Gateway::MarketData), socket(Gateway::Trading));
        Runtime(Arc::new(Inner {
            domain: grvt_domain(env.chain_id),
            env,
            signer: config.signer,
            order_transport: config.order_transport,
            encoding,
            check_orders: config.check_orders,
            sub_account_id: config.sub_account_id,
            clock: config.clock,
            nonces: config.nonces,
            client_order_ids: config.client_order_ids,
            http,
            hooks: config.hooks,
            http_attempt_timeout: config.http_attempt_timeout,
            call_timeout: config.call_timeout,
            queue_size: config.queue_size,
            metadata: MetadataCache::default(),
            loaders,
            session,
            md,
            td,
            rotation: Mutex::new(None),
            session_retry_delay: behavior::SESSION_RETRY_DELAY,
        }))
    }

    pub fn env(&self) -> &Environment {
        &self.0.env
    }

    pub fn signer(&self) -> Option<&Arc<dyn Signer>> {
        self.0.signer.as_ref()
    }

    pub fn clock(&self) -> &dyn Clock {
        &*self.0.clock
    }

    pub fn nonces(&self) -> &dyn NonceProvider {
        &*self.0.nonces
    }

    pub fn client_order_ids(&self) -> &dyn ClientOrderIdProvider {
        &*self.0.client_order_ids
    }

    pub fn metadata(&self) -> &MetadataCache {
        &self.0.metadata
    }

    pub fn session(&self) -> &Session {
        &self.0.session
    }

    pub fn order_transport(&self) -> OrderTransport {
        self.0.order_transport
    }

    /// The client's wire encoding (edge routes are always full).
    pub fn encoding(&self) -> Encoding {
        self.0.encoding
    }

    /// Whether orders are checked against their instrument's limits before signing.
    pub fn check_orders(&self) -> bool {
        self.0.check_orders
    }

    /// The managed socket for a gateway.
    pub fn socket(&self, gateway: Gateway) -> &WsSocket {
        match gateway {
            Gateway::MarketData => &self.0.md,
            _ => &self.0.td,
        }
    }

    /// The EIP-712 domain for this environment's chain id.
    pub fn domain(&self) -> &Domain {
        &self.0.domain
    }

    /// Close both sockets and stop background tasks. Pending writes become OrderStateUnknown.
    pub async fn close(&self) {
        if let Some(t) = self.0.rotation.lock().unwrap_or_else(|e| e.into_inner()).take() {
            t.abort();
        }
        self.0.md.close().await;
        self.0.td.close().await;
    }

    // -- RPC ----------------------------------------------------------------------------------

    /// One RPC: `before_send` -> sign -> encode -> REST or WS -> decode.
    pub async fn call<Req, Resp>(
        &self,
        spec: &'static RpcSpec,
        req: Req,
        sign: Option<&SignSpec<Req>>,
        timeout: Option<Duration>,
    ) -> Result<Resp>
    where
        Req: Request + 'static,
        Resp: DeserializeOwned + LiteDecode,
    {
        let duration = timeout.unwrap_or(self.0.call_timeout);
        if duration.is_zero() || self.0.http_attempt_timeout.is_zero() {
            return Err(Error::Config("call and HTTP attempt timeouts must be positive".into()));
        }
        let deadline = Instant::now()
            .checked_add(duration)
            .ok_or_else(|| Error::Config("call timeout is too large".into()))?;
        let mut dispatched = None;
        let dispatch = Arc::new(AtomicU8::new(0));
        match tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.call_inner(spec, req, sign, deadline, &mut dispatched, &dispatch),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                let error = Error::transport(
                    "call deadline exceeded",
                    Some(spec.method),
                    dispatch.load(Ordering::Acquire) == 1,
                );
                Err(super::request::write_outcome(
                    spec,
                    error,
                    dispatched.unwrap_or_default(),
                ))
            }
        }
    }

    async fn ensure_ready_for_send(&self, spec: &'static RpcSpec) -> Result<()> {
        if spec.auth {
            self.ensure_session().await?;
        }
        Ok(())
    }

    async fn call_inner<Req, Resp>(
        &self,
        spec: &'static RpcSpec,
        mut req: Req,
        sign: Option<&SignSpec<Req>>,
        deadline: Instant,
        dispatched: &mut Option<Vec<u64>>,
        dispatch: &Arc<AtomicU8>,
    ) -> Result<Resp>
    where
        Req: Request + 'static,
        Resp: DeserializeOwned + LiteDecode,
    {
        let defaults = RequestDefaults {
            sub_account_id: self.sub_account_id(),
            client_order_ids: Some(&*self.0.client_order_ids),
        };
        prepare_request(spec, &mut req, &defaults)?;
        let via = if spec.order_entry && self.0.order_transport == OrderTransport::Ws {
            Via::Ws
        } else {
            Via::Rest
        };
        if let Some(hook) = &self.0.hooks.before_send {
            let params = serde_json::to_value(&req).map_err(|e| Error::Config(format!("before_send: {e}")))?;
            let mut call = Call {
                rpc: spec.method,
                params,
                transport: via,
            };
            hook(&mut call);
            req = serde_json::from_value(call.params)
                .map_err(|e| Error::Config(format!("before_send changed the request into something invalid: {e}")))?;
        }
        if Instant::now() >= deadline {
            return Err(Error::transport(
                "call deadline exceeded before signing",
                Some(spec.method),
                false,
            ));
        }
        if let Some(sign) = sign {
            req = self.sign(req, sign).await?; // nothing may change the request after this line
        }
        let body = serde_json::to_vec(&req).map_err(|e| Error::Config(format!("cannot encode request: {e}")))?;
        let final_request: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| Error::Config(e.to_string()))?;
        let orders: Vec<&serde_json::Value> = final_request
            .get("orders")
            .and_then(|v| v.as_array())
            .map(|v| v.iter().collect())
            .unwrap_or_else(|| final_request.get("order").into_iter().collect());
        let client_order_ids: Vec<u64> = orders
            .into_iter()
            .filter_map(|o| o.pointer("/metadata/client_order_id"))
            .filter_map(|id| id.as_u64().or_else(|| id.as_str()?.parse().ok()))
            .collect();
        let refresh = (!spec.refresh_metadata_on.is_empty()).then(|| body.clone());
        // The ids and stale instruments above are read from the full JSON; the wire gets this call's encoding.
        let encoding = spec.encoding(self.0.encoding);
        let body = match encoding {
            Encoding::Full => body,
            Encoding::Lite => encoding
                .to_vec(&req)
                .map_err(|e| Error::Config(format!("cannot encode request: {e}")))?,
        };
        if Instant::now() >= deadline {
            return Err(Error::transport(
                "call deadline exceeded before dispatch",
                Some(spec.method),
                false,
            ));
        }
        self.ensure_ready_for_send(spec).await?;
        if Instant::now() >= deadline {
            return Err(Error::transport(
                "call deadline exceeded before dispatch",
                Some(spec.method),
                false,
            ));
        }
        *dispatched = Some(client_order_ids.clone());
        let out = self.send(spec, via, body, client_order_ids, dispatch, deadline).await;
        if let (Err(e), Some(body)) = (&out, refresh) {
            for name in stale_instruments(spec, e, &body) {
                self.0.metadata.drop_instrument(&name);
            }
        }
        out
    }

    async fn send<Resp: DeserializeOwned + LiteDecode>(
        &self,
        spec: &'static RpcSpec,
        via: Via,
        body: Vec<u8>,
        client_order_ids: Vec<u64>,
        dispatch: &Arc<AtomicU8>,
        deadline: Instant,
    ) -> Result<Resp> {
        let outcome_ids = client_order_ids.clone();
        let raw_result: Result<Vec<u8>> = async {
            Ok(match via {
                Via::Ws => {
                    let text = self
                        .0
                        .td
                        .request_tracked(
                            spec.method,
                            &body,
                            spec.write,
                            None,
                            client_order_ids,
                            (dispatch.clone(), Some(deadline)),
                        )
                        .await?;
                    text.into_bytes()
                }
                Via::Rest => {
                    let request = self.0.session.with_state(|s| {
                        self.0
                            .encoding
                            .rest_request_body(spec, &self.0.env, body, spec.auth.then_some(s))
                    })?;
                    rest::rest_call(rest::RestCall {
                        http: &*self.0.http,
                        request,
                        spec,
                        timeout: self.0.http_attempt_timeout,
                        hooks: &self.0.hooks,
                        client_order_ids,
                        deadline,
                        dispatch,
                        encoding: self.0.encoding,
                    })
                    .await?
                }
            })
        }
        .await;
        raw_result
            .and_then(|raw| self.0.encoding.decode_response(spec, &raw))
            .map_err(|e| super::request::write_outcome(spec, e, outcome_ids))
    }

    // -- signing ------------------------------------------------------------------------------

    /// Sign a request the way RPC methods do: load the metadata it needs (cached), fill and hash it with
    /// the core sign functions, then sign each digest with the client's `Signer` (`sign_prehash`, else
    /// `sign_typed` with the typed message, whose signature must recover to the signer's address). Users
    /// reach it through the generated `Grvt::sign_<call>`.
    pub(crate) async fn sign<Req>(&self, mut req: Req, spec: &SignSpec<Req>) -> Result<Req> {
        if (spec.count)(&req) == 0 {
            return Ok(req); // e.g. bulk orders without orders: nothing to sign
        }
        let signer = self
            .0
            .signer
            .clone()
            .ok_or_else(|| Error::Config("this call needs a signer: set Config::signer".into()))?;
        let meta = self.signing_metadata((spec.needs)(&req)).await?;
        let pending = {
            let ctx = PrepareContext {
                address: signer.address(),
                metadata: &meta,
                chain_id: self.0.env.chain_id,
                now: Some(Timestamp::from_nanoseconds(self.0.clock.now_ns())),
                nonces: Some(&*self.0.nonces),
                check_orders: self.0.check_orders,
            };
            let prepared = (spec.prepare)(&mut req, &ctx, true)?;
            // only plain data crosses the awaits below
            prepared
                .into_iter()
                .map(|p| (p.digest, p.chain_id, p.struct_type))
                .collect::<Vec<_>>()
        };
        let mut signatures = Vec::with_capacity(pending.len());
        for (i, (digest, chain_id, struct_type)) in pending.into_iter().enumerate() {
            let signature = match signer.sign_prehash(&digest) {
                Some(s) => s?,
                None => {
                    // A wallet/KMS signer gets the typed message; its signature must recover to its
                    // address against the digest the generated encoder computed.
                    let message = (spec.message)(&req, &meta, i)?;
                    let s = if chain_id == self.0.env.chain_id {
                        signer.sign_typed(&self.0.domain, struct_type, &message).await?
                    } else {
                        signer.sign_typed(&grvt_domain(chain_id), struct_type, &message).await?
                    };
                    verify_signature(&digest, &s, signer.address())?;
                    s
                }
            };
            signatures.push(signature);
        }
        (spec.commit)(&mut req, signatures);
        Ok(req)
    }

    /// The metadata a sign call needs, from the cache or loaded once (main account, instruments,
    /// currencies, in that order).
    async fn signing_metadata(&self, needs: SigningNeeds) -> Result<Meta> {
        let mut meta = Meta::default();
        let loaders = self.0.loaders;
        if needs.main_account {
            meta.main_account = Some(self.0.metadata.main_account(|| (loaders.main_account)(self)).await?);
        }
        for name in needs.instruments {
            let v = self
                .0
                .metadata
                .instrument(&name, || (loaders.instrument)(self, name.clone()))
                .await?;
            meta.instruments.insert(name, v);
        }
        for symbol in needs.currencies {
            let v = self
                .0
                .metadata
                .currency(&symbol, || (loaders.currency)(self, symbol.clone()))
                .await?;
            meta.currencies.insert(symbol, v);
        }
        Ok(meta)
    }

    // -- session ------------------------------------------------------------------------------

    async fn ensure_session(&self) -> Result<()> {
        if !self.0.session.enabled() {
            return Err(Error::Config("this call needs a session: set Config::api_key".into()));
        }
        self.0.session.ensure().await?;
        let mut rot = self.0.rotation.lock().unwrap_or_else(|e| e.into_inner());
        if rot.is_none() {
            *rot = Some(tokio::spawn(rotate_forever(
                self.0.session.clone(),
                self.0.hooks.clone(),
                self.0.session_retry_delay,
            )));
        }
        Ok(())
    }

    // -- clock --------------------------------------------------------------------------------

    /// GET {md}/time (`{"server_time": ms}`) and feed it to `clock` (e.g. a `SystemClock`).
    pub async fn sync_clock(&self, clock: &SystemClock) -> Result<()> {
        let t0 = Instant::now();
        let resp = self
            .0
            .http
            .get(&format!("{}/time", self.0.env.md_url), self.0.http_attempt_timeout)
            .await?;
        let rtt_ms = t0.elapsed().as_secs_f64() * 1000.0;
        #[derive(serde::Deserialize)]
        struct T {
            #[serde(with = "codec::opt_i64")]
            server_time: Option<i64>,
        }
        if resp.status == 200 {
            if let Ok(T { server_time: Some(ms) }) = serde_json::from_slice::<T>(&resp.body) {
                clock.observe(ms, rtt_ms);
            }
        }
        Ok(())
    }
}

async fn rotate_forever(session: Arc<Session>, hooks: Hooks, retry_delay: Duration) {
    loop {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let wait = (session.snapshot().refresh_due() - now).max(1.0);
        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        match session.login().await {
            Ok(()) => {
                let s = session.snapshot();
                hooks.emit(|| Event::SessionRotated {
                    websocket_reconnect_required: true,
                    generation: s.generation,
                    expires_at: UNIX_EPOCH + Duration::from_secs_f64(s.expires_at.max(0.0)),
                });
            }
            Err(e) => {
                tracing::warn!(target: "grvt", "session re-login failed: {e}");
                hooks.emit(|| Event::Error {
                    socket: None,
                    fatal: false,
                    error: format!("session: {e}"),
                });
                tokio::time::sleep(retry_delay).await;
            }
        }
    }
}

/// One RPC call, ready to send: set fields with the generated setters, then `.await` it.
/// Required fields are checked before anything is signed or sent.
#[must_use = "an RpcCall does nothing until awaited"]
pub struct RpcCall<'a, Req, Resp> {
    pub(crate) rt: &'a Runtime,
    pub(crate) spec: &'static RpcSpec,
    pub(crate) req: Req,
    pub(crate) provided: Option<std::collections::HashSet<&'static str>>,
    pub(crate) sign: Option<&'a SignSpec<Req>>,
    pub(crate) timeout: Option<Duration>,
    _resp: PhantomData<fn() -> Resp>,
}

impl<'a, Req: Default, Resp> RpcCall<'a, Req, Resp> {
    pub fn new(rt: &'a Runtime, spec: &'static RpcSpec, sign: Option<&'a SignSpec<Req>>) -> Self {
        RpcCall {
            rt,
            spec,
            req: Req::default(),
            provided: Some(Default::default()),
            sign,
            timeout: None,
            _resp: PhantomData,
        }
    }
}

impl<'a, Req, Resp> RpcCall<'a, Req, Resp> {
    /// Replace the whole request (instead of the field setters).
    pub fn with_request(mut self, req: Req) -> Self {
        self.req = req;
        self.provided = None;
        self
    }

    /// Override the total deadline for this call, including preparation and retries.
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    /// The request as it will be sent (before signing).
    pub fn as_request(&self) -> &Req {
        &self.req
    }

    pub fn spec(&self) -> &'static RpcSpec {
        self.spec
    }
}

impl<'a, Req, Resp> IntoFuture for RpcCall<'a, Req, Resp>
where
    Req: Request + 'static,
    Resp: DeserializeOwned + LiteDecode + Send + 'a,
{
    type Output = Result<Resp>;
    type IntoFuture = BoxFuture<'a, Result<Resp>>;

    fn into_future(mut self) -> Self::IntoFuture {
        if self.req.default_account(self.rt.sub_account_id()) {
            if let Some(fields) = &mut self.provided {
                fields.insert("sub_account_id");
            }
        }
        if let Some(fields) = &self.provided {
            if let Some(name) = Req::required_fields().iter().find(|name| !fields.contains(**name)) {
                let error = Error::Config(format!("{name} is required"));
                return Box::pin(async move { Err(error) });
            }
        }
        Box::pin(self.rt.call(self.spec, self.req, self.sign, self.timeout))
    }
}

/// One stream subscription, ready to start: set selector fields with the generated setters, then
/// `.await` it to subscribe (waits for the server's ack) and get a `Subscription`. `Sel` is a
/// generated marker type naming the stream.
#[must_use = "a StreamCall does nothing until awaited"]
pub struct StreamCall<'a, F, Sel> {
    pub(crate) rt: &'a Runtime,
    pub(crate) spec: &'static StreamSpec,
    pub(crate) primary: Vec<Option<String>>,
    pub(crate) secondary: Vec<Option<String>>,
    required: &'static [(usize, &'static str)],
    queue_size: Option<usize>,
    _p: PhantomData<fn() -> (F, Sel)>,
}

impl<'a, F, Sel> StreamCall<'a, F, Sel> {
    /// `required`: (index into the primary fields, field name) of every required selector field.
    pub fn new(
        rt: &'a Runtime,
        spec: &'static StreamSpec,
        primary: usize,
        secondary: usize,
        required: &'static [(usize, &'static str)],
    ) -> Self {
        StreamCall {
            rt,
            spec,
            primary: vec![None; primary],
            secondary: vec![None; secondary],
            required,
            queue_size: None,
            _p: PhantomData,
        }
    }

    /// The selector this subscription will send, e.g. "BTC_USDT_Perp@500-10".
    pub fn selector(&self) -> String {
        super::signing::wire::selector(&self.primary, &self.secondary)
    }

    pub fn spec(&self) -> &'static StreamSpec {
        self.spec
    }

    /// Override the client's per-subscription queue bound.
    pub fn with_queue_size(mut self, n: usize) -> Self {
        self.queue_size = Some(n);
        self
    }
}

impl<'a, F, Sel> IntoFuture for StreamCall<'a, F, Sel>
where
    F: FeedFrame + Send + 'a,
    Sel: 'a,
{
    type Output = Result<Subscription<F>>;
    type IntoFuture = BoxFuture<'a, Result<Subscription<F>>>;

    fn into_future(self) -> Self::IntoFuture {
        let missing = self
            .required
            .iter()
            .find(|(i, _)| self.primary.get(*i).is_none_or(Option::is_none))
            .map(|(_, n)| *n);
        let selector = self.selector();
        let cap = self.queue_size.unwrap_or(self.rt.0.queue_size);
        let rt = self.rt;
        let spec = self.spec;
        Box::pin(async move {
            if let Some(field) = missing {
                return Err(Error::Config(format!("{}: {field} is required", spec.stream)));
            }
            if spec.auth {
                rt.ensure_session().await?;
            }
            rt.socket(spec.gateway).subscribe(spec, selector, cap).await
        })
    }
}
