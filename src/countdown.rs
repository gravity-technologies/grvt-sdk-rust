//! Explicit arm/refresh of the server's account-wide cancellation countdown. The request and the receipt
//! are plain functions; `refresh_cancel_countdown` (client) sends it once.
use crate::generated::behavior;
use crate::{models, Error, Result};

/// Acknowledgment only: another caller can replace the same account's timer.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CountdownReceipt {
    pub sub_account_id: u64,
    pub countdown_seconds: u16,
    pub acknowledged: bool,
}
impl CountdownReceipt {
    pub fn scope(&self) -> &'static str {
        "sub_account"
    }
}

/// The `cancel_on_disconnect` request that arms or refreshes the countdown. Send it once; never retry.
pub fn cancel_countdown_request(
    sub_account_id: u64,
    countdown_seconds: u16,
) -> Result<models::ApiCancelOnDisconnectRequest> {
    let (min, max) = (behavior::COUNTDOWN_MIN_SECONDS, behavior::COUNTDOWN_MAX_SECONDS);
    if sub_account_id == 0 || !(min..=max).contains(&countdown_seconds) {
        return Err(Error::Config(format!(
            "positive sub-account and countdown seconds from {min} to {max} required"
        )));
    }
    Ok(models::ApiCancelOnDisconnectRequest::default()
        .with_sub_account_id(sub_account_id)
        .with_countdown_time(u64::from(countdown_seconds) * 1000))
}

/// The receipt for a `cancel_on_disconnect` reply.
pub fn countdown_receipt(
    sub_account_id: u64,
    countdown_seconds: u16,
    response: &models::AckResponse,
) -> CountdownReceipt {
    CountdownReceipt {
        sub_account_id,
        countdown_seconds,
        acknowledged: response.result.ack,
    }
}

/// Send once, never retry or disarm. Reconnect alone never calls this helper.
/// The caller owns refresh scheduling and decides when recovery permits rearming.
/// An ambiguous write propagates as OrderStateUnknown.
#[cfg(feature = "client")]
pub async fn refresh_cancel_countdown(
    client: &crate::Grvt,
    sub_account_id: u64,
    countdown_seconds: u16,
) -> Result<CountdownReceipt> {
    let request = cancel_countdown_request(sub_account_id, countdown_seconds)?;
    let response = client.cancel_on_disconnect().with_request(request).await?;
    Ok(countdown_receipt(sub_account_id, countdown_seconds, &response))
}
