#![cfg(feature = "client")]
mod encodings;
mod fixtures;
mod lite_names;
mod shared;
use encodings::as_full;
use grvt::runtime::testing::FakeHttp;
use grvt::{resolve_order, Config, Grvt, OrderResolution};

#[tokio::test]
async fn lookup_only_and_not_found_stays_unknown() {
    encodings::each(|encoding| async move {
        for missing in [true, false] {
            let http = FakeHttp::default();
            encodings::serve(&http, move |url, body| {
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
            let client = Grvt::new(Config::testnet().encoding(encoding).api_key("key").http(http.clone())).unwrap();
            let result = resolve_order(&client, 1, 2).await.unwrap();
            assert_eq!(matches!(result, OrderResolution::StillUnknown), missing);
            let urls: Vec<String> = http.calls().iter().map(|c| as_full(&c.url)).collect();
            assert_eq!(urls.iter().filter(|u| u.ends_with("/full/v1/order")).count(), 1);
            assert!(urls
                .iter()
                .all(|u| u.ends_with("/full/v1/order") || u.ends_with("/auth/api_key/login")));
        }
    })
    .await;
}
