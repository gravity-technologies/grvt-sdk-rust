//! The contract's lite names as a JSON renamer, independent of the SDK's generated codecs: the tests compare
//! what the SDK sends and reads in lite with this. Renaming is per type and recursive, as the server does.
#![allow(dead_code)]

use std::collections::HashMap;

use serde_json::{json, Map, Value};

pub struct LiteNames {
    types: HashMap<String, Value>,
    enums: HashMap<String, Value>,
}

impl LiteNames {
    /// From the parsed sdk/contract/contract.json.
    pub fn new(contract: &Value) -> LiteNames {
        let by_name = |key: &str| {
            contract[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| (t["name"].as_str().unwrap().to_owned(), t.clone()))
                .collect::<HashMap<_, _>>()
        };
        LiteNames {
            types: by_name("types"),
            enums: by_name("enums"),
        }
    }

    /// The contract type an SDK model mirrors: SDK copies (`OrderInput`, `PartialTicker`) share their wire
    /// type's fields.
    fn wire_type(&self, model: &str) -> &Value {
        let base = model.strip_suffix("Input").unwrap_or(model);
        let base = base.strip_prefix("Partial").unwrap_or(base);
        self.types
            .get(model)
            .or_else(|| self.types.get(base))
            .unwrap_or_else(|| panic!("{model} is not in the contract"))
    }

    fn fields<'a>(&'a self, model: &str) -> impl Iterator<Item = &'a Value> {
        self.wire_type(model)["fields"].as_array().into_iter().flatten()
    }

    /// `model.field`'s lite name.
    pub fn lite(&self, model: &str, field: &str) -> String {
        self.fields(model)
            .find(|f| f["name"] == field)
            .unwrap_or_else(|| panic!("{model}.{field} is not in the contract"))["lite"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// A full JSON value of `model` with every field set, lists of two.
    pub fn sample(&self, model: &str) -> Value {
        self.sample_at(model, 0)
    }

    fn sample_at(&self, model: &str, depth: usize) -> Value {
        let mut out = Map::new();
        for f in self.fields(model) {
            let optional = f["optional"].as_bool().unwrap_or(false);
            if depth > 3 && optional && f["kind"] == "struct" {
                continue;
            }
            let mut v = self.scalar(f, depth);
            for _ in 0..f["array_depth"].as_u64().unwrap_or(0) {
                v = json!([v.clone(), v]);
            }
            out.insert(f["name"].as_str().unwrap().to_owned(), v);
        }
        Value::Object(out)
    }

    fn scalar(&self, f: &Value, depth: usize) -> Value {
        let ty = f["type"].as_str().unwrap();
        match f["kind"].as_str().unwrap() {
            "struct" => self.sample_at(ty, depth + 1),
            "enum" => self.enums[ty]["values"][0]["name"].clone(),
            "decimal" => json!("1.5"),
            "timestamp" => json!("1700000000000000001"),
            "any" => json!({"k": [1, "v"]}),
            _ => match ty {
                "bool" => json!(true),
                "uint64" | "int64" => json!("7"),
                "uint8" | "uint16" | "uint32" | "int8" | "int16" | "int32" => json!(7),
                _ => json!("x7"),
            },
        }
    }

    /// `value` (full names, of `model`) with every key renamed to its lite name. A key the contract does
    /// not list for the type panics.
    pub fn to_lite(&self, model: &str, value: &Value) -> Value {
        self.rename(model, value, true).unwrap_or_else(|e| panic!("{e}"))
    }

    /// `to_lite` as a server writes: keys the contract does not list for the type stay as they are.
    pub fn to_lite_lenient(&self, model: &str, value: &Value) -> Value {
        self.rename_with(model, value, true, true).unwrap()
    }

    /// `value` (lite names, of `model`) back to full names. A key that is not a lite name of its type is
    /// an error: a request in lite must carry only lite keys.
    pub fn to_full(&self, model: &str, value: &Value) -> Result<Value, String> {
        self.rename(model, value, false)
    }

    fn rename(&self, model: &str, value: &Value, to_lite: bool) -> Result<Value, String> {
        self.rename_with(model, value, to_lite, false)
    }

    fn rename_with(&self, model: &str, value: &Value, to_lite: bool, lenient: bool) -> Result<Value, String> {
        let (from, to) = if to_lite { ("name", "lite") } else { ("lite", "name") };
        match value {
            Value::Array(items) => items
                .iter()
                .map(|i| self.rename_with(model, i, to_lite, lenient))
                .collect(),
            Value::Object(map) => {
                let mut out = Map::new();
                for (k, v) in map {
                    let Some(f) = self.fields(model).find(|f| f[from] == *k) else {
                        if lenient {
                            out.insert(k.clone(), v.clone());
                            continue;
                        }
                        return Err(format!("{model} has no {from} key {k:?}"));
                    };
                    let v = if f["kind"] == "struct" {
                        self.rename_with(f["type"].as_str().unwrap(), v, to_lite, lenient)?
                    } else {
                        v.clone()
                    };
                    out.insert(f[to].as_str().unwrap().to_owned(), v);
                }
                Ok(Value::Object(out))
            }
            v => Ok(v.clone()),
        }
    }
}
