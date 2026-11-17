//! tipper: tops up a list of accounts to a target balance from one funder key.
//!
//! Every `interval_secs` it reads each account's balance and, when it is below `target`, sends
//! the difference from the funder. One YAML file lists the accounts; the funder key comes from
//! the environment only.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use alloy_network::{EthereumWallet, TransactionBuilder};
use alloy_primitives::{
    Address, U256,
    utils::{format_ether, parse_ether},
};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use anyhow::{Context, Result, bail};
use clap::Parser;
use metrics::{counter, gauge};
use metrics_exporter_prometheus::PrometheusBuilder;
use serde::Deserialize;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// The YAML file listing the accounts.
    #[arg(long, default_value = "/etc/tipper/config.yaml")]
    config: PathBuf,
    /// The funder's private key, hex. Environment only: never put it in the config.
    #[arg(long, env = "FUNDER_KEY", hide_env_values = true)]
    funder_key: Option<String>,
    /// Overrides `rpc_url` from the config.
    #[arg(long, env = "RPC_URL")]
    rpc_url: Option<String>,
    /// One pass, then exit.
    #[arg(long)]
    once: bool,
    /// Read balances and say what would be sent; no key needed.
    #[arg(long)]
    dry_run: bool,
    /// Where Prometheus metrics are served (tipper_balance_ether, tipper_topups_total, ...).
    #[arg(long, env = "METRICS_ADDR", default_value = "0.0.0.0:9090")]
    metrics_addr: SocketAddr,
}

#[derive(Deserialize)]
struct Config {
    rpc_url: Option<String>,
    #[serde(default = "default_interval")]
    interval_secs: u64,
    /// Upper bound on one top-up, in ETH. A typo in a target must not drain the funder.
    max_topup: Option<f64>,
    accounts: Vec<Account>,
}

fn default_interval() -> u64 {
    600
}

#[derive(Deserialize)]
struct Account {
    name: String,
    address: Address,
    /// Target balance in ETH; the account is topped up to it when below.
    target: f64,
}

/// For gauges only: ETH as a float, fine for a dashboard, never for arithmetic.
fn ether_f64(wei: U256) -> f64 {
    format_ether(wei).parse().unwrap_or(f64::NAN)
}

fn eth(amount: f64) -> Result<U256> {
    if !amount.is_finite() || amount <= 0.0 {
        bail!("not a positive ETH amount: {amount}");
    }
    parse_ether(&amount.to_string()).with_context(|| format!("not an ETH amount: {amount}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    PrometheusBuilder::new()
        .with_http_listener(args.metrics_addr)
        .install()
        .with_context(|| format!("serving metrics on {}", args.metrics_addr))?;
    let config: Config = serde_yaml_ng::from_str(
        &std::fs::read_to_string(&args.config).with_context(|| format!("reading {}", args.config.display()))?,
    )
    .context("parsing the config")?;
    if config.accounts.is_empty() {
        bail!("no accounts in the config");
    }
    let rpc_url = args.rpc_url.or(config.rpc_url).context("rpc_url: set it in the config or RPC_URL")?;
    let max_topup = config.max_topup.map(eth).transpose()?;

    // A dry run without a key signs nothing, so any signer does; it keeps one provider type.
    let signer = match &args.funder_key {
        Some(key) => key.parse::<PrivateKeySigner>().context("FUNDER_KEY is not a private key")?,
        None if args.dry_run => PrivateKeySigner::random(),
        None => bail!("FUNDER_KEY is not set (use --dry-run to only read balances)"),
    };
    let funder = args.funder_key.is_some().then(|| signer.address());
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect(&rpc_url)
        .await
        .context("connecting to the RPC")?;
    // Register the counters at zero, so a scrape before the first top-up already sees them.
    for account in &config.accounts {
        counter!("tipper_topups_total", "account" => account.name.clone()).absolute(0);
        counter!("tipper_topup_failures_total", "account" => account.name.clone()).absolute(0);
    }
    info!(rpc = %rpc_url, accounts = config.accounts.len(), interval_secs = config.interval_secs, funder = ?funder, "tipper started");

    loop {
        if let Some(funder) = funder {
            match provider.get_balance(funder).await {
                Ok(b) => {
                    gauge!("tipper_funder_balance_ether").set(ether_f64(b));
                    info!(balance = %format_ether(b), "funder");
                }
                Err(e) => warn!(error = %e, "funder balance"),
            }
        }
        for account in &config.accounts {
            if let Err(e) = top_up(&provider, account, max_topup, args.dry_run).await {
                counter!("tipper_topup_failures_total", "account" => account.name.clone()).increment(1);
                error!(account = account.name, error = %e, "top-up failed");
            }
        }
        if args.once {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(config.interval_secs)).await;
    }
}

async fn top_up(provider: &impl Provider, account: &Account, max_topup: Option<U256>, dry_run: bool) -> Result<()> {
    let target = eth(account.target)?;
    let balance = provider.get_balance(account.address).await.context("reading the balance")?;
    gauge!("tipper_balance_ether", "account" => account.name.clone()).set(ether_f64(balance));
    if balance >= target {
        info!(account = account.name, balance = %format_ether(balance), target = account.target, "ok");
        return Ok(());
    }
    let mut amount = target - balance;
    if let Some(cap) = max_topup
        && amount > cap
    {
        warn!(account = account.name, wanted = %format_ether(amount), cap = %format_ether(cap), "top-up capped");
        amount = cap;
    }
    if dry_run {
        info!(account = account.name, balance = %format_ether(balance), send = %format_ether(amount), "would top up (dry run)");
        return Ok(());
    }
    let tx = TransactionRequest::default().with_to(account.address).with_value(amount);
    let pending = provider.send_transaction(tx).await.context("sending")?;
    let hash = *pending.tx_hash();
    info!(account = account.name, balance = %format_ether(balance), send = %format_ether(amount), tx = %hash, "topping up");
    let receipt = pending.get_receipt().await.context("waiting for the receipt")?;
    if receipt.status() {
        counter!("tipper_topups_total", "account" => account.name.clone()).increment(1);
        // A gauge that only grows: counters are integers and ETH is not.
        gauge!("tipper_topped_up_ether", "account" => account.name.clone()).increment(ether_f64(amount));
        info!(account = account.name, tx = %hash, block = receipt.block_number.unwrap_or_default(), "topped up");
    } else {
        bail!("transaction {hash} reverted");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses_and_amounts_convert() {
        let config: Config = serde_yaml_ng::from_str(include_str!("../config.example.yaml")).unwrap();
        assert_eq!(config.interval_secs, 600);
        assert_eq!(config.accounts.len(), 3);
        assert_eq!(eth(config.accounts[2].target).unwrap(), parse_ether("0.5").unwrap());
        assert_eq!(eth(config.max_topup.unwrap()).unwrap(), parse_ether("2").unwrap());
        assert_eq!(ether_f64(parse_ether("1.25").unwrap()), 1.25);
    }

    #[test]
    fn missing_interval_defaults_and_bad_amounts_fail() {
        let config: Config = serde_yaml_ng::from_str(
            "accounts:\n  - {name: a, address: \"0x0000000000000000000000000000000000000001\", target: 1}\n",
        )
        .unwrap();
        assert_eq!(config.interval_secs, 600);
        assert!(config.max_topup.is_none());
        assert!(eth(-1.0).is_err());
    }
}
