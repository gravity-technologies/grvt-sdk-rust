#![cfg(feature = "client")]
use grvt::runtime::testing::FakeHttp;
use grvt::{refresh_cancel_countdown, Config, Encoding, Error, Grvt};
#[tokio::test]
async fn countdown_is_one_explicit_write_even_when_unknown() {
    for unknown in [false, true] {
        let http = FakeHttp::default();
        http.set_handler(move |url, body| {
            assert!(url.ends_with("/full/v1/cancel_on_disconnect"));
            let request: serde_json::Value = serde_json::from_slice(body).unwrap();
            assert_eq!(
                request,
                serde_json::json!({"sub_account_id":"1","countdown_time":"120000"})
            );
            if unknown {
                Err(Error::Transport {
                    message: "timeout".into(),
                    rpc: None,
                    sent: true,
                })
            } else {
                Ok(FakeHttp::ok(r#"{"result":{"ack":true}}"#))
            }
        });
        let client = Grvt::new(
            Config::testnet()
                .encoding(Encoding::Full)
                .api_key("key")
                .http(http.clone()),
        )
        .unwrap();
        assert!(refresh_cancel_countdown(&client, 1, 0).await.is_err());
        assert!(refresh_cancel_countdown(&client, 1, 301).await.is_err());
        let result = refresh_cancel_countdown(&client, 1, 120).await;
        if unknown {
            assert!(matches!(result, Err(Error::OrderStateUnknown { .. })));
        } else {
            let receipt = result.unwrap();
            assert!(receipt.acknowledged);
            assert_eq!(receipt.scope(), "sub_account");
        }
        assert_eq!(
            http.calls()
                .iter()
                .filter(|c| c.url.ends_with("/full/v1/cancel_on_disconnect"))
                .count(),
            1
        );
        client.close().await;
    }
}
