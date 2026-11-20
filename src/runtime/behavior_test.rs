//! The runtime's shared defaults and rules equal sdk/contract/behavior.json (`crate::generated::behavior`).
//! A failure here means the runtime holds its own copy of a shared constant: use the generated one instead.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::generated::behavior;
use crate::generated::specs::ORDER_CREATE_ORDER;
use crate::runtime::{
    request::write_outcome, session, Backoff, ClientOrderIdProvider, Error, TimeCounterClientOrderId, READ_ATTEMPTS,
};

#[path = "../../tests/shared/mod.rs"]
pub(crate) mod shared;

#[cfg(feature = "client")]
#[test]
fn client_defaults() {
    let c = crate::runtime::Config::testnet();
    assert_eq!(c.call_timeout, behavior::CALL_TIMEOUT);
    assert_eq!(c.http_attempt_timeout, behavior::HTTP_ATTEMPT_TIMEOUT);
    assert_eq!(c.ws_request_timeout, behavior::WS_REQUEST_TIMEOUT);
    assert_eq!(c.resync_timeout, behavior::RESYNC_TIMEOUT);
    assert_eq!(c.idle_timeout, behavior::WS_IDLE_TIMEOUT);
    assert_eq!(c.queue_size, behavior::QUEUE_SIZE);
}

#[cfg(feature = "client")]
#[tokio::test]
async fn session_and_read_retry_timings() {
    let rt = crate::Grvt::new(crate::runtime::Config::testnet()).unwrap();
    assert_eq!(
        rt.runtime().session_timings(),
        (behavior::HTTP_ATTEMPT_TIMEOUT, behavior::SESSION_RETRY_DELAY)
    );
    let b = crate::runtime::rest::read_backoff();
    assert_eq!(
        (b.base, b.cap),
        (behavior::READ_BACKOFF_BASE, behavior::READ_BACKOFF_CAP)
    );
}

#[test]
fn retry_backoff_and_session() {
    assert_eq!(READ_ATTEMPTS, behavior::READ_ATTEMPTS);
    let b = Backoff::default();
    assert_eq!(
        (b.base, b.cap),
        (behavior::RECONNECT_BACKOFF_BASE, behavior::RECONNECT_BACKOFF_CAP)
    );
    assert_eq!(session::REFRESH_AT, behavior::SESSION_RENEW_AT);
    assert_eq!(session::DEFAULT_TTL, behavior::SESSION_DEFAULT_LIFETIME);
}

#[test]
fn client_order_id_layout() {
    let now = || SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let ids = TimeCounterClientOrderId::default();
    let before = now();
    let (a, b) = (ids.next_id(), ids.next_id());
    let after = now();
    for n in [a, b] {
        let seconds = (n >> behavior::CLIENT_ORDER_ID_COUNTER_BITS) & behavior::CLIENT_ORDER_ID_SECONDS_MASK;
        assert!(n >> 63 == 1 && (before..=after).contains(&seconds), "{n}");
    }
    let mask = behavior::CLIENT_ORDER_ID_COUNTER_MASK;
    assert_eq!(b & mask, (a + 1) & mask);
}

#[test]
fn countdown_bounds() {
    for ok in [behavior::COUNTDOWN_MIN_SECONDS, behavior::COUNTDOWN_MAX_SECONDS] {
        assert!(crate::cancel_countdown_request(1, ok).is_ok(), "{ok}");
    }
    for bad in [behavior::COUNTDOWN_MIN_SECONDS - 1, behavior::COUNTDOWN_MAX_SECONDS + 1] {
        assert!(crate::cancel_countdown_request(1, bad).is_err(), "{bad}");
    }
}

#[test]
fn error_rules() {
    let table = crate::error_codes::TABLE;
    for s in behavior::READ_RETRY_STATUSES {
        assert!(table.retryable(99_999, Some(s)), "{s}");
    }
    assert!(!table.retryable(99_999, Some(500)));
    assert!(matches!(
        table.api_error(behavior::RATE_LIMIT_CODE, "x".into(), Some(429), None),
        Error::RateLimit { .. }
    ));
    let known = 1003; // a contract code with HTTP status 400
    let (low, high) = (behavior::WRITE_REJECTED_STATUS_MIN, behavior::WRITE_REJECTED_STATUS_MAX);
    for (status, unknown) in [(low, false), (high, false), (low - 1, true), (high + 1, true)] {
        let err = table.api_error(known, "x".into(), Some(status), None);
        let out = write_outcome(&ORDER_CREATE_ORDER, err, vec![]);
        assert_eq!(
            matches!(out, Error::OrderStateUnknown { .. }),
            unknown,
            "status {status}"
        );
    }
}

/// Replays sdk/scenarios/decisions.json, the truth table of the generated write and retry decisions.
#[test]
fn shared_decisions() {
    let table = shared::load("scenarios/decisions.json");
    let rows = |section: &str| table[section].as_array().unwrap().clone();
    let status = |v: &serde_json::Value| v.as_u64().map(|s| u16::try_from(s).unwrap());
    for r in rows("write_outcome_unknown") {
        let got = behavior::write_outcome_unknown(status(&r["status"]), status(&r["code_status"]));
        assert_eq!(
            got,
            r["unknown"].as_bool().unwrap(),
            "write_outcome_unknown {}",
            r["case"]
        );
    }
    for r in rows("read_retryable") {
        let code = u32::try_from(r["code"].as_u64().unwrap()).unwrap();
        let got = behavior::read_retryable(code, status(&r["status"]), r["code_retryable"].as_bool().unwrap());
        assert_eq!(got, r["retryable"].as_bool().unwrap(), "read_retryable {}", r["case"]);
    }
    for r in rows("retry_read") {
        let attempt = u32::try_from(r["attempt"].as_u64().unwrap()).unwrap();
        let got = behavior::retry_read(r["retryable"].as_bool().unwrap(), attempt);
        assert_eq!(got, r["retry"].as_bool().unwrap(), "retry_read {r}");
    }
}

// -- options and errors: the "options" and "errors" tables of behavior.json ----------------------------------

/// Sets a shared option on a Config; `v` is in milliseconds for a duration.
#[cfg(feature = "client")]
fn set_option(c: crate::runtime::Config, name: &str, v: u64) -> crate::runtime::Config {
    use std::time::Duration;
    let d = Duration::from_millis(v);
    match name {
        "call_timeout" => c.call_timeout(d),
        "http_attempt_timeout" => c.http_attempt_timeout(d),
        "ws_request_timeout" => c.ws_request_timeout(d),
        "resync_timeout" => c.resync_timeout(d),
        "idle_timeout" => c.idle_timeout(d),
        "queue_size" => c.queue_size(v as usize),
        other => panic!("behavior.json option {other} has no Config setter"),
    }
}

/// Each shared option has a Config setter; a value below its minimum (a zero duration) fails validation.
#[cfg(feature = "client")]
#[test]
fn shared_options() {
    for o in shared::load("contract/behavior.json")["options"].as_array().unwrap() {
        let name = o["name"].as_str().unwrap();
        let (below, lowest) = match (name, o["min"].as_u64()) {
            (_, Some(min)) => (min - 1, min),
            // Rust's idle timeout must also cover one watchdog tick (the option's "rust" extra).
            ("idle_timeout", None) => (0, behavior::WATCHDOG_INTERVAL.as_millis() as u64),
            (_, None) => (0, 1),
        };
        let config = |v| set_option(crate::runtime::Config::testnet(), name, v).validate();
        assert!(
            matches!(config(below), Err(Error::Config(_))),
            "{name} = {below} is accepted"
        );
        assert!(config(lowest).is_ok(), "{name} = {lowest} is rejected");
    }
}

/// A third of the idle timeout is the ping interval, which tokio rejects (panics) when zero.
#[cfg(feature = "client")]
#[test]
fn idle_timeout_below_the_watchdog_interval_is_rejected() {
    use std::time::Duration;
    let config = |d| crate::runtime::Config::testnet().idle_timeout(d).validate();
    for d in [
        Duration::from_nanos(2),
        behavior::WATCHDOG_INTERVAL - Duration::from_nanos(1),
    ] {
        assert!(matches!(config(d), Err(Error::Config(_))), "{d:?} is accepted");
    }
    assert!(config(behavior::WATCHDOG_INTERVAL).is_ok());
}

/// The variant's name. No wildcard arm, so a new variant does not compile until it is listed here.
fn variant(e: &Error) -> &'static str {
    match e {
        Error::Api { .. } => "Api",
        Error::RateLimit { .. } => "RateLimit",
        Error::Transport { .. } => "Transport",
        Error::FrameTooLarge { .. } => "FrameTooLarge",
        Error::OrderStateUnknown { .. } => "OrderStateUnknown",
        Error::StreamOverflow { .. } => "StreamOverflow",
        Error::OrderOutOfBounds(_) => "OrderOutOfBounds",
        Error::Signing(_) => "Signing",
        Error::Decode { .. } => "Decode",
        Error::Config(_) => "Config",
        Error::PaginationCycle { .. } => "PaginationCycle",
        Error::Closed => "Closed",
    }
}

/// The Debug text of one value of each kind, which names its fields.
fn error_samples() -> std::collections::BTreeMap<&'static str, String> {
    let s = String::new;
    let errors = [
        Error::Api {
            code: 1,
            message: s(),
            status: None,
            rpc: None,
            retryable: false,
        },
        Error::RateLimit {
            message: s(),
            status: None,
            rpc: None,
        },
        Error::Transport {
            message: s(),
            rpc: None,
            sent: false,
        },
        Error::FrameTooLarge { size: 2, limit: 1 },
        Error::OrderStateUnknown {
            rpc: s(),
            reason: s(),
            client_order_id: None,
            client_order_ids: vec![],
            source: None,
        },
        Error::StreamOverflow {
            stream: s(),
            selector: s(),
            capacity: 1,
        },
        Error::OrderOutOfBounds(Box::new(crate::runtime::error::OrderOutOfBounds {
            instrument: s(),
            leg: 0,
            field: s(),
            check: s(),
            limit: s(),
            value: s(),
            limit_value: s(),
            nearest: vec![],
            server_code: 0,
            message: s(),
        })),
        Error::Signing(s()),
        Error::Decode { rpc: s(), message: s() },
        Error::Config(s()),
        Error::PaginationCycle { cursor: s() },
        Error::Closed,
    ];
    let mut out: std::collections::BTreeMap<_, _> = errors.iter().map(|e| (variant(e), format!("{e:?}"))).collect();
    // The order book helper's own error type (behavior.json, errors.book.extras).
    out.insert(
        "BookError",
        format!("{:?}", crate::BookError { reason: "x", gap: None }),
    );
    out
}

/// Every grvt::Error variant (and BookError) is a shared kind, with the shared fields. Rust has one enum, so
/// parents don't apply.
#[test]
fn shared_errors() {
    let samples = error_samples();
    let kinds = shared::load("contract/behavior.json")["errors"]
        .as_array()
        .unwrap()
        .clone();
    let names: Vec<&str> = kinds.iter().map(|e| e["names"]["rust"].as_str().unwrap()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(samples.keys().copied().collect::<Vec<_>>(), sorted);
    for (e, name) in kinds.iter().zip(names) {
        let debug = &samples[name];
        for f in e["fields"].as_array().unwrap() {
            let f = f.as_str().unwrap();
            let f = e["renamed"]["rust"][f].as_str().unwrap_or(f);
            assert!(debug.contains(&format!("{f}: ")), "{name} has no field {f}: {debug}");
        }
    }
}

/// After a failed renewal the next login waits the retry delay, not the retry delay and then the minimum wait
/// again; otherwise it waits until the renewal is due, at least the minimum wait.
#[cfg(feature = "client")]
#[test]
fn session_renewal_waits() {
    use super::client::renewal_wait;
    let (now, past) = (1_000.0, 900.0);
    assert_eq!(
        renewal_wait(past, now, behavior::SESSION_RETRY_DELAY),
        behavior::SESSION_RETRY_DELAY
    );
    assert_eq!(
        renewal_wait(past, now, behavior::SESSION_RENEW_MIN_WAIT),
        behavior::SESSION_RENEW_MIN_WAIT
    );
    assert_eq!(
        renewal_wait(now + 60.0, now, behavior::SESSION_RETRY_DELAY),
        std::time::Duration::from_secs(60)
    );
}

/// One kind's name and fields. The pattern has no `..`, so it compiles only when it names every field of the
/// variant: the field list here can't drift from the enum.
#[cfg(feature = "client")]
macro_rules! variant {
    ($kind:literal, $v:ident { $($f:ident),* }) => {{
        let _exact = |e: &crate::runtime::hooks::Event| matches!(e, crate::runtime::hooks::Event::$v { $($f: _),* });
        ($kind, vec![$(stringify!($f)),*])
    }};
}

/// Every hook event is a shared kind with exactly the shared fields (behavior.json, events).
#[cfg(feature = "client")]
#[test]
fn shared_events() {
    use crate::runtime::hooks::EventKind;
    // Exhaustive: a new EventKind fails to compile until it is listed here and below.
    fn name(k: EventKind) -> &'static str {
        match k {
            EventKind::Connect => "connect",
            EventKind::Reconnect => "reconnect",
            EventKind::Disconnect => "disconnect",
            EventKind::Subscribe => "subscribe",
            EventKind::Gap => "gap",
            EventKind::Retry => "retry",
            EventKind::Frame => "frame",
            EventKind::SessionRotated => "session_rotated",
            EventKind::Error => "error",
        }
    }
    let ours = [
        (EventKind::Connect, variant!("connect", Connect { socket })),
        (EventKind::Reconnect, variant!("reconnect", Reconnect { socket })),
        (
            EventKind::Disconnect,
            variant!("disconnect", Disconnect { socket, code, reason }),
        ),
        (
            EventKind::Subscribe,
            variant!(
                "subscribe",
                Subscribe {
                    socket,
                    stream,
                    selector,
                    latest_sequence_number,
                    num_snapshots
                }
            ),
        ),
        (
            EventKind::Gap,
            variant!(
                "gap",
                Gap {
                    gap,
                    socket,
                    stream,
                    selector,
                    expected_prev,
                    prev,
                    seq
                }
            ),
        ),
        (
            EventKind::Retry,
            variant!(
                "retry",
                Retry {
                    socket,
                    rpc,
                    attempt,
                    delay,
                    error
                }
            ),
        ),
        (EventKind::Frame, variant!("frame", Frame { socket, direction, raw })),
        (
            EventKind::SessionRotated,
            variant!(
                "session_rotated",
                SessionRotated {
                    websocket_reconnect_required,
                    generation,
                    expires_at
                }
            ),
        ),
        (EventKind::Error, variant!("error", Error { socket, fatal, error })),
    ];
    for (k, (kind, _)) in &ours {
        assert_eq!(name(*k), *kind);
    }
    let table = shared::load("contract/behavior.json")["events"]
        .as_array()
        .unwrap()
        .clone();
    let mut want: Vec<(String, Vec<String>)> = table
        .iter()
        .map(|e| {
            let mut fields: Vec<String> = e["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| {
                    let f = f.as_str().unwrap();
                    e["renamed"]["rust"][f].as_str().unwrap_or(f).to_owned()
                })
                .collect();
            fields.sort();
            (e["kind"].as_str().unwrap().to_owned(), fields)
        })
        .collect();
    want.sort();
    let mut got: Vec<(String, Vec<String>)> = ours
        .iter()
        .map(|(_, (kind, fields))| {
            let mut fields: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
            fields.sort();
            (kind.to_string(), fields)
        })
        .collect();
    got.sort();
    assert_eq!(got, want);
}
