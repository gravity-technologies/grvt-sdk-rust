//! Where the SDK connects and which chain id it signs for.
//!
//! Use a named environment (`testnet`, `prod`) or build your own `Environment` (a local gateway, a
//! test server).

use super::error::{Error, Result};
use super::lite::Encoding;
use super::spec::Gateway;
use crate::generated::specs;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub name: String,
    /// Market-data gateway, REST.
    pub md_url: String,
    /// Trading gateway, REST.
    pub td_url: String,
    /// Edge: login.
    pub edge_url: String,
    pub md_ws_url: String,
    pub td_ws_url: String,
    /// EIP-712 domain chainId.
    pub chain_id: u64,
    /// Largest WS frame the gateway accepts (websocket ReadLimitSize). The deployed value is not
    /// published yet, so the named environments leave it `None` (do not check).
    pub ws_max_frame_bytes: Option<usize>,
}

impl Environment {
    fn hosted(name: &str, domain: &str, chain_id: u64) -> Environment {
        Environment {
            name: name.to_owned(),
            md_url: format!("https://market-data.{domain}"),
            td_url: format!("https://trades.{domain}"),
            edge_url: format!("https://edge.{domain}"),
            md_ws_url: format!("wss://market-data.{domain}/ws/full"),
            td_ws_url: format!("wss://trades.{domain}/ws/full"),
            chain_id,
            ws_max_frame_bytes: None,
        }
    }

    pub fn testnet() -> Environment {
        Self::hosted("testnet", "testnet.grvt.io", 326)
    }

    pub fn prod() -> Environment {
        Self::hosted("prod", "grvt.io", 325)
    }

    /// `"testnet"` or `"prod"`.
    pub fn named(name: &str) -> Result<Environment> {
        match name {
            "testnet" => Ok(Self::testnet()),
            "prod" => Ok(Self::prod()),
            _ => Err(Error::Config(format!(
                "unknown env {name:?}; use \"testnet\", \"prod\" or build an Environment"
            ))),
        }
    }

    /// Reports a missing URL or chain id. Every field of a custom environment is used as given: nothing
    /// falls back to a named environment, so an empty URL or a zero `chain_id` is an error, not a default.
    /// `Grvt::new` calls it.
    pub fn validate(&self) -> Result<()> {
        let urls = [
            ("md_url", &self.md_url),
            ("td_url", &self.td_url),
            ("edge_url", &self.edge_url),
            ("md_ws_url", &self.md_ws_url),
            ("td_ws_url", &self.td_ws_url),
        ];
        let mut missing: Vec<&str> = urls
            .iter()
            .filter(|(_, v)| v.trim().is_empty())
            .map(|(k, _)| *k)
            .collect();
        if self.chain_id == 0 {
            missing.push("chain_id");
        }
        if missing.is_empty() {
            return Ok(());
        }
        Err(Error::Config(format!(
            "environment {:?} is missing {}: a custom Environment must set every URL and the chain id",
            self.name,
            missing.join(", ")
        )))
    }

    /// Every environment the SDK knows by name.
    pub fn all() -> Vec<Environment> {
        vec![Self::testnet(), Self::prod()]
    }

    pub fn rest_url(&self, gateway: Gateway, path: &str) -> String {
        let base = match gateway {
            Gateway::MarketData => &self.md_url,
            Gateway::Trading => &self.td_url,
            Gateway::Edge => &self.edge_url,
        };
        format!("{base}{path}")
    }

    pub fn ws_url(&self, gateway: Gateway) -> &str {
        match gateway {
            Gateway::MarketData => &self.md_ws_url,
            _ => &self.td_ws_url,
        }
    }

    /// The gateway's WebSocket URL in `encoding`. The lite URL is the full one with the contract's full WS
    /// path (`/ws/full`) swapped for the lite one (`/ws/lite`); a URL that does not end in the full path has
    /// no known lite form and is an error.
    pub fn ws_url_for(&self, gateway: Gateway, encoding: Encoding) -> Result<String> {
        let url = self.ws_url(gateway);
        match encoding {
            Encoding::Full => Ok(url.to_owned()),
            Encoding::Lite => url
                .strip_suffix(specs::FULL_WS_PATH)
                .map(|base| format!("{base}{}", specs::LITE_WS_PATH))
                .ok_or_else(|| {
                    Error::Config(format!(
                        "environment {:?}: {url} does not end in {}, so it has no lite URL",
                        self.name,
                        specs::FULL_WS_PATH
                    ))
                }),
        }
    }
}
