#![allow(dead_code)]
use serde_json::{json, Value};
use std::sync::OnceLock;
pub fn contract() -> &'static Value {
    static CONTRACT: OnceLock<Value> = OnceLock::new();
    CONTRACT.get_or_init(|| crate::shared::load("contract/contract.json"))
}
pub fn model(name: &str) -> Value {
    let mut result = serde_json::Map::new();
    let t = contract()["types"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap();
    for f in t["fields"].as_array().unwrap() {
        if f["optional"] == true {
            continue;
        }
        let v = if f["array_depth"].as_u64().unwrap_or(0) > 0 {
            json!([])
        } else if f["kind"] == "struct" {
            model(f["type"].as_str().unwrap())
        } else if f["kind"] == "enum" {
            contract()["enums"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["name"] == f["type"])
                .unwrap()["values"][0]["name"]
                .clone()
        } else {
            match f["type"].as_str().unwrap() {
                "bool" => json!(false),
                "uint8" | "uint16" | "uint32" | "int8" | "int16" | "int32" => json!(1),
                "any" => json!({}),
                _ => json!("1"),
            }
        };
        result.insert(f["name"].as_str().unwrap().into(), v);
    }
    Value::Object(result)
}
pub fn response(route: &str) -> Value {
    contract()["rpcs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| route == r["ws_method"].as_str().unwrap() || route.ends_with(r["http_path"].as_str().unwrap()))
        .map(|r| model(r["response"].as_str().unwrap()))
        .unwrap_or(json!({}))
}
pub fn feed(channel: &str) -> Value {
    let s = contract()["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["channel"] == channel)
        .unwrap();
    let mut frame = model(s["feed_type"].as_str().unwrap());
    let payload = &mut frame["feed"];
    if payload.is_object() {
        payload["instrument"] = json!("X");
    }
    payload.take()
}
