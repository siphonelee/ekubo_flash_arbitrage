use clap::{Parser, ValueEnum};
use color_eyre::eyre::{bail, ensure, eyre, Result};
use futures::future::join_all;
use starknet::{
    accounts::{Account, Call, ConnectedAccount, ExecutionEncoding, SingleOwnerAccount},
    core::{
        chain_id,
        types::{
            BlockId, BlockTag, Felt, FunctionCall, TransactionReceiptWithBlockInfo,
            TransactionStatus, U256,
        },
        utils::get_selector_from_name,
    },
    providers::{
        jsonrpc::{HttpTransport, JsonRpcClient},
        Provider, Url,
    },
    signers::{LocalWallet, SigningKey},
};
use std::{cmp::Reverse, str::FromStr};
use std::collections::HashSet;
use std::env;
use std::iter;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod client;
use client::Client;

pub mod ekubo;

use ekubo::models::{pool_key::PoolKey, quotes::Quotes, route_node::RouteNode};
use ekubo::models::quote::Quote;

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum Mode {
    /// atomic arbitrage with my own money
    Direct,
    /// atomic arbitrage with Ekubo flash loan
    Flash,
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[arg(value_enum)]
    mode: Mode,
}

struct Opportunity {
    amount: Felt,
    quotes: Quotes, 
    profit: Felt,
}

impl Opportunity {
    fn generate_strategy(
        self,
        arbitrage_address: Felt,
        token_address: Felt,
        mode: Mode,
    ) -> Option<(Felt, Felt, Vec<Call>)> {
        let amount = self.amount;
        let profit = self.profit;
        let mut splits = self.quotes.splits;
        let call = if splits.len() == 1 {
            let split = splits.pop()?;
            if split.route.len() == 1 {
                error!("unexpected single hop route");
                return None;
            }
            Call {
                to: arbitrage_address,
                selector: get_selector_from_name("multihop_swap").unwrap(),
                calldata: call_data_for_split(split, token_address).collect(),
            }
        } else {
            Call {
                to: arbitrage_address,
                selector: get_selector_from_name("multi_multihop_swap").unwrap(),
                calldata: call_data_for_multisplit(splits, token_address).collect(),
            }
        };
        let calls = match mode {
            Mode::Direct => {
                let transfer_call = Call {
                    to: token_address,
                    selector: get_selector_from_name("transfer").unwrap(),
                    calldata: vec![arbitrage_address, amount, Felt::ZERO],
                };

                let clear_profits_call = Call {
                    to: arbitrage_address,
                    selector: get_selector_from_name("clear_minimum").unwrap(),
                    calldata: vec![token_address, amount, Felt::ZERO],
                };
                vec![transfer_call, call, clear_profits_call]
            }
            Mode::Flash => {
                vec![call]
            }
        };

        Some((profit, amount, calls))
    }
}

async fn check_opportunities(client: &Client, amount: Felt, min_profit: Felt, token_address: &str,
                            max_splits: u8, max_hops: u8, official_extensions: &HashSet<Felt>) -> Option<Opportunity> {
    let quotes = client
        .quotes(amount, token_address, token_address, max_splits, max_hops)
        .await
        .map_err(|e| {
            error!("quotes err: {e:#?}");
            e
        })
        .ok()?;
    debug!("quotes for amount {amount}:\n{quotes:#?}");
    let total = quotes.total;
    let extensions: HashSet<Felt> = quotes
        .splits
        .iter()
        .map(|quote| quote.route.iter())
        .flatten()
        .filter_map(|node| {
            let ext = node.pool_key.extension;
            (ext != Felt::ZERO).then_some(ext)
        })
        .collect();

    (extensions.is_subset(official_extensions)
        && total > amount + min_profit
        && !quotes.splits.is_empty())
    .then(|| Opportunity {
        amount,
        quotes,
        profit: total - amount,
    })
}

fn node_to_array(node: RouteNode) -> [Felt; 8] {
    let RouteNode {
        pool_key:
            PoolKey {
                token0,
                token1,
                fee,
                tick_spacing,
                extension,
            },
        sqrt_ratio_limit,
        skip_ahead,
    } = node;
    let sqrt_ratio = U256::from(sqrt_ratio_limit);
    let low = Felt::from(sqrt_ratio.low());
    let high = Felt::from(sqrt_ratio.high());
    [
        token0,
        token1,
        fee,
        tick_spacing.into(),
        extension,
        low,
        high,
        Felt::from(skip_ahead),
    ]
}

fn call_data_for_split(split: Quote, token_address: Felt) -> impl Iterator<Item = Felt> {
    let specified_amount = split.specified_amount;
    iter::once(Felt::from(split.route.len()))
        .chain(split.route.into_iter().flat_map(node_to_array))
        .chain(iter::once(token_address))
        .chain(iter::once(specified_amount))
        .chain(iter::once(Felt::ZERO)) // we specify an exact input amount, so it is positive
}

fn call_data_for_multisplit(splits: Vec<Quote>, token_address: Felt) -> impl Iterator<Item = Felt> {
    iter::once(Felt::from(splits.len())).chain(
        splits
            .into_iter()
            .flat_map(move |split| call_data_for_split(split, token_address)),
    )
}

fn get_chain_id(ekubo_url: &str, provider_url: &str) -> Result<Felt> {
    if ekubo_url.contains("sepolia") {
        ensure!(provider_url.contains("sepolia"), "Ekubo API and RPC provider urls should all point to Sepolia");
        Ok(chain_id::SEPOLIA)
    } else if ekubo_url.contains("mainnet") {
        ensure!(provider_url.contains("mainnet"), "Ekubo API and RPC provider urls should all point to Mainnet");
        Ok(chain_id::MAINNET)
    } else {
        bail!("unsupported chain - verify environment variables")
    }
}

async fn get_account_balance(token_contract: Felt, account_address: Felt, provider: &JsonRpcClient<HttpTransport>) -> Result<U256> {
    let felts = provider
        .call(FunctionCall {
                contract_address: token_contract,
                entry_point_selector: get_selector_from_name("balanceOf")?,
                calldata: vec![account_address],
            },
            BlockId::Tag(BlockTag::Latest),
        )
        .await
        .map_err(|e| eyre!("Error when fetching account balance:\n{e:#?}"))?;
    let low = u128::from_le_bytes(felts[0].to_bytes_le()[0..16].try_into()?);
    let high = u128::from_le_bytes(felts[1].to_bytes_le()[0..16].try_into()?);
    Ok(U256::from_words(low, high))
}

async fn wait_for_transaction(provider: &JsonRpcClient<HttpTransport>, tx_hash: Felt) -> Result<TransactionReceiptWithBlockInfo> {
    let mut retries = 200;
    let retry_interval = Duration::from_millis(3000);

    while retries >= 0 {
        tokio::time::sleep(retry_interval).await; // sleep before the tx status to give some time for a tx get to the provider node
        let status = provider
            .get_transaction_status(tx_hash)
            .await
            .map_err(|e| eyre!("failed to get tx status: {e:#?}"))?;
        retries -= 1;
        match status {
            TransactionStatus::Received => continue,
            TransactionStatus::Rejected => bail!("transaction is rejected"),
            TransactionStatus::AcceptedOnL2(_) | TransactionStatus::AcceptedOnL1(_) => {
                match provider.get_transaction_receipt(tx_hash).await {
                    Ok(receipt) => return Ok(receipt),
                    Err(_) => continue,
                }
            }
        }
    }
    bail!("maximum retries attempts")
}

#[allow(unreachable_code)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Cli::parse();
    dotenvy::dotenv()?;

    color_eyre::install()?;
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();
    let token_address_hex = env::var("TOKEN_TO_ARBITRAGE")?;
    let token_address = Felt::from_hex(&token_address_hex)?;
    let arbitrage_address_hex = match args.mode {
        Mode::Direct => env::var("ROUTER_ADDRESS")?,
        Mode::Flash => env::var("ARBITRAGE_CONTRACT")?,
    };
    let arbitrage_address = Felt::from_hex(&arbitrage_address_hex)?;
    let url = env::var("EKUBO_URL")?;
    let provider_url = env::var("JSON_RPC_URL")?;
    let explorer_url = env::var("EXPLORER_TX_PREFIX")?;
    info!("starting bot with Ekubo API {url} and RPC {provider_url}, strategy: {:?}", args.mode);

    let chain_id = get_chain_id(&url, &provider_url)?;

    let client = Client::new(url, "atomic-bot".to_string());
    let rpc_transport = HttpTransport::new(Url::parse(&provider_url)?);
    let provider = JsonRpcClient::new(rpc_transport);
    ensure!(chain_id == provider.chain_id().await?);

    let signer = LocalWallet::from(SigningKey::from_secret_scalar(Felt::from_hex(&env::var("ACCOUNT_PRIVATE_KEY",)?)?));

    let account_address = Felt::from_hex(&env::var("ACCOUNT_ADDRESS")?)?;

    let mut account = SingleOwnerAccount::new(
        provider,
        signer,
        account_address,
        chain_id,
        ExecutionEncoding::New,
    );
    account.set_block_id(BlockId::Tag(BlockTag::Pending));

    let min_power: u8 = 32.max(env::var("MIN_POWER_OF_2")?.parse()?);
    let max_power: u8 = (min_power + 1).max(65.min(env::var("MAX_POWER_OF_2")?.parse()?));

    let amounts_to_quote: Vec<Felt> = (min_power..max_power)
        .map(|p| Felt::from(2u8).pow(p))
        .collect();

    let max_splits: u8 = env::var("MAX_SPLITS")?.parse()?;
    let max_hops: u8 = env::var("MAX_HOPS")?.parse()?;
    let min_profit = Felt::from_dec_str(&env::var("MIN_PROFIT")?)?;
    let num_top_quotes: usize = env::var("NUM_TOP_QUOTES_TO_ESTIMATE")?.parse()?;
    let check_interval: u64 = env::var("CHECK_INTERVAL_MS")?.parse()?;
    let twamm_extension = Felt::from_hex(&env::var("TWAMM_EXTENSION")?)?;
    let official_extensions = HashSet::from([twamm_extension]);

    loop {
        let account_balance = get_account_balance(token_address, account_address, account.provider()).await?;
        info!("Account balance: {account_balance}");

        let mut opportunities: Vec<Opportunity> = join_all(
            amounts_to_quote
                .iter()
                .filter(|&&amount| {args.mode != Mode::Direct || U256::from(amount) <= account_balance})
                .map(|&amount| {
                    check_opportunities(&client, amount, min_profit, &token_address_hex, max_splits, max_hops,&official_extensions)
                }),
        )
        .await
        .into_iter()
        .flatten()
        .collect();

        info!("opportunities: {}", opportunities.len());

        opportunities.sort_unstable_by_key(|opportunity| Reverse(opportunity.profit));
        let top: Option<(Felt, Felt, Vec<Call>)> = opportunities
            .into_iter()
            .take(num_top_quotes)
            .find_map(|opportunity| {
                opportunity.generate_strategy(arbitrage_address, token_address, args.mode)
            });
        if let Some((profit, amount, calls)) = top {
            info!("top arbitrage profit: {profit}, amount {amount}");
            info!("Executing top arbitrage:\n{calls:#?}");
            
            let cost = account.execute_v1(calls.to_vec()).estimate_fee().await?;
            
            let total_gas_cost_wei = cost.overall_fee;
            info!("cost etimation:\n{total_gas_cost_wei:#?}");
            let limit_fee = total_gas_cost_wei * Felt::TWO;
            info!("cost etimation:\n{total_gas_cost_wei}");
            info!("profit etimation:\n{profit}, limit fee:\n{limit_fee}");

            if profit > limit_fee {
                let tx = account
                    .execute_v1(calls.to_vec())
                    .max_fee(limit_fee)
                    .send()
                    .await
                    .map_err(|e| eyre!("Error while sending arbitrage transaction:\n{e:#?}"))?;
                info!("sent transaction:\n{explorer_url}{:#x}", tx.transaction_hash);

                match wait_for_transaction(account.provider(), tx.transaction_hash).await {
                    Ok(receipt) => info!("Transaction receipt: {receipt:#?}"),
                    Err(e) => error!("Arbitrage transaction failed: {e:#?}"),
                }
            } else {
                info!("Non-profitable opportunity");
            }
        }
        sleep(Duration::from_millis(check_interval)).await;
    }

    Ok(())
}
