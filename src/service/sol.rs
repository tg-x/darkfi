use std::convert::TryFrom;
use std::str::FromStr;

use async_native_tls::TlsConnector;
use async_std::sync::{Arc, Mutex};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use log::{debug, error};
use rand::rngs::OsRng;
use serde::Serialize;
use serde_json::{json, Value};
use solana_client::{blockhash_query::BlockhashQuery, rpc_client::RpcClient};
use solana_sdk::{
    native_token::lamports_to_sol,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Signature, Signer},
    signer::keypair::Keypair,
    system_instruction,
    transaction::Transaction,
};
use spl_associated_token_account::{create_associated_token_account, get_associated_token_address};
use tungstenite::Message;

use crate::rpc::{jsonrpc, jsonrpc::JsonResult, websockets};
use crate::serial::{deserialize, serialize, Decodable, Encodable};
use crate::{Error, Result};

use super::bridge::{NetworkClient, TokenNotification, TokenSubscribtion};

#[derive(Serialize)]
struct SubscribeParams {
    encoding: Value,
    commitment: Value,
}

pub struct SolClient {
    keypair: Keypair,
    // Subscriptions vector of pubkey
    subscriptions: Arc<Mutex<Vec<Pubkey>>>,
    notify_channel: (
        async_channel::Sender<TokenNotification>,
        async_channel::Receiver<TokenNotification>,
    ),
    rpc_server: &'static str,
    wss_server: &'static str,
}

impl SolClient {
    pub async fn new(keypair: Vec<u8>, network: &str) -> Result<Arc<Self>> {
        let keypair: Keypair = deserialize(&keypair)?;
        let notify_channel = async_channel::unbounded();

        debug!("Main SOL wallet pubkey: {:?}", &keypair.pubkey());

        let (rpc_server, wss_server) = match network {
            "mainnet" => (
                "https://api.mainnet-beta.solana.com",
                "wss://api.devnet.solana.com",
            ),
            "devnet" => (
                "https://api.devnet.solana.com",
                "wss://api.devnet.solana.com",
            ),
            "testnet" => (
                "https://api.testnet.solana.com",
                "wss://api.testnet.solana.com",
            ),
            "localhost" => ("http://localhost:8899", "ws://localhost:8900"),
            _ => return Err(Error::NotSupportedNetwork),
        };

        Ok(Arc::new(Self {
            keypair,
            subscriptions: Arc::new(Mutex::new(Vec::new())),
            notify_channel,
            rpc_server,
            wss_server,
        }))
    }

    // TODO: Make this function more robust. Currently we just call it
    // and put it in the background. This means no errors are actually
    // handled, and it just fails silently.
    async fn handle_subscribe_request(
        self: Arc<Self>,
        keypair: Keypair,
        mint: Option<Pubkey>,
    ) -> SolResult<()> {
        debug!(target: "SOL BRIDGE", "handle_subscribe_request()");

        // Derive token pubkey if mint was provided.
        let pubkey = if mint.is_some() {
            get_associated_token_address(&keypair.pubkey(), &mint.unwrap())
        } else {
            keypair.pubkey()
        };

        // Check if we're already subscribed
        if self.subscriptions.lock().await.contains(&pubkey) {
            return Ok(());
        }

        let rpc = RpcClient::new(self.rpc_server.to_string());

        // Fetch the current balance.
        let (prev_balance, decimals) = if mint.is_none() {
            (rpc.get_balance(&pubkey).map_err(SolFailed::from)?, 9)
        } else {
            get_account_token_balance(&rpc, &pubkey, &mint.unwrap()).map_err(SolFailed::from)?
        };

        // WebSocket connection
        let builder = native_tls::TlsConnector::builder();
        let tls = TlsConnector::from(builder);
        let (mut stream, _) = websockets::connect(self.wss_server, tls).await?;

        // Subscription request build
        let sub_params = SubscribeParams {
            encoding: json!("jsonParsed"),
            commitment: json!("finalized"),
        };

        let subscription = jsonrpc::request(
            json!("accountSubscribe"),
            json!([json!(pubkey.to_string()), json!(sub_params)]),
        );

        debug!(target: "SOLANA RPC", "--> {}", serde_json::to_string(&subscription)?);
        stream
            .send(Message::text(serde_json::to_string(&subscription)?))
            .await?;

        // Subscription ID used for unsubscribing later.
        let mut sub_id: i64 = 0;

        // The balance we are going to receive from the JSONRPC notification
        let cur_balance: u64;

        loop {
            let message = stream.next().await.ok_or(Error::TungsteniteError)?;
            let message = message.unwrap();
            debug!(target: "SOLANA SUBSCRIPTION", "<-- {}", message.clone().into_text()?);

            match serde_json::from_slice(&message.into_data())? {
                JsonResult::Resp(r) => {
                    // ACK
                    debug!(target: "SOLANA RPC", "<-- {}", serde_json::to_string(&r)?);
                    self.subscriptions.lock().await.push(pubkey);
                    sub_id = r.result.as_i64().unwrap();
                }
                JsonResult::Err(e) => {
                    debug!(target: "SOLANA RPC", "<-- {}", serde_json::to_string(&e)?);
                    // TODO: Try removing pubkey from subscriptions here?
                    return Err(SolFailed::RpcError(e.error.message.to_string()));
                }
                JsonResult::Notif(n) => {
                    // Account updated
                    debug!(target: "SOLANA RPC", "Got WebSocket notification");
                    let params = n.params["result"]["value"].clone();

                    if mint.is_some() {
                        cur_balance = params["data"]["parsed"]["info"]["tokenAmount"]["amount"]
                            .as_str()
                            .unwrap()
                            .parse()
                            .map_err(|e| SolFailed::from(Error::from(e)))?;
                    } else {
                        cur_balance = params["lamports"].as_u64().unwrap();
                    }
                    break;
                }
            }
        }

        // I miss goto/defer.
        let index = self
            .subscriptions
            .lock()
            .await
            .iter()
            .position(|p| p == &pubkey);
        if let Some(ind) = index {
            debug!("Removing subscription from list");
            self.subscriptions.lock().await.remove(ind);
        }

        let unsubscription = jsonrpc::request(json!("accountUnsubscribe"), json!([sub_id]));
        stream
            .send(Message::text(serde_json::to_string(&unsubscription)?))
            .await?;

        if cur_balance < prev_balance {
            error!("New balance is less than previous balance");
            return Err(SolFailed::Notification(
                "New balance is less than previous balance".into(),
            ));
        }

        if mint.is_some() {
            let amnt = cur_balance - prev_balance;
            let ui_amnt = amnt / u64::pow(10, decimals as u32);
            debug!(target: "SOL BRIDGE", "Received {} {:?} tokens", ui_amnt, mint.unwrap());
            let _ = self.send_tok_to_main_wallet(&rpc, &mint.unwrap(), amnt, decimals, &keypair)?;
        } else {
            let amnt = cur_balance - prev_balance;
            let ui_amnt = lamports_to_sol(amnt);
            debug!(target: "SOL BRIDGE", "Received {} SOL", ui_amnt);
            let _ = self.send_sol_to_main_wallet(&rpc, amnt, &keypair)?;
        }

        Ok(())
    }

    fn send_tok_to_main_wallet(
        self: Arc<Self>,
        rpc: &RpcClient,
        mint: &Pubkey,
        amount: u64,
        decimals: u64,
        keypair: &Keypair,
    ) -> SolResult<Signature> {
        debug!(target: "SOL BRIDGE", "Sending {} {:?} tokens to main wallet",
                amount / u64::pow(10, decimals as u32), mint);

        if !account_is_initialized_mint(rpc, mint) {
            return Err(SolFailed::MintIsNotValid(mint.to_string()));
        }

        // The token account from our main wallet
        let main_tok_pk = get_associated_token_address(&self.keypair.pubkey(), mint);
        // The token account from the deposit wallet
        let temp_tok_pk = get_associated_token_address(&keypair.pubkey(), mint);

        let mut instructions = vec![];

        match rpc.get_account_data(&main_tok_pk) {
            Ok(v) => {
                match spl_token::state::Account::unpack_from_slice(&v) {
                    // It's valid token data, and we consider account initialized.
                    Ok(_) => {}
                    // Some other unexpected data.
                    Err(_) => {
                        return Err(SolFailed::SolClientError(
                            "Invalid data on derived account".into(),
                        ));
                    }
                }
            }
            Err(_) => {
                // Unitinialized, so we add a creation instruction
                debug!("Main wallet token account is uninitialized. Adding init instruction.");
                let init_ix = create_associated_token_account(
                    &self.keypair.pubkey(), // fee payer
                    &self.keypair.pubkey(), // wallet
                    mint,
                );
                instructions.push(init_ix);
            }
        }

        // Transfer tokens from the deposit wallet to the main wallet
        let transfer_ix = spl_token::instruction::transfer_checked(
            &spl_token::id(),
            &temp_tok_pk,
            mint,
            &main_tok_pk,
            &keypair.pubkey(),
            &[],
            amount,
            decimals as u8,
        )?;
        instructions.push(transfer_ix);

        // Close the account and reap the rent if there's no more tokens on it.
        let (tok_balance, _) = get_account_token_balance(&rpc, &temp_tok_pk, mint)?;
        if tok_balance - amount == 0 {
            debug!(target: "SOL BRIDGE", "Adding account close instruction because resulting balance is 0");
            let close_ix = spl_token::instruction::close_account(
                &spl_token::id(),
                &temp_tok_pk,
                &self.keypair.pubkey(),
                &keypair.pubkey(),
                &[],
            )?;
            instructions.push(close_ix);
        }

        let tx = Transaction::new_with_payer(&instructions, Some(&self.keypair.pubkey()));
        let signature = sign_and_send_transaction(&rpc, tx, vec![&self.keypair, keypair])?;

        debug!(target: "SOL BRIDGE", "Sent tokens to main wallet: {}", signature);

        Ok(signature)
    }

    fn send_sol_to_main_wallet(
        self: Arc<Self>,
        rpc: &RpcClient,
        amount: u64,
        keypair: &Keypair,
    ) -> SolResult<Signature> {
        debug!(target: "SOL BRIDGE", "Sending {} SOL to main wallet", lamports_to_sol(amount));

        let ix = system_instruction::transfer(&keypair.pubkey(), &self.keypair.pubkey(), amount);
        let tx = Transaction::new_with_payer(&[ix], Some(&self.keypair.pubkey()));
        let signature = sign_and_send_transaction(&rpc, tx, vec![&self.keypair, keypair])?;

        debug!(target: "SOL BRIDGE", "Sent SOL to main wallet: {}", signature);
        Ok(signature)
    }
}

#[async_trait]
impl NetworkClient for SolClient {
    async fn subscribe(self: Arc<Self>) -> Result<TokenSubscribtion> {
        let keypair = Keypair::generate(&mut OsRng);

        let public_key = keypair.pubkey().to_string();
        let secret_key = serialize(&keypair);

        // TODO: Option<Pubkey> for 2nd arg representing Token Mint account
        let mint = Pubkey::from_str("F4wkXLN5n1ckejfnJoahGpgW3ffRsrvS9GGVME6ckxS9").unwrap();
        smol::spawn(self.handle_subscribe_request(keypair, Some(mint))).detach();
        //smol::spawn(self.handle_subscribe_request(keypair, None)).detach();

        Ok(TokenSubscribtion {
            secret_key,
            public_key,
        })
    }

    // in solana case private key it's the same as keypair
    async fn subscribe_with_keypair(
        self: Arc<Self>,
        private_key: Vec<u8>,
        _public_key: Vec<u8>,
    ) -> Result<String> {
        let keypair: Keypair = deserialize(&private_key)?;

        let public_key = keypair.pubkey().to_string();

        // TODO: Option<Pubkey> for 2nd arg representing Token Mint account
        let mint = Pubkey::from_str("F4wkXLN5n1ckejfnJoahGpgW3ffRsrvS9GGVME6ckxS9").unwrap();
        smol::spawn(self.handle_subscribe_request(keypair, Some(mint))).detach();
        //smol::spawn(self.handle_subscribe_request(keypair, None)).detach();

        Ok(public_key)
    }

    async fn get_notifier(self: Arc<Self>) -> Result<async_channel::Receiver<TokenNotification>> {
        Ok(self.notify_channel.1.clone())
    }

    async fn send(self: Arc<Self>, address: Vec<u8>, amount: u64) -> Result<()> {
        let rpc = RpcClient::new(self.rpc_server.to_string());
        let address: Pubkey = deserialize(&address)?;
        let instruction = system_instruction::transfer(&self.keypair.pubkey(), &address, amount);

        let mut tx = Transaction::new_with_payer(&[instruction], Some(&self.keypair.pubkey()));
        let bhq = BlockhashQuery::default();
        match bhq.get_blockhash_and_fee_calculator(&rpc, rpc.commitment()) {
            Err(_) => panic!("Couldn't connect to RPC"),
            Ok(v) => tx.sign(&[&self.keypair], v.0),
        }

        let _signature = rpc
            .send_and_confirm_transaction(&tx)
            .map_err(SolFailed::from)?;

        Ok(())
    }
}

/// Gets account token balance for given mint.
/// Returns: (amount, decimals)
pub fn get_account_token_balance(
    rpc: &RpcClient,
    address: &Pubkey,
    mint: &Pubkey,
) -> SolResult<(u64, u64)> {
    let mint_account = rpc.get_account(mint)?;
    let token_account = rpc.get_account(address)?;
    let mint_data = spl_token::state::Mint::unpack_from_slice(&mint_account.data)?;
    let token_data = spl_token::state::Account::unpack_from_slice(&token_account.data)?;

    Ok((token_data.amount, mint_data.decimals as u64))
}

/// Check if given account is a valid token mint
pub fn account_is_initialized_mint(rpc: &RpcClient, mint: &Pubkey) -> bool {
    rpc.get_token_supply(mint).is_ok()
}

pub fn sign_and_send_transaction(
    rpc: &RpcClient,
    mut tx: Transaction,
    signers: Vec<&Keypair>,
) -> SolResult<Signature> {
    let bhq = BlockhashQuery::default();
    match bhq.get_blockhash_and_fee_calculator(rpc, rpc.commitment()) {
        Err(_) => return Err(SolFailed::RpcError("Couldn't connect to RPC".into())),
        Ok(v) => tx.sign(&signers, v.0),
    }

    match rpc.send_and_confirm_transaction(&tx) {
        Ok(s) => Ok(s),
        Err(_) => Err(SolFailed::RpcError("Failed to send transaction".into())),
    }
}

impl Encodable for Keypair {
    fn encode<S: std::io::Write>(&self, s: S) -> Result<usize> {
        let key: Vec<u8> = self.to_bytes().to_vec();
        let len = key.encode(s)?;
        Ok(len)
    }
}

impl Decodable for Keypair {
    fn decode<D: std::io::Read>(mut d: D) -> Result<Self> {
        let key: Vec<u8> = Decodable::decode(&mut d)?;
        let key = Keypair::from_bytes(key.as_slice()).map_err(|_| {
            crate::Error::from(SolFailed::DecodeAndEncodeError(
                "load keypair from slice".into(),
            ))
        })?;
        Ok(key)
    }
}

impl Encodable for Pubkey {
    fn encode<S: std::io::Write>(&self, s: S) -> Result<usize> {
        let key = self.to_string();
        let len = key.encode(s)?;
        Ok(len)
    }
}

impl Decodable for Pubkey {
    fn decode<D: std::io::Read>(mut d: D) -> Result<Self> {
        let key: String = Decodable::decode(&mut d)?;
        let key = Pubkey::try_from(key.as_str()).map_err(|_| {
            crate::Error::from(SolFailed::DecodeAndEncodeError(
                "load public key from slice".into(),
            ))
        })?;
        Ok(key)
    }
}

#[derive(Debug)]
pub enum SolFailed {
    NotEnoughValue(u64),
    BadSolAddress(String),
    DecodeAndEncodeError(String),
    WebSocketError(String),
    RpcError(String),
    SolClientError(String),
    Notification(String),
    ProgramError(String),
    MintIsNotValid(String),
    JsonError(String),
    ParseError(String),
    SolError(String),
}

impl std::error::Error for SolFailed {}

impl std::fmt::Display for SolFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            SolFailed::NotEnoughValue(i) => {
                write!(f, "There is no enough value {}", i)
            }
            SolFailed::BadSolAddress(ref err) => {
                write!(f, "Bad Sol Address: {}", err)
            }
            SolFailed::DecodeAndEncodeError(ref err) => {
                write!(f, "Decode and decode keys error: {}", err)
            }
            SolFailed::WebSocketError(i) => {
                write!(f, "WebSocket Error: {}", i)
            }
            SolFailed::RpcError(i) => {
                write!(f, "Rpc Error: {}", i)
            }
            SolFailed::ParseError(i) => {
                write!(f, "Parse Error: {}", i)
            }
            SolFailed::SolClientError(i) => {
                write!(f, "Solana Client Error: {}", i)
            }
            SolFailed::Notification(i) => {
                write!(f, "Received Notification Error: {}", i)
            }
            SolFailed::ProgramError(i) => {
                write!(f, "ProgramError Error: {}", i)
            }
            SolFailed::MintIsNotValid(i) => {
                write!(f, "Given mint is not valid: {}", i)
            }
            SolFailed::JsonError(i) => {
                write!(f, "JsonError: {}", i)
            }
            SolFailed::SolError(i) => {
                write!(f, "SolFailed: {}", i)
            }
        }
    }
}

impl From<solana_sdk::pubkey::ParsePubkeyError> for SolFailed {
    fn from(err: solana_sdk::pubkey::ParsePubkeyError) -> SolFailed {
        SolFailed::ParseError(err.to_string())
    }
}

impl From<tungstenite::Error> for SolFailed {
    fn from(err: tungstenite::Error) -> SolFailed {
        SolFailed::WebSocketError(err.to_string())
    }
}

impl From<solana_client::client_error::ClientError> for SolFailed {
    fn from(err: solana_client::client_error::ClientError) -> SolFailed {
        SolFailed::SolError(err.to_string())
    }
}

impl From<solana_sdk::program_error::ProgramError> for SolFailed {
    fn from(err: solana_sdk::program_error::ProgramError) -> SolFailed {
        SolFailed::ProgramError(err.to_string())
    }
}

impl From<crate::error::Error> for SolFailed {
    fn from(err: crate::error::Error) -> SolFailed {
        SolFailed::SolError(err.to_string())
    }
}
impl From<serde_json::Error> for SolFailed {
    fn from(err: serde_json::Error) -> SolFailed {
        SolFailed::JsonError(err.to_string())
    }
}

pub type SolResult<T> = std::result::Result<T, SolFailed>;
