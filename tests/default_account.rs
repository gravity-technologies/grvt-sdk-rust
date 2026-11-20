#![cfg(feature = "client")]
mod encodings;
mod fixtures;
mod lite_names;
mod shared;
use grvt::runtime::testing::FakeHttp;
use grvt::{models, Config, Grvt};
use std::sync::{Arc, Mutex};
#[tokio::test]
async fn default_account_and_explicit_order_overrides() {
    encodings::each(|encoding| async move {
        let http = FakeHttp::default();
        encodings::serve(&http, |url, _| Ok(FakeHttp::ok(&fixtures::response(url).to_string())));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let copy = captured.clone();
        let client = Grvt::new(
            Config::testnet()
                .encoding(encoding)
                .api_key("key")
                .sub_account_id(123)
                .http(http.clone())
                .before_send(move |call| copy.lock().unwrap().push(call.params.clone())),
        )
        .unwrap();
        client.get_order().client_order_id(1).await.unwrap();
        assert_eq!(captured.lock().unwrap().last().unwrap()["sub_account_id"], "123");
        client.get_order().sub_account_id(456).client_order_id(1).await.unwrap();
        assert_eq!(captured.lock().unwrap().last().unwrap()["sub_account_id"], "456");
        let order = models::OrderInput::default();
        assert!(client.create_order().order(order.clone()).await.is_err());
        assert_eq!(
            captured.lock().unwrap().last().unwrap()["order"]["sub_account_id"],
            "123"
        );
        assert!(order.sub_account_id.is_none());
        assert!(client
            .bulk_orders()
            .sub_account_id(456)
            .orders(vec![order.clone()])
            .await
            .is_err());
        assert_eq!(
            captured.lock().unwrap().last().unwrap()["orders"][0]["sub_account_id"],
            "456"
        );
        let mut explicit = order.clone();
        explicit.sub_account_id = Some(789);
        assert!(client.create_order().order(explicit).await.is_err());
        assert_eq!(
            captured.lock().unwrap().last().unwrap()["order"]["sub_account_id"],
            "789"
        );
        assert_eq!(client.order_stream().selector(), "123");
        assert_eq!(client.order_stream().sub_account_id(456).selector(), "456");
        client.close().await;
        assert!(Grvt::new(Config::testnet().sub_account_id(0)).is_err());
    })
    .await;
}

#[tokio::test]
async fn transfer_selector_keeps_optional_account_omitted() {
    for default in [None, Some(123)] {
        let mut config = Config::testnet();
        if let Some(id) = default {
            config = config.sub_account_id(id);
        }
        let client = Grvt::new(config).unwrap();
        assert_eq!(client.transfer_stream().main_account_id("main").selector(), "main");
        assert_eq!(
            client
                .transfer_stream()
                .main_account_id("main")
                .sub_account_id(456)
                .selector(),
            "main-456"
        );
        client.close().await;
    }
}
