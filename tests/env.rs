//! A custom Environment is used exactly as given: every URL and the chain id are set independently, and a
//! missing one is an `Error::Config` from `Grvt::new`, never a silent fallback to a named environment.
#![cfg(feature = "client")]

use grvt::{Config, Environment, Error, Grvt};

fn custom() -> Environment {
    Environment {
        name: "custom".into(),
        md_url: "https://md.gateway.example.com".into(),
        td_url: "https://td.gateway.example.com".into(),
        edge_url: "https://edge.gateway.example.com".into(),
        md_ws_url: "wss://md.gateway.example.com/ws/full".into(),
        td_ws_url: "wss://td.gateway.example.com/ws/full".into(),
        chain_id: 4242,
        ws_max_frame_bytes: None,
    }
}

#[test]
fn custom_environment_is_used_as_given() {
    let client = Grvt::new(Config::new(custom())).unwrap();
    assert_eq!(client.runtime().env(), &custom());
    assert_eq!(client.runtime().domain().chain_id, 4242);
}

#[test]
fn custom_environment_rejects_a_missing_url_or_chain_id() {
    type Unset = fn(&mut Environment);
    let unset: [(&str, Unset); 6] = [
        ("md_url", |e| e.md_url.clear()),
        ("td_url", |e| e.td_url.clear()),
        ("edge_url", |e| e.edge_url.clear()),
        ("md_ws_url", |e| e.md_ws_url.clear()),
        ("td_ws_url", |e| e.td_ws_url.clear()),
        ("chain_id", |e| e.chain_id = 0),
    ];
    for (field, clear) in unset {
        let mut env = custom();
        clear(&mut env);
        match Grvt::new(Config::new(env)) {
            Err(Error::Config(msg)) => assert!(msg.contains(field), "{field}: {msg}"),
            Err(e) => panic!("{field}: want Error::Config, got {e:?}"),
            Ok(_) => panic!("{field}: accepted"),
        }
    }
}

#[test]
fn only_testnet_and_production_are_named() {
    let names: Vec<String> = Environment::all().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["testnet", "production"]);
    assert_eq!(Environment::named("production").unwrap(), Config::production().env);
    assert!(Environment::named("prod").is_err());
    assert!(Environment::named("staging").is_err());
}
