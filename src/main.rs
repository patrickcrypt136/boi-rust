use anyhow::Result;
use chrono::Local;
use colored::*;
use dotenv::dotenv;
use ethers::{
    middleware::SignerMiddleware,
    prelude::*,
    providers::{Http, Provider, Ws},
    signers::{LocalWallet, Signer},
    types::{Address, U256},
};
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use std::{
    collections::HashSet,
    env,
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;
use tokio::time::sleep;

// ── CONFIG ────────────────────────────────────────────
const BOI_API: &str = "https://www.boithebear.com/api/socialfi/new-arrivals?limit=20&offset=0&userId=562acdb3-50d7-49aa-86d4-1b778da6ca12";

const SHARE_AMOUNT: u64 = 100;
const SLIPPAGE:     f64 = 1.15;

struct Chain {
    name:     &'static str,
    http:     &'static str,
    ws:       &'static str,
    contract: &'static str,
    chain_id: u64,
    symbol:   &'static str,
    explorer: &'static str,
}

const CHAINS: &[Chain] = &[
    Chain {
        name:     "avalanche",
        http:     "https://api.avax.network/ext/bc/C/rpc",
        ws:       "wss://go.getblock.io/acccbcbc392744d083ccaa244a9c10df",
        contract: "0x2Fec21938e4d11117Bda59a5fE880c1d0AE54A7F",
        chain_id: 43114,
        symbol:   "AVAX",
        explorer: "https://snowtrace.io/tx/",
    },
    Chain {
        name:     "bsc",
        http:     "https://go.getblock.asia/d8ef98407a514636b4c0daad4fc714fd",
        ws:       "wss://bsc-rpc.publicnode.com",
        contract: "0xC12ab9BC529809d6041564FE6aC65FAF8e190E7B",
        chain_id: 56,
        symbol:   "BNB",
        explorer: "https://bscscan.com/tx/",
    },
];

abigen!(
    BoiContract,
    r#"[
        function buyShares(address _sharesSubject, address _to, uint256 _amount) external payable
        function getBuyPriceAfterFee(address _sharesSubject, uint256 _amount) external view returns (uint256)
        function sharesSupply(address _sharesSubject) external view returns (uint256)
        function sharesBalance(address _sharesSubject, address _holder) external view returns (uint256)
        function sellShares(address _sharesSubject, address _to, uint256 _amount) external
        function getSellPriceAfterFee(address _sharesSubject, uint256 _amount) external view returns (uint256)
    ]"#
);

// ── STRUCTS ───────────────────────────────────────────
#[derive(Debug, Deserialize, Clone)]
struct User {
    id:             String,
    username:       String,
    wallet_address: Option<String>,
    selected_chain: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    users: Vec<User>,
}

// ── TELEGRAM ──────────────────────────────────────────
async fn tg(client: &Client, token: &str, chat_id: &str, text: &str) {
    let _ = client
        .post(format!("https://api.telegram.org/bot{}/sendMessage", token))
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML"
        }))
        .send()
        .await;
}

// ── BUY ───────────────────────────────────────────────
async fn buy_shares(
    username:    &str,
    wallet:      &str,
    chain:       &Chain,
    my_wallet:   &str,
    private_key: &str,
    bot_token:   &str,
    chat_id:     &str,
    http_client: &Client,
    source:      &str,
) {
    println!(
        "{} [{source}] @{username} on {} — firing!",
        "⚡".yellow(),
        chain.name.to_uppercase().cyan()
    );

    let t0 = std::time::Instant::now();

    let provider = match Provider::<Http>::try_from(chain.http) {
        Ok(p) => Arc::new(p),
        Err(e) => { println!("{} Provider error: {e}", "❌".red()); return; }
    };

    let signer = match private_key.parse::<LocalWallet>() {
        Ok(w) => w.with_chain_id(chain.chain_id),
        Err(e) => { println!("{} Wallet error: {e}", "❌".red()); return; }
    };

    let client  = Arc::new(SignerMiddleware::new(provider.clone(), signer));
    let contract = BoiContract::new(
        chain.contract.parse::<Address>().unwrap(),
        client.clone(),
    );

    let subject: Address = match wallet.parse() {
        Ok(a) => a,
        Err(e) => { println!("{} Address error: {e}", "❌".red()); return; }
    };

    let me: Address = my_wallet.parse().unwrap();
    let amount      = U256::from(SHARE_AMOUNT);

    // Get price
    let price = match contract.get_buy_price_after_fee(subject, amount).call().await {
        Ok(p) => p,
        Err(e) => { println!("{} Price error: {e}", "❌".red()); return; }
    };

    let price_with_slip = U256::from((price.as_u128() as f64 * SLIPPAGE) as u128);
    let price_eth       = ethers::utils::format_ether(price);
    println!("{} Price: {} {}", "💰".green(), price_eth, chain.symbol);

    // Check balance
    let balance = match provider.get_balance(me, None).await {
        Ok(b) => b,
        Err(e) => { println!("{} Balance error: {e}", "❌".red()); return; }
    };

    if balance < price_with_slip {
        let bal_eth = ethers::utils::format_ether(balance);
        println!("{} Insufficient balance: {} {}", "❌".red(), bal_eth, chain.symbol);
        tg(http_client, bot_token, chat_id,
            &format!("❌ Low balance on {}!\nNeed: {} {}\nHave: {} {}",
                chain.name.to_uppercase(), price_eth, chain.symbol, bal_eth, chain.symbol)
        ).await;
        return;
    }

    // Build the call
    let call = contract
        .buy_shares(subject, me, amount)
        .value(price_with_slip)
        .gas(250_000u64);

    // Fire tx
    let pending = match call.send().await {
        Ok(p) => p,
        Err(e) => {
            println!("{} Send error: {e}", "❌".red());
            return;
        }
    };

    let tx_hash = format!("{:?}", pending.tx_hash());
    println!("{} TX: {}{}", "📡".blue(), chain.explorer, tx_hash);

    // Wait for receipt
    match pending.await {
        Ok(Some(receipt)) => {
            let elapsed = t0.elapsed().as_millis();
            let now     = Local::now().format("%Y-%m-%d %I:%M %p").to_string();

            if receipt.status == Some(1u64.into()) {
                println!("{} Sniped @{username} on {}! {}ms",
                    "✅".green(), chain.name.to_uppercase(), elapsed);

                tg(http_client, bot_token, chat_id, &format!(
                    "🎯 <b>Sniped @{username}!</b>\n\n\
                     ⛓️ {}\n\
                     📡 Source: {source}\n\
                     💰 Paid: {} {}\n\
                     ⚡ {}ms\n\
                     🕐 {now}\n\
                     🔗 {}{}",
                    chain.name.to_uppercase(),
                    price_eth, chain.symbol,
                    elapsed,
                    chain.explorer, tx_hash
                )).await;
            } else {
                println!("{} TX reverted for @{username}", "❌".red());
                tg(http_client, bot_token, chat_id, &format!(
                    "❌ <b>Reverted @{username}</b>\n⛓️ {}\n🔗 {}{}",
                    chain.name.to_uppercase(), chain.explorer, tx_hash
                )).await;
            }
        }
        _ => println!("{} Receipt timeout for @{username}", "⚠️".yellow()),
    }
}

// ── MEMPOOL WATCHER ───────────────────────────────────
async fn watch_mempool(
    chain:       &'static Chain,
    fired:       Arc<Mutex<HashSet<String>>>,
    my_wallet:   String,
    private_key: String,
    bot_token:   String,
    chat_id:     String,
    http_client: Client,
) {
    // BUY_SHARES selector: keccak256("buyShares(address,address,uint256)")[:4]
    let buy_sel = "dd06f6bd";

    loop {
        println!("{} [{}] Connecting mempool WS...", "🔌".yellow(), chain.name.to_uppercase());

        match Provider::<Ws>::connect(chain.ws).await {
            Ok(provider) => {
                println!("{} [{}] Mempool live!", "✅".green(), chain.name.to_uppercase());

                let contract_lc = chain.contract.to_lowercase();

                match provider.subscribe_pending_txs().await {
                    Ok(mut stream) => {
                        while let Some(tx_hash) = stream.next().await {
                            // Get full tx
                            let p2 = provider.clone();
                            let contract_lc2  = contract_lc.clone();
                            let fired2        = fired.clone();
                            let my_wallet2    = my_wallet.clone();
                            let private_key2  = private_key.clone();
                            let bot_token2    = bot_token.clone();
                            let chat_id2      = chat_id.clone();
                            let http_client2  = http_client.clone();
                            let buy_sel2      = buy_sel.to_string();

                            tokio::spawn(async move {
                                if let Ok(Some(tx)) = p2.get_transaction(tx_hash).await {
                                    let to = tx.to.map(|a| format!("{:?}", a).to_lowercase())
                                        .unwrap_or_default();

                                    if to != contract_lc2 {
                                        return;
                                    }

                                    let input = hex::encode(&tx.input);
                                    if !input.starts_with(&buy_sel2) {
                                        return;
                                    }

                                    // Decode subject from calldata
                                    // Layout: [4 sel][32 subject][32 to][32 amount]
                                    if input.len() < 8 + 64 {
                                        return;
                                    }

                                    let subject_hex = &input[8 + 24..8 + 64];
                                    let subject = format!("0x{}", subject_hex);

                                    println!("{} [{}] Mempool: {}",
                                        "🔭".yellow(),
                                        chain.name.to_uppercase(),
                                        &subject[..12]
                                    );

                                    // Dedup
                                    {
                                        let mut f = fired2.lock().await;
                                        if f.contains(&subject.to_lowercase()) {
                                            return;
                                        }
                                        f.insert(subject.to_lowercase());
                                    }

                                    buy_shares(
                                        &subject[..10],
                                        &subject,
                                        chain,
                                        &my_wallet2,
                                        &private_key2,
                                        &bot_token2,
                                        &chat_id2,
                                        &http_client2,
                                        "MEMPOOL",
                                    ).await;
                                }
                            });
                        }
                    }
                    Err(e) => println!("{} Subscribe error: {e}", "⚠️".yellow()),
                }
            }
            Err(e) => println!("{} WS connect error: {e}", "⚠️".yellow()),
        }

        println!("{} [{}] WS disconnected — retry in 5s",
            "🔌".yellow(), chain.name.to_uppercase());
        sleep(Duration::from_secs(5)).await;
    }
}

// ── API POLLER ────────────────────────────────────────
async fn poll_api(
    http_client: Client,
    fired:       Arc<Mutex<HashSet<String>>>,
    my_wallet:   String,
    private_key: String,
    bot_token:   String,
    chat_id:     String,
    cookie:      String,
) {
    let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let mut first = true;

    println!("{} API poller started", "🌐".green());

    loop {
        let res = http_client
            .get(BOI_API)
            .header("cookie",     &cookie)
            .header("referer",    "https://www.boithebear.com/")
            .header("user-agent", "Mozilla/5.0 Chrome/147.0.0.0")
            .send()
            .await;

        if let Ok(r) = res {
            if let Ok(data) = r.json::<ApiResponse>().await {
                let mut seen_lock = seen.lock().await;

                if first {
                    for u in &data.users {
                        seen_lock.insert(u.id.clone());
                        if let Some(w) = &u.wallet_address {
                            fired.lock().await.insert(w.to_lowercase());
                        }
                    }
                    first = false;
                    println!("{} Seeded {} users", "🌱".green(), seen_lock.len());
                } else {
                    for u in data.users {
                        if seen_lock.contains(&u.id) {
                            continue;
                        }
                        seen_lock.insert(u.id.clone());

                        let chain_name = u.selected_chain.clone()
                            .unwrap_or_default().to_lowercase();
                        let username   = u.username.clone();
                        let wallet     = match u.wallet_address {
                            Some(w) => w,
                            None    => continue,
                        };

                        println!("{} [API] @{username} | {chain_name}",
                            "🆕".yellow());

                        let chain = CHAINS.iter().find(|c| c.name == chain_name);
                        if chain.is_none() {
                            println!("{} Chain '{chain_name}' not supported",
                                "⏭️".yellow());
                            continue;
                        }
                        let chain = chain.unwrap();

                        // Dedup
                        {
                            let mut f = fired.lock().await;
                            if f.contains(&wallet.to_lowercase()) {
                                continue;
                            }
                            f.insert(wallet.to_lowercase());
                        }

                        let mw  = my_wallet.clone();
                        let pk  = private_key.clone();
                        let bt  = bot_token.clone();
                        let ci  = chat_id.clone();
                        let hc  = http_client.clone();
                        let w   = wallet.clone();

                        tokio::spawn(async move {
                            buy_shares(
                                &username, &w, chain,
                                &mw, &pk, &bt, &ci, &hc,
                                "API"
                            ).await;
                        });
                    }
                }
            }
        }

        sleep(Duration::from_millis(500)).await;
    }
}

// ── MAIN ─────────────────────────────────────────────
#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    let private_key = env::var("PRIVATE_KEY").expect("PRIVATE_KEY missing");
    let my_wallet   = env::var("MY_WALLET").expect("MY_WALLET missing");
    let bot_token   = env::var("BOT_TOKEN").expect("BOT_TOKEN missing");
    let chat_id     = env::var("CHAT_ID").expect("CHAT_ID missing");
    let cookie = env::var("COOKIE").unwrap_or_default();

    println!("{}", "╔═══════════════════════════════════════╗".cyan());
    println!("{}", "║   BOI THE BEAR — RUST SNIPER v1.0    ║".cyan().bold());
    println!("{}", "╠═══════════════════════════════════════╣".cyan());
    println!("║  💳 {}", &my_wallet[..20]);
    println!("║  ⛓️  AVALANCHE + BSC");
    println!("║  🔭 Mempool WS + API fallback");
    println!("║  💣 {} units per snipe", SHARE_AMOUNT);
    println!("║  🛡️  {}% slippage", ((SLIPPAGE - 1.0) * 100.0) as u32);
    println!("{}", "╚═══════════════════════════════════════╝".cyan());
    println!();

    let http_client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let fired: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    // Send startup telegram
    tg(&http_client, &bot_token, &chat_id,
        &format!("🦀 <b>BOI Rust Sniper v1.0 live!</b>\n\n\
                  ⛓️ AVALANCHE + BSC\n\
                  🔭 Mempool + API\n\
                  💳 <code>{my_wallet}</code>")
    ).await;

    let mut handles = vec![];

    // Mempool watchers
    for chain in CHAINS {
        let fired2  = fired.clone();
        let mw      = my_wallet.clone();
        let pk      = private_key.clone();
        let bt      = bot_token.clone();
        let ci      = chat_id.clone();
        let hc      = http_client.clone();

        handles.push(tokio::spawn(async move {
            watch_mempool(chain, fired2, mw, pk, bt, ci, hc).await;
        }));
    }

    // API poller
    {
        let fired2 = fired.clone();
        let mw     = my_wallet.clone();
        let pk     = private_key.clone();
        let bt     = bot_token.clone();
        let ci     = chat_id.clone();
        let hc     = http_client.clone();
        let ck     = cookie.clone();

        handles.push(tokio::spawn(async move {
            poll_api(hc, fired2, mw, pk, bt, ci, ck).await;
        }));
    }

    // Heartbeat
    handles.push(tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(60)).await;
            println!("{} alive", "💓".red());
        }
    }));

    futures::future::join_all(handles).await;

    Ok(())
}