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
