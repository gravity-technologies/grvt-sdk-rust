//! Public testnet data by default. --trade places one real order which may fill.
use grvt::{
    enums::{OrderStatus, TimeInForce},
    models as m, resolve_order, Config, Encoding, Environment, Error, Grvt, OrderResolution, PrivateKeySigner,
};
use rust_decimal::Decimal;
use std::{
    env,
    io::{self, Write},
    str::FromStr,
    time::Duration,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut trade = false;
    let mut instrument = "BTC_USDT_Perp".to_owned();
    let mut cap = Decimal::from(200);
    let mut environment = Environment::testnet();
    let mut fixture = false;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--trade" {
            trade = true;
            index += 1;
            continue;
        }
        let value = args.get(index + 1).ok_or("missing option value")?;
        match args[index].as_str() {
            "--instrument" => instrument = value.clone(),
            "--max-notional" => cap = Decimal::from_str(value)?,
            "--fixture-url" => {
                let port = value
                    .strip_prefix("http://127.0.0.1:")
                    .ok_or("fixture URL must be an HTTP loopback origin")?;
                let _: u16 = port.parse()?;
                environment = Environment {
                    name: "fixture".into(),
                    md_url: value.clone(),
                    td_url: value.clone(),
                    edge_url: value.clone(),
                    md_ws_url: value.replace("http:", "ws:"),
                    td_ws_url: value.replace("http:", "ws:"),
                    chain_id: 326,
                    ws_max_frame_bytes: None,
                };
                fixture = true;
            }
            _ => return Err("unknown option".into()),
        }
        index += 2;
    }
    if cap <= Decimal::ZERO {
        return Err("max-notional must be positive".into());
    }
    let mut config = Config::new(environment).http_attempt_timeout(Duration::from_secs(5));
    if fixture {
        // the local fixture gateway speaks full names only
        config = config.encoding(Encoding::Full);
    }
    let mut account = 0;
    if trade {
        let key = env::var("GRVT_API_KEY")?;
        if key.trim().is_empty() {
            return Err("GRVT_API_KEY is required".into());
        }
        account = env::var("GRVT_SUB_ACCOUNT_ID")?.parse::<u64>()?;
        if account == 0 {
            return Err("GRVT_SUB_ACCOUNT_ID must be positive".into());
        }
        config = config
            .api_key(key)
            .signer(PrivateKeySigner::new(&env::var("GRVT_PRIVATE_KEY")?)?);
    }
    let client = Grvt::new(config)?;
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        journey(&client, trade, &instrument, account, cap),
    )
    .await;
    client.close().await;
    let code = match outcome {
        Ok(result) => result?,
        Err(_) => {
            eprintln!("deadline reached; retain any printed client_order_id and reconcile; do not resend");
            2
        }
    };
    std::process::exit(code);
}

async fn journey(client: &Grvt, trade: bool, instrument: &str, account: u64, cap: Decimal) -> Result<i32> {
    let ticker = client.ticker().instrument(instrument).await?.result;
    let metadata = client.get_instrument().instrument(instrument).await?.result;
    println!("mark price {:?}", ticker.mark_price);
    if !trade {
        return Ok(0);
    }
    let tick = metadata.tick_size;
    let step = metadata.min_size;
    let bid = ticker.best_bid_price.ok_or("instrument has no usable bid")?;
    if tick <= Decimal::ZERO || step <= Decimal::ZERO || bid <= tick {
        return Err("instrument has no usable tick, bid or size".into());
    }
    let price = ((bid / tick).floor() - Decimal::ONE) * tick;
    if price <= Decimal::ZERO {
        return Err("no positive passive price; no order sent".into());
    }
    let size = (metadata.min_notional / (price * step)).ceil().max(Decimal::ONE) * step;
    if price * size > cap {
        return Err("minimum order exceeds max-notional; no order sent".into());
    }
    let id = rand::random::<u64>() | (1 << 63);
    println!("client_order_id {id} price {price} size {size}");
    io::stdout().flush()?;
    let leg = m::OrderLeg::new(instrument, size, true).with_limit_price(price);
    let order = m::OrderInput::new(TimeInForce::GoodTillTime, Some(vec![leg]))
        .with_sub_account_id(account)
        .with_post_only(true)
        .with_metadata(m::OrderMetadataInput::default().with_client_order_id(id));
    match client.create_order().order(order).await {
        Ok(_) => (),
        Err(Error::OrderStateUnknown { .. }) => println!("create outcome unknown; looking up the same ID"),
        Err(error) => return Err(error.into()),
    }
    let mut cancelled = false;
    for _ in 0..8 {
        let observed = match resolve_order(client, account, id).await {
            Ok(value) => value,
            Err(_) => {
                println!("lookup unavailable; continuing bounded observation");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        if let OrderResolution::Found(order) = observed {
            if let Some(state) = order.state {
                println!("order status {}", state.status.as_str());
                match state.status {
                    OrderStatus::Cancelled | OrderStatus::Filled => return Ok(0),
                    OrderStatus::Rejected => return Ok(3),
                    OrderStatus::Open if !cancelled => {
                        cancelled = true;
                        if client
                            .cancel_order()
                            .sub_account_id(account)
                            .client_order_id(id)
                            .await
                            .is_err()
                        {
                            println!("cancel response unavailable; observing without retry");
                        }
                    }
                    _ => (),
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    println!("outcome still unknown; retain client_order_id and reconcile manually; do not resend");
    Ok(2)
}
