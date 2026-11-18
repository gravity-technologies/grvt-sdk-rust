#[test]
fn bulk_id_aliases_preserve_wire_spelling() {
    let request = grvt::models::ApiBulkOrdersRequestInput::default()
        .with_order_ids(vec!["abc".into()])
        .with_client_order_ids(vec![42]);
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(wire["order_i_ds"], serde_json::json!(["abc"]));
    assert_eq!(wire["client_order_i_ds"], serde_json::json!(["42"]));
    assert!(wire.get("order_ids").is_none());
    assert_eq!(
        serde_json::from_value::<grvt::models::ApiBulkOrdersRequestInput>(wire).unwrap(),
        request
    );
}

#[test]
fn complete_models_require_fields_and_preserve_false() {
    use grvt::models::{ApiGetAllInstrumentsResponse, Order, OrderInput, PartialWSTickerFeedDataV1};
    let wire = serde_json::json!({"sub_account_id":"1","time_in_force":"GOOD_TILL_TIME","legs":[{"instrument":"BTC_USDT_Perp","size":"1","is_buying_asset":false}]});
    let input: OrderInput = serde_json::from_value(wire.clone()).unwrap();
    assert!(!input.legs.as_ref().unwrap()[0].is_buying_asset);
    assert_eq!(
        serde_json::to_value(input).unwrap()["legs"][0]["is_buying_asset"],
        false
    );
    assert!(
        serde_json::from_value::<Order>(wire).is_err(),
        "a complete order needs its signature and metadata"
    );
    assert!(serde_json::from_value::<ApiGetAllInstrumentsResponse>(serde_json::json!({})).is_err());
    assert!(serde_json::from_value::<ApiGetAllInstrumentsResponse>(serde_json::json!({"result":null})).is_ok());
    let frame = serde_json::json!({"stream":"v1.ticker.d","selector":"X","sequence_number":"1","prev_sequence_number":"0","feed":{"mark_price":"1"}});
    assert!(serde_json::from_value::<PartialWSTickerFeedDataV1>(frame.clone()).is_ok());
    let mut missing = frame;
    missing.as_object_mut().unwrap().remove("sequence_number");
    assert!(serde_json::from_value::<PartialWSTickerFeedDataV1>(missing).is_err());
}

#[test]
fn required_input_constructor_and_optional_setters() {
    use grvt::{
        enums::TimeInForce,
        models::{OrderInput, OrderLeg},
    };
    use rust_decimal::Decimal;
    let leg = OrderLeg::new("BTC_USDT_Perp", Decimal::ONE, true).with_limit_price(Decimal::from(60000));
    assert!(leg.is_buying_asset);
    assert_eq!(leg.limit_price, Some(Decimal::from(60000)));
    let order = OrderInput::new(TimeInForce::GoodTillTime, Some(vec![leg])).with_sub_account_id(123);
    assert_eq!(order.sub_account_id, Some(123));
    assert_eq!(order.legs.unwrap()[0].instrument, "BTC_USDT_Perp");
    let page = grvt::models::ApiTradeHistoryResponse::default().with_next("cursor");
    assert_eq!(page.next.as_deref(), Some("cursor"));
}

#[test]
fn history_filter_keeps_nanoseconds_on_wire() {
    let value = grvt::Timestamp::from_nanoseconds(1793172225200080949);
    let request = grvt::models::ApiFillHistoryRequest::default()
        .with_sub_account_id(123)
        .with_start_time(value);
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(wire["start_time"], "1793172225200080949");
    let decoded: grvt::models::ApiFillHistoryRequest = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded.start_time, Some(value));
}
