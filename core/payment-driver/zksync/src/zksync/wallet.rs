/*
    Wallet functions on zksync.
*/

// External crates
use bigdecimal::{BigDecimal, Zero};
use num_bigint::BigUint;
use std::env;
use std::str::FromStr;
use zksync::operations::SyncTransactionHandle;
use zksync::types::BlockStatus;
use zksync::zksync_types::{tx::TxHash, Address, Nonce, TxFeeTypes};
use zksync::{
    provider::get_rpc_addr,
    provider::{Provider, RpcProvider},
    Network as ZkNetwork, Wallet, WalletCredentials,
};
use zksync_eth_signer::EthereumSigner;

// Workspace uses
use ya_payment_driver::{
    db::models::Network,
    model::{AccountMode, Exit, GenericError, Init, PaymentDetails},
};

// Local uses
use crate::{
    network::get_network_token,
    zksync::{faucet, signer::YagnaEthSigner, utils},
    DEFAULT_NETWORK,
};

pub async fn account_balance(address: &str, network: Network) -> Result<BigDecimal, GenericError> {
    let pub_address = Address::from_str(&address[2..]).map_err(GenericError::new)?;
    let acc_info = get_provider(network)
        .account_info(pub_address)
        .await
        .map_err(GenericError::new)?;
    // TODO: implement tokens, replace None
    let token = get_network_token(network, None);
    let balance_com = acc_info
        .committed
        .balances
        .get(&token)
        .map(|x| x.0.clone())
        .unwrap_or(BigUint::zero());
    let balance = utils::big_uint_to_big_dec(balance_com);
    log::debug!(
        "account_balance. address={}, network={}, balance={}",
        address,
        &network,
        &balance
    );
    Ok(balance)
}

pub async fn init_wallet(msg: &Init) -> Result<(), GenericError> {
    log::debug!("init_wallet. msg={:?}", msg);
    let mode = msg.mode();
    let address = msg.address().clone();
    let network = msg.network().unwrap_or(DEFAULT_NETWORK.to_string());
    let network = Network::from_str(&network).map_err(|e| GenericError::new(e))?;

    if mode.contains(AccountMode::SEND) {
        let wallet = get_wallet(&address, network).await?;
        unlock_wallet(&wallet, network).await?;
    }
    Ok(())
}

pub async fn fund(address: &str, network: Network) -> Result<(), GenericError> {
    if network == Network::Mainnet {
        return Err(GenericError::new("Wallet can not be funded on mainnet."));
    }
    faucet::request_tglm(address, network).await?;
    Ok(())
}

pub async fn exit(msg: &Exit) -> Result<String, GenericError> {
    let network = msg.network().unwrap_or(DEFAULT_NETWORK.to_string());
    let network = Network::from_str(&network).map_err(|e| GenericError::new(e))?;
    let wallet = get_wallet(&msg.sender(), network).await?;
    unlock_wallet(&wallet, network).await?;
    let tx_handle = withdraw(wallet, network, msg.amount(), msg.to()).await?;
    let tx_info = tx_handle
        .wait_for_commit()
        .await
        .map_err(GenericError::new)?;

    match tx_info.success {
        Some(true) => Ok(hash_to_hex(tx_handle.hash())),
        Some(false) => Err(GenericError::new(
            tx_info
                .fail_reason
                .unwrap_or("Unknown failure reason".to_string()),
        )),
        None => Err(GenericError::new("Transaction time-outed")),
    }
}

pub async fn get_tx_fee(address: &str, network: Network) -> Result<BigDecimal, GenericError> {
    let token = get_network_token(network, None);
    let wallet = get_wallet(&address, network).await?;
    let tx_fee = wallet
        .provider
        .get_tx_fee(TxFeeTypes::Transfer, wallet.address(), token.as_str())
        .await
        .map_err(GenericError::new)?
        .total_fee;
    let tx_fee_bigdec = utils::big_uint_to_big_dec(tx_fee);

    log::debug!("Transaction fee {:.5} {}", tx_fee_bigdec, token.as_str());
    Ok(tx_fee_bigdec)
}

fn hash_to_hex(hash: TxHash) -> String {
    // TxHash::to_string adds a prefix to the hex value
    hex::encode(hash.as_ref())
}

pub async fn get_nonce(address: &str, network: Network) -> u32 {
    let addr = match Address::from_str(&address[2..]) {
        Ok(a) => a,
        Err(e) => {
            log::error!("Unable to parse address, failed to get nonce. {:?}", e);
            return 0;
        }
    };
    let provider = get_provider(network);
    let account_info = match provider.account_info(addr).await {
        Ok(i) => i,
        Err(e) => {
            log::error!("Unable to get account info, failed to get nonce. {:?}", e);
            return 0;
        }
    };
    *account_info.committed.nonce
}

pub async fn make_transfer(
    details: &PaymentDetails,
    nonce: u32,
    network: Network,
) -> Result<String, GenericError> {
    log::debug!("make_transfer. {:?}", details);
    let amount = details.amount.clone();
    let amount = utils::big_dec_to_big_uint(amount)?;
    let amount = utils::pack_up(&amount);

    let sender = details.sender.clone();
    let wallet = get_wallet(&sender, network).await?;
    let token = get_network_token(network, None);

    let balance = wallet
        .get_balance(BlockStatus::Committed, token.as_str())
        .await
        .map_err(GenericError::new)?;
    log::debug!("balance before transfer={}", balance);

    let transfer_builder = wallet
        .start_transfer()
        .nonce(Nonce(nonce))
        .str_to(&details.recipient[2..])
        .map_err(GenericError::new)?
        .token(token.as_str())
        .map_err(GenericError::new)?
        .amount(amount.clone());
    log::debug!(
        "transfer raw data. nonce={}, to={}, token={}, amount={}",
        nonce,
        &details.recipient,
        token,
        amount
    );
    let transfer = transfer_builder.send().await.map_err(GenericError::new)?;

    let tx_hash = hex::encode(transfer.hash());
    log::info!("Created zksync transaction with hash={}", tx_hash);
    Ok(tx_hash)
}

pub async fn check_tx(tx_hash: &str, network: Network) -> Option<Result<(), String>> {
    let provider = get_provider(network);
    let tx_hash = format!("sync-tx:{}", tx_hash);
    let tx_hash = TxHash::from_str(&tx_hash).unwrap();
    let tx_info = provider.tx_info(tx_hash).await.unwrap();
    log::trace!("tx_info: {:?}", tx_info);
    match tx_info.success {
        None => None,
        Some(true) => Some(Ok(())),
        Some(false) => match tx_info.fail_reason {
            Some(err) => Some(Err(err)),
            None => Some(Err("Unknown failure".to_string())),
        },
    }
}

#[derive(serde::Deserialize)]
struct TxRespObj {
    to: String,
    from: String,
    amount: String,
    created_at: String,
}

pub async fn verify_tx(tx_hash: &str, network: Network) -> Result<PaymentDetails, GenericError> {
    let provider_url = match get_rpc_addr_from_env(network) {
        Some(rpc_addr) => rpc_addr,
        None => get_rpc_addr(get_zk_network(network)).to_string(),
    };
    // HACK: Get the transaction data from v0.1 api
    let api_url = provider_url.replace("/jsrpc", "/api/v0.1");
    let req_url = format!("{}/transactions_all/{}", api_url, tx_hash);
    log::debug!("Request URL: {}", &req_url);

    let client = awc::Client::new();
    let response = client
        .get(req_url)
        .send()
        .await
        .map_err(GenericError::new)?
        .body()
        .await
        .map_err(GenericError::new)?;
    let response = String::from_utf8_lossy(response.as_ref());
    log::trace!("Request response: {}", &response);
    let v: TxRespObj = serde_json::from_str(&response).map_err(GenericError::new)?;

    let recipient = v.to;
    let sender = v.from;
    let amount =
        utils::big_uint_to_big_dec(BigUint::from_str(&v.amount).map_err(GenericError::new)?);
    let date_str = format!("{}Z", v.created_at);
    let date = Some(chrono::DateTime::from_str(&date_str).map_err(GenericError::new)?);
    let details = PaymentDetails {
        recipient,
        sender,
        amount,
        date,
    };
    log::debug!("PaymentDetails from server: {:?}", &details);

    Ok(details)
}

fn get_provider(network: Network) -> RpcProvider {
    let zk_network = get_zk_network(network);
    let provider: RpcProvider = match get_rpc_addr_from_env(network) {
        Some(rpc_addr) => RpcProvider::from_addr_and_network(rpc_addr, zk_network),
        None => RpcProvider::new(zk_network),
    };
    provider.clone()
}

fn get_rpc_addr_from_env(network: Network) -> Option<String> {
    match network {
        Network::Mainnet => env::var("ZKSYNC_MAINNET_RPC_ADDRESS").ok(),
        Network::Rinkeby => env::var("ZKSYNC_RINKEBY_RPC_ADDRESS").ok(),
    }
}

async fn get_wallet(
    address: &str,
    network: Network,
) -> Result<Wallet<YagnaEthSigner, RpcProvider>, GenericError> {
    log::debug!("get_wallet {:?}", address);
    let addr = Address::from_str(&address[2..]).map_err(GenericError::new)?;
    let provider = get_provider(network);
    let signer = YagnaEthSigner::new(addr);
    let credentials = WalletCredentials::from_eth_signer(addr, signer, get_zk_network(network))
        .await
        .map_err(GenericError::new)?;
    let wallet = Wallet::new(provider, credentials)
        .await
        .map_err(GenericError::new)?;
    Ok(wallet)
}

fn get_zk_network(network: Network) -> ZkNetwork {
    ZkNetwork::from_str(&network.to_string()).unwrap() // _or(ZkNetwork::Rinkeby)
}

async fn unlock_wallet<S: EthereumSigner + Clone, P: Provider + Clone>(
    wallet: &Wallet<S, P>,
    network: Network,
) -> Result<(), GenericError> {
    log::debug!("unlock_wallet");
    if !wallet
        .is_signing_key_set()
        .await
        .map_err(GenericError::new)?
    {
        log::info!("Unlocking wallet... address = {}", wallet.signer.address);
        let token = get_network_token(network, None);

        let unlock = wallet
            .start_change_pubkey()
            .fee_token(token.as_str())
            .map_err(|e| GenericError::new(format!("Failed to create change_pubkey request: {}", e)))?
            .send()
            .await
            .map_err(|e| GenericError::new(format!("Failed to send change_pubkey request: '{}'. HINT: Did you run `yagna payment fund` and follow the instructions?", e)))?;
        log::info!("Unlock send. tx_hash= {}", unlock.hash().to_string());

        let tx_info = unlock.wait_for_commit().await.map_err(GenericError::new)?;
        log::debug!("tx_info = {:?}", tx_info);
        match tx_info.success {
            Some(true) => log::info!("Wallet successfully unlocked. address = {}", wallet.signer.address),
            Some(false) => return Err(GenericError::new(format!("Failed to unlock wallet. reason={}", tx_info.fail_reason.unwrap_or("Unknown reason".to_string())))),
            None => return Err(GenericError::new(format!("Unknown result from zksync unlock, please check your wallet on zkscan and try again. {:?}", tx_info))),
        }
    }
    Ok(())
}

pub async fn withdraw<S: EthereumSigner + Clone, P: Provider + Clone>(
    wallet: Wallet<S, P>,
    network: Network,
    amount: Option<BigDecimal>,
    recipient: Option<String>,
) -> Result<SyncTransactionHandle<P>, GenericError> {
    let token = get_network_token(network, None);
    let balance = wallet
        .get_balance(BlockStatus::Committed, token.as_str())
        .await
        .map_err(GenericError::new)?;
    info!(
        "Wallet funded with {} {} available for withdrawal",
        utils::big_uint_to_big_dec(balance.clone()),
        token
    );

    info!("Obtaining withdrawal fee");
    let address = wallet.address();
    let withdraw_fee = wallet
        .provider
        .get_tx_fee(TxFeeTypes::Withdraw, address, token.as_str())
        .await
        .map_err(GenericError::new)?
        .total_fee;
    info!(
        "Withdrawal transaction fee {:.5} {}",
        utils::big_uint_to_big_dec(withdraw_fee.clone()),
        token
    );

    let amount = match amount {
        Some(amount) => utils::big_dec_to_big_uint(amount)?,
        None => balance.clone(),
    };
    let withdraw_amount = std::cmp::min(balance - withdraw_fee, amount);
    info!(
        "Withdrawal of {:.5} {} started",
        utils::big_uint_to_big_dec(withdraw_amount.clone()),
        token
    );

    let recipient_address = match recipient {
        Some(addr) => Address::from_str(&addr[2..]).map_err(GenericError::new)?,
        None => address,
    };

    let withdraw_builder = wallet
        .start_withdraw()
        .token(token.as_str())
        .map_err(GenericError::new)?
        .amount(withdraw_amount.clone())
        .to(recipient_address);
    log::debug!(
        "Withdrawal raw data. token={}, amount={}, to={}",
        token,
        withdraw_amount,
        recipient_address
    );
    let withdraw_handle = withdraw_builder.send().await.map_err(GenericError::new)?;

    Ok(withdraw_handle)
}
