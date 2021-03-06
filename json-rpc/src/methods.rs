// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! Module contains RPC method handlers for Full Node JSON-RPC interface
use crate::{
    errors::{ErrorData, InvalidArguments, JsonRpcError},
    views::{
        AccountStateWithProofView, AccountView, BlockMetadata, CurrencyInfoView, EventView,
        StateProofView, TransactionView,
    },
};
use anyhow::{ensure, format_err, Error, Result};
use core::future::Future;
use futures::{channel::oneshot, SinkExt};
use libra_config::config::RoleType;
use libra_crypto::hash::CryptoHash;
use libra_mempool::MempoolClientSender;
use libra_trace::prelude::*;
use libra_types::{
    account_address::AccountAddress,
    account_config::{from_currency_code_string, CurrencyInfoResource},
    account_state::AccountState,
    chain_id::ChainId,
    event::EventKey,
    ledger_info::LedgerInfoWithSignatures,
    mempool_status::MempoolStatusCode,
    move_resource::MoveStorage,
    on_chain_config::{OnChainConfig, RegisteredCurrencies},
    transaction::SignedTransaction,
};
use network::counters;
use serde_json::Value;
use std::{collections::HashMap, convert::TryFrom, ops::Deref, pin::Pin, str::FromStr, sync::Arc};
use storage_interface::DbReader;

#[derive(Clone)]
pub(crate) struct JsonRpcService {
    db: Arc<dyn DbReader>,
    mempool_sender: MempoolClientSender,
    role: RoleType,
    chain_id: ChainId,
}

impl JsonRpcService {
    pub fn new(
        db: Arc<dyn DbReader>,
        mempool_sender: MempoolClientSender,
        role: RoleType,
        chain_id: ChainId,
    ) -> Self {
        Self {
            db,
            mempool_sender,
            role,
            chain_id,
        }
    }

    pub fn get_latest_ledger_info(&self) -> Result<LedgerInfoWithSignatures> {
        self.db.get_latest_ledger_info()
    }

    pub fn chain_id(&self) -> ChainId {
        self.chain_id
    }
}

type RpcHandler =
    Box<fn(JsonRpcService, JsonRpcRequest) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>>>;

pub(crate) type RpcRegistry = HashMap<String, RpcHandler>;

pub(crate) struct JsonRpcRequest {
    pub params: Vec<Value>,
    pub ledger_info: LedgerInfoWithSignatures,
}

impl JsonRpcRequest {
    /// Returns the request parameter at the given index.
    /// Returns Null if given index is out of bounds.
    fn get_param(&self, index: usize) -> Value {
        self.get_param_with_default(index, Value::Null)
    }

    /// Returns the request parameter at the given index.
    /// Returns default Value if given index is out of bounds.
    fn get_param_with_default(&self, index: usize, default: Value) -> Value {
        if self.params.len() > index {
            return self.params[index].clone();
        }
        default
    }

    fn version(&self) -> u64 {
        self.ledger_info.ledger_info().version()
    }
}

/// Submits transaction to full node
async fn submit(mut service: JsonRpcService, request: JsonRpcRequest) -> Result<()> {
    let txn_payload: String = serde_json::from_value(request.get_param(0))?;
    let transaction: SignedTransaction = lcs::from_bytes(&hex::decode(txn_payload)?)?;
    trace_code_block!("json-rpc::submit", {"txn", transaction.sender(), transaction.sequence_number()});

    let (req_sender, callback) = oneshot::channel();
    service
        .mempool_sender
        .send((transaction, req_sender))
        .await?;
    let (mempool_status, vm_status_opt) = callback.await??;

    if let Some(vm_status) = vm_status_opt {
        Err(Error::new(JsonRpcError::vm_status(vm_status)))
    } else if mempool_status.code == MempoolStatusCode::Accepted {
        Ok(())
    } else {
        Err(Error::new(JsonRpcError::mempool_error(mempool_status)?))
    }
}

/// Returns account state (AccountView) by given address
async fn get_account(
    service: JsonRpcService,
    request: JsonRpcRequest,
) -> Result<Option<AccountView>> {
    let address: String = serde_json::from_value(request.get_param(0))?;
    let account_address = AccountAddress::from_str(&address)?;
    let response = service
        .db
        .get_account_state_with_proof_by_version(account_address, request.version())?
        .0;
    let currency_info = currencies_info(service, request).await?;
    let currencies: Vec<_> = currency_info
        .into_iter()
        .map(|info| from_currency_code_string(&info.code))
        .collect::<Result<_, _>>()?;
    if let Some(blob) = response {
        let account_state = AccountState::try_from(&blob)?;
        if let Some(account) = account_state.get_account_resource()? {
            let balances = account_state.get_balance_resources(&currencies)?;
            if let Some(account_role) = account_state.get_account_role(&currencies)? {
                if let Some(freezing_bit) = account_state.get_freezing_bit()? {
                    return Ok(Some(AccountView::new(
                        &account,
                        balances,
                        account_role,
                        freezing_bit,
                    )));
                }
            }
        }
    }
    Ok(None)
}

/// Returns the blockchain metadata for a specified version. If no version is specified, default to
/// returning the current blockchain metadata
/// Can be used to verify that target Full Node is up-to-date
async fn get_metadata(service: JsonRpcService, request: JsonRpcRequest) -> Result<BlockMetadata> {
    match serde_json::from_value::<u64>(request.get_param(0)) {
        Ok(version) => Ok(BlockMetadata {
            version,
            timestamp: service.db.get_block_timestamp(version)?,
        }),
        _ => Ok(BlockMetadata {
            version: request.version(),
            timestamp: request.ledger_info.ledger_info().timestamp_usecs(),
        }),
    }
}

/// Returns transactions by range
async fn get_transactions(
    service: JsonRpcService,
    request: JsonRpcRequest,
) -> Result<Vec<TransactionView>> {
    let start_version: u64 = serde_json::from_value(request.get_param(0))?;
    let limit: u64 = serde_json::from_value(request.get_param(1))?;
    let include_events: bool = serde_json::from_value(request.get_param(2))?;

    ensure!(
        limit > 0 && limit <= 1000,
        "limit must be smaller than 1000"
    );

    let txs =
        service
            .db
            .get_transactions(start_version, limit, request.version(), include_events)?;

    let mut result = vec![];

    let all_events = if include_events {
        txs.events
            .ok_or_else(|| format_err!("Storage layer didn't return events when requested!"))?
    } else {
        vec![]
    };

    let txs_with_info = txs
        .transactions
        .into_iter()
        .zip(txs.proof.transaction_infos().iter());

    for (v, (tx, info)) in txs_with_info.enumerate() {
        let events = if include_events {
            all_events
                .get(v)
                .ok_or_else(|| format_err!("Missing events for version: {}", v))?
                .iter()
                .cloned()
                .map(|x| (start_version + v as u64, x).into())
                .collect()
        } else {
            vec![]
        };

        result.push(TransactionView {
            version: start_version + v as u64,
            hash: tx.hash().to_hex(),
            transaction: tx.into(),
            events,
            vm_status: info.status().into(),
            gas_used: info.gas_used(),
        });
    }
    Ok(result)
}

/// Returns account transaction by account and sequence_number
async fn get_account_transaction(
    service: JsonRpcService,
    request: JsonRpcRequest,
) -> Result<Option<TransactionView>> {
    let p_account: String = serde_json::from_value(request.get_param(0))?;
    let sequence: u64 = serde_json::from_value(request.get_param(1))?;
    let include_events: bool = serde_json::from_value(request.get_param(2))?;

    let account = AccountAddress::try_from(p_account)?;

    let tx = service
        .db
        .get_txn_by_account(account, sequence, request.version(), include_events)?;

    if let Some(tx) = tx {
        if include_events {
            ensure!(
                tx.events.is_some(),
                "Storage layer didn't return events when requested!"
            );
        }
        let tx_version = tx.version;

        let events = tx
            .events
            .unwrap_or_default()
            .into_iter()
            .map(|x| ((tx_version, x).into()))
            .collect();

        Ok(Some(TransactionView {
            version: tx_version,
            hash: tx.transaction.hash().to_hex(),
            transaction: tx.transaction.into(),
            events,
            vm_status: tx.proof.transaction_info().status().into(),
            gas_used: tx.proof.transaction_info().gas_used(),
        }))
    } else {
        Ok(None)
    }
}

/// Returns events by given access path
async fn get_events(service: JsonRpcService, request: JsonRpcRequest) -> Result<Vec<EventView>> {
    let raw_event_key: String = serde_json::from_value(request.get_param(0))?;
    let start: u64 = serde_json::from_value(request.get_param(1))?;
    let limit: u64 = serde_json::from_value(request.get_param(2))?;

    let event_key = EventKey::try_from(&hex::decode(raw_event_key)?[..])?;
    let events_with_proof = service.db.get_events(&event_key, start, true, limit)?;

    let req_version = request.version();
    let events = events_with_proof
        .into_iter()
        .filter(|(version, _event)| version <= &req_version)
        .map(|event| event.into())
        .collect();
    Ok(events)
}

/// Returns meta information about supported currencies
async fn currencies_info(
    service: JsonRpcService,
    request: JsonRpcRequest,
) -> Result<Vec<CurrencyInfoView>> {
    let raw_data = service.db.deref().batch_fetch_resources_by_version(
        vec![RegisteredCurrencies::CONFIG_ID.access_path()],
        request.version(),
    )?;
    ensure!(raw_data.len() == 1, "invalid storage result");
    let currencies = RegisteredCurrencies::from_bytes(&raw_data[0])?;
    let access_paths: Vec<_> = currencies
        .currency_codes()
        .iter()
        .map(|code| CurrencyInfoResource::resource_path_for(code.clone()))
        .collect();

    let mut currencies = vec![];
    for raw_data in service
        .db
        .deref()
        .batch_fetch_resources_by_version(access_paths, request.version())?
    {
        let currency_info = CurrencyInfoResource::try_from_bytes(&raw_data)?;
        currencies.push(CurrencyInfoView::from(currency_info));
    }
    Ok(currencies)
}

/// Returns proof of new state relative to version known to client
async fn get_state_proof(
    service: JsonRpcService,
    request: JsonRpcRequest,
) -> Result<StateProofView> {
    let known_version: u64 = serde_json::from_value(request.get_param(0))?;
    let proofs = service
        .db
        .get_state_proof_with_ledger_info(known_version, request.ledger_info.clone())?;
    StateProofView::try_from((request.ledger_info, proofs.0, proofs.1))
}

/// Returns the account state to the client, alongside a proof relative to the version and
/// ledger_version specified by the client. If version or ledger_version are not specified,
/// the latest known versions will be used.
async fn get_account_state_with_proof(
    service: JsonRpcService,
    request: JsonRpcRequest,
) -> Result<AccountStateWithProofView> {
    let address: String = serde_json::from_value(request.get_param(0))?;
    let account_address = AccountAddress::from_str(&address)?;

    // If versions are specified by the request parameters, use them, otherwise use the defaults
    let version =
        serde_json::from_value::<u64>(request.get_param(1)).unwrap_or_else(|_| request.version());
    let ledger_version =
        serde_json::from_value::<u64>(request.get_param(2)).unwrap_or_else(|_| request.version());

    let account_state_with_proof =
        service
            .db
            .get_account_state_with_proof(account_address, version, ledger_version)?;
    Ok(AccountStateWithProofView::try_from(
        account_state_with_proof,
    )?)
}

/// Returns the number of peers this node is connected to
async fn get_network_status(service: JsonRpcService, _request: JsonRpcRequest) -> Result<u64> {
    let blah = counters::LIBRA_NETWORK_PEERS
        .get_metric_with_label_values(&[service.role.as_str(), "connected"])?;
    Ok(blah.get() as u64)
}

/// Builds registry of all available RPC methods
/// To register new RPC method, add it via `register_rpc_method!` macros call
/// Note that RPC method name will equal to name of function
#[allow(unused_comparisons)]
pub(crate) fn build_registry() -> RpcRegistry {
    let mut registry = RpcRegistry::new();
    register_rpc_method!(registry, "submit", submit, 1, 0);
    register_rpc_method!(registry, "get_metadata", get_metadata, 0, 1);
    register_rpc_method!(registry, "get_account", get_account, 1, 0);
    register_rpc_method!(registry, "get_transactions", get_transactions, 3, 0);
    register_rpc_method!(
        registry,
        "get_account_transaction",
        get_account_transaction,
        3,
        0
    );
    register_rpc_method!(registry, "get_events", get_events, 3, 0);
    register_rpc_method!(registry, "get_currencies", currencies_info, 0, 0);

    register_rpc_method!(registry, "get_state_proof", get_state_proof, 1, 0);
    register_rpc_method!(
        registry,
        "get_account_state_with_proof",
        get_account_state_with_proof,
        3,
        0
    );
    register_rpc_method!(registry, "get_network_status", get_network_status, 0, 0);

    registry
}
