#![cfg(feature = "client")]
mod fixtures;
mod shared;
use grvt::runtime::testing::FakeHttp;
use grvt::{resolve_order, Config, Encoding, Grvt, OrderResolution};

#[tokio::test]
async fn lookup_only_and_not_found_stays_unknown() {
    for missing in [true, false] {
        let http = FakeHttp::default();
        http.set_handler(move |url, body| {
            assert!(url.ends_with("/full/v1/order"));
            let request: serde_json::Value = serde_json::from_slice(body).unwrap();
            assert_eq!(request, serde_json::json!({"sub_account_id":"1","client_order_id":"2"}));
            if missing {
                let mut response = FakeHttp::ok(r#"{"code":1004,"message":"not visible"}"#);
                response.status = 404;
                Ok(response)
            } else {
                let mut order = fixtures::model("Order");
                order["sub_account_id"] = serde_json::json!("1");
                order["metadata"]["client_order_id"] = serde_json::json!("2");
                Ok(FakeHttp::ok(&serde_json::json!({"result":order}).to_string()))
            }
        });
        let client = Grvt::new(
            Config::testnet()
                .encoding(Encoding::Full)
                .api_key("key")
                .http(http.clone()),
        )
        .unwrap();
        let result = resolve_order(&client, 1, 2).await.unwrap();
        assert_eq!(matches!(result, OrderResolution::StillUnknown), missing);
        assert_eq!(
            http.calls()
                .iter()
                .filter(|c| c.url.ends_with("/full/v1/order"))
                .count(),
            1
        );
        assert!(http
            .calls()
            .iter()
            .all(|c| c.url.ends_with("/full/v1/order") || c.url.ends_with("/auth/api_key/login")));
    }
}
