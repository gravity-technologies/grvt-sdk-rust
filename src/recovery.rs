//! Read an uncertain order's state without placing or resending anything. The request and the verdict
//! are plain functions; `resolve_order` (client) sends the one lookup.

use crate::{error_codes, models, Error, Result};

/// An observed ID match, or an outcome that is still unknown. Neither permits a resend.
#[derive(Debug, Clone)]
pub enum OrderResolution {
    /// If client IDs were reused, this order may belong to a different submission.
    Found(Box<models::Order>),
    /// Not found does not prove absence: history can lag behind an accepted order.
    StillUnknown,
}

/// The one `get_order` lookup for an order whose write ended in `OrderStateUnknown`. For a bulk call,
/// look up each id of the error separately.
pub fn resolve_order_request(sub_account_id: u64, client_order_id: u64) -> Result<models::ApiGetOrderRequest> {
    if sub_account_id == 0 || client_order_id == 0 {
        return Err(Error::Config(
            "sub_account_id and client_order_id must be positive".into(),
        ));
    }
    Ok(models::ApiGetOrderRequest::default()
        .with_sub_account_id(sub_account_id)
        .with_client_order_id(client_order_id))
}

/// Read the lookup's reply. NOT_FOUND stays unknown; other errors are returned.
pub fn resolve_order_result(
    sub_account_id: u64,
    client_order_id: u64,
    reply: Result<models::ApiGetOrderResponse>,
) -> Result<OrderResolution> {
    match reply {
        Ok(response) => {
            let order = response.result;
            if order.sub_account_id == sub_account_id && order.metadata.client_order_id == client_order_id {
                return Ok(OrderResolution::Found(Box::new(order)));
            }
            Ok(OrderResolution::StillUnknown)
        }
        Err(Error::Api {
            code: error_codes::NOT_FOUND,
            ..
        }) => Ok(OrderResolution::StillUnknown),
        Err(error) => Err(error),
    }
}

/// Look up the client ID once; never place, cancel or resend an order.
#[cfg(feature = "client")]
pub async fn resolve_order(client: &crate::Grvt, sub_account_id: u64, client_order_id: u64) -> Result<OrderResolution> {
    let request = resolve_order_request(sub_account_id, client_order_id)?;
    let reply = client.get_order().with_request(request).await;
    resolve_order_result(sub_account_id, client_order_id, reply)
}
