use std::collections::{HashMap, HashSet};

use super::super::FundingError;
use crate::ckb::serde_utils::EntityHex;

use anyhow::anyhow;
use ckb_sdk::{
    constants::SIGHASH_TYPE_HASH,
    traits::{
        CellCollector, CellDepResolver, CellQueryOptions, DefaultCellCollector,
        DefaultCellDepResolver, DefaultHeaderDepResolver, DefaultTransactionDependencyProvider,
        HeaderDepResolver, SecpCkbRawKeySigner, TransactionDependencyProvider, ValueRangeOption,
    },
    tx_builder::{unlock_tx, CapacityBalancer, TxBuilder, TxBuilderError},
    unlock::{ScriptUnlocker, SecpSighashUnlocker},
    CkbRpcClient, ScriptId,
};
use ckb_types::{
    core::{BlockView, Capacity, DepType, TransactionView},
    h256,
    packed::{self, Bytes, CellDep, CellInput, OutPoint, Script, Transaction},
    prelude::*,
};
use log::warn;
use molecule::{
    bytes::{BufMut as _, BytesMut},
    prelude::*,
};
use serde::Deserialize;
use serde_with::serde_as;

/// Funding transaction wrapper.
///
/// It includes extra fields to verify the transaction.
#[derive(Clone, Debug, Default)]
pub struct FundingTx {
    tx: Option<TransactionView>,
}

impl From<TransactionView> for FundingTx {
    fn from(tx: TransactionView) -> Self {
        Self { tx: Some(tx) }
    }
}

impl From<Transaction> for FundingTx {
    fn from(tx: Transaction) -> Self {
        Self {
            tx: Some(tx.into_view()),
        }
    }
}

#[allow(dead_code)]
#[serde_as]
#[derive(Clone, Debug, Default, Deserialize)]
pub struct FundingUdtInfo {
    /// The UDT type script
    #[serde_as(as = "EntityHex")]
    pub type_script: packed::Script,
    /// CKB amount to be provided by the local party.
    pub local_ckb_amount: u64,
    /// CKB amount to be provided by the remote party.
    pub remote_ckb_amount: u64,
}

#[serde_as]
#[derive(Clone, Debug, Default, Deserialize)]
pub struct FundingRequest {
    /// UDT channel info
    pub udt_info: Option<FundingUdtInfo>,
    /// The funding cell lock script args
    #[serde_as(as = "EntityHex")]
    pub script: Script,
    /// Assets amount to be provided by the local party
    pub local_amount: u64,
    /// Fee to be provided by the local party
    pub local_fee_rate: u64,
    /// Assets amount to be provided by the remote party
    pub remote_amount: u64,
}

// TODO: trace locked cells
#[derive(Clone, Debug)]
pub struct FundingContext {
    pub secret_key: secp256k1::SecretKey,
    pub rpc_url: String,
    pub funding_source_lock_script: packed::Script,
    pub funding_cell_lock_script: packed::Script,
}

#[allow(dead_code)]
struct FundingTxBuilder {
    funding_tx: FundingTx,
    request: FundingRequest,
    context: FundingContext,
}

impl TxBuilder for FundingTxBuilder {
    fn build_base(
        &self,
        cell_collector: &mut dyn CellCollector,
        cell_dep_resolver: &dyn CellDepResolver,
        _header_dep_resolver: &dyn HeaderDepResolver,
        _tx_dep_provider: &dyn TransactionDependencyProvider,
    ) -> Result<TransactionView, TxBuilderError> {
        let (funding_cell_output, funding_cell_output_data) = self
            .build_funding_cell()
            .map_err(|err| TxBuilderError::Other(err.into()))?;

        // Funding cell does not need new cell deps and header deps. The type script deps will be added with inputs.
        let mut outputs: Vec<packed::CellOutput> = vec![funding_cell_output];
        let mut outputs_data: Vec<packed::Bytes> = vec![funding_cell_output_data];

        // Try to find a proper UDT input cell.
        let mut inputs = vec![];
        let mut cell_deps = HashSet::new();
        if let Some(ref udt_info) = self.request.udt_info {
            let udt_type_script = udt_info.type_script.clone();
            let owner = self.context.funding_source_lock_script.clone();
            warn!("anan owner now: {:?}", owner);
            let owner_query = {
                let mut query = CellQueryOptions::new_lock(owner.clone());
                //query.secondary_script = Some(udt_type_script.clone());
                query.data_len_range = Some(ValueRangeOption::new_min(16));
                query.min_total_capacity = u64::MAX;
                query
            };

            let local_ckb_amount = udt_info.local_ckb_amount;
            let udt_amount = self.request.local_amount as u128;

            let (owner_cells, _) = cell_collector.collect_live_cells(&owner_query, true)?;
            warn!("anan owner_cells: {:?}", owner_cells.len());
            for cell in owner_cells.iter() {
                let cell_capacity: u64 = cell.output.capacity().unpack();
                let mut amount_bytes = [0u8; 16];
                amount_bytes.copy_from_slice(&cell.output_data.as_ref()[0..16]);
                let cell_udt_amount = u128::from_le_bytes(amount_bytes);
                //FIXME(yukang): we may need to revise the check here
                warn!("anan cell_capacity: {}, local_ckb_amount: {}, cell_udt_amount: {}, udt_amount: {}", cell_capacity, local_ckb_amount, cell_udt_amount, udt_amount);
                if cell_capacity >= local_ckb_amount && cell_udt_amount >= udt_amount {
                    inputs.push(CellInput::new(cell.out_point.clone(), 0));
                    if cell_udt_amount > udt_amount {
                        let change_output = packed::CellOutput::new_builder()
                            .capacity(Capacity::shannons(cell_capacity - local_ckb_amount).pack())
                            .lock(owner.clone())
                            .build();
                        let change_output_data: Bytes =
                            (cell_udt_amount - udt_amount).to_le_bytes().pack();

                        outputs.push(change_output);
                        outputs_data.push(change_output_data);
                    }
                    warn!("anan find proper UDT owner cell: {:?}", cell);
                    break;
                }
            }
            if inputs.is_empty() {
                return Err(TxBuilderError::Other(anyhow!(
                    "proper UDT owner cell not found"
                )));
            }
            let owner_cell_dep = cell_dep_resolver
                .resolve(&owner)
                .ok_or_else(|| TxBuilderError::ResolveCellDepFailed(owner.clone()))?;
            warn!("anan begin to resolve udt cell dep: {:?}", udt_type_script);
            let udt_cell_dep = cell_dep_resolver
                .resolve(&udt_type_script)
                .ok_or_else(|| TxBuilderError::ResolveCellDepFailed(udt_type_script.clone()))?;
            warn!("anan udt_cell_dep: {:?}", udt_cell_dep);
            #[allow(clippy::mutable_key_type)]
            cell_deps.insert(owner_cell_dep);
            cell_deps.insert(udt_cell_dep);
        }

        let mut tx_inputs: Vec<packed::CellInput> =
            inputs.into_iter().map(|input| input.into()).collect();

        if let Some(ref tx) = self.funding_tx.tx {
            for (i, output) in tx.outputs().into_iter().enumerate().skip(1) {
                outputs.push(output.clone());
                outputs_data.push(tx.outputs_data().get(i).unwrap_or_default().clone());
            }
            for input in tx.inputs().into_iter() {
                tx_inputs.push(input.clone());
            }
            for cell_dep in tx.cell_deps().into_iter() {
                cell_deps.insert(cell_dep);
            }
        }

        let builder = match self.funding_tx.tx {
            Some(ref tx) => tx.as_advanced_builder(),
            None => packed::Transaction::default().as_advanced_builder(),
        };

        warn!("anan now here : {:?}", cell_deps);
        let tx_builder = builder
            .set_inputs(tx_inputs)
            .set_outputs(outputs)
            .set_outputs_data(outputs_data)
            .set_cell_deps(cell_deps.into_iter().collect());
        warn!("anan tx_builder: {:?}", tx_builder);
        let tx = tx_builder.build();
        warn!("anan tx_builder finished: {:?}", tx);
        Ok(tx)
    }
}

impl FundingTxBuilder {
    fn build_funding_cell(&self) -> Result<(packed::CellOutput, packed::Bytes), FundingError> {
        // If outputs is not empty, assume that the remote party has already funded.
        let remote_funded = self
            .funding_tx
            .tx
            .as_ref()
            .map(|tx| !tx.outputs().is_empty())
            .unwrap_or(false);

        match self.request.udt_info {
            Some(ref udt_info) => {
                let mut udt_amount = self.request.local_amount as u128;
                let mut ckb_amount = udt_info.local_ckb_amount;

                // To make tx building easier, do not include the amount not funded yet in the
                // funding cell.
                if remote_funded {
                    udt_amount += self.request.remote_amount as u128;
                    ckb_amount = ckb_amount
                        .checked_add(udt_info.remote_ckb_amount)
                        .ok_or(FundingError::InvalidChannel)?;
                }

                let udt_output = packed::CellOutput::new_builder()
                    .capacity(Capacity::shannons(ckb_amount).pack())
                    .type_(Some(udt_info.type_script.clone()).pack())
                    .lock(self.context.funding_cell_lock_script.clone())
                    .build();
                let mut data = BytesMut::with_capacity(16);
                data.put(&udt_amount.to_le_bytes()[..]);

                // TODO: xudt extension
                Ok((udt_output, data.freeze().pack()))
            }
            None => {
                let mut ckb_amount = self.request.local_amount;
                if remote_funded {
                    ckb_amount = ckb_amount
                        .checked_add(self.request.remote_amount)
                        .ok_or(FundingError::InvalidChannel)?;
                }
                let ckb_output = packed::CellOutput::new_builder()
                    .capacity(Capacity::shannons(ckb_amount).pack())
                    .lock(self.context.funding_cell_lock_script.clone())
                    .build();
                warn!("build_funding_cell debug ckb_output: {:?}", ckb_output);
                Ok((ckb_output, packed::Bytes::default()))
            }
        }
    }

    fn build(self) -> Result<FundingTx, FundingError> {
        // Build ScriptUnlocker
        let signer = SecpCkbRawKeySigner::new_with_secret_keys(vec![]);
        let sighash_unlocker = SecpSighashUnlocker::from(Box::new(signer) as Box<_>);
        let sighash_script_id = ScriptId::new_type(SIGHASH_TYPE_HASH.clone());
        let mut unlockers = HashMap::default();
        unlockers.insert(
            sighash_script_id,
            Box::new(sighash_unlocker) as Box<dyn ScriptUnlocker>,
        );

        let sender = self.context.funding_source_lock_script.clone();
        // Build CapacityBalancer
        let placeholder_witness = packed::WitnessArgs::new_builder()
            .lock(Some(molecule::bytes::Bytes::from(vec![0u8; 65])).pack())
            .build();
        let balancer =
            CapacityBalancer::new_simple(sender, placeholder_witness, self.request.local_fee_rate);

        let ckb_client = CkbRpcClient::new(&self.context.rpc_url);
        let mut cell_dep_resolver = {
            let genesis_block = ckb_client.get_block_by_number(0.into()).unwrap().unwrap();
            DefaultCellDepResolver::from_genesis(&BlockView::from(genesis_block)).unwrap()
        };

        if let Some(ref udt_info) = self.request.udt_info {
            // FIXME(yukang): how to add cell deps for udt?
            let udt_type_script = udt_info.type_script.clone();
            let tx_hash =
                h256!("0x371c4d9727fa47c0d77d04bdbb9951a7c63860f50c26108372cd28a336a31058");
            let out_point = OutPoint::new(tx_hash.pack(), 0);
            let cell_dep = CellDep::new_builder()
                .out_point(out_point)
                .dep_type(DepType::Code.into())
                .build();
            warn!(
                "anan adding cell_dep: code_hash {:?} => {:?}",
                ScriptId::from(&udt_type_script),
                cell_dep
            );
            cell_dep_resolver.insert(
                ScriptId::from(&udt_type_script),
                cell_dep,
                "Simple UDT".to_string(),
            );
        }

        let header_dep_resolver = DefaultHeaderDepResolver::new(&self.context.rpc_url);
        let mut cell_collector = DefaultCellCollector::new(&self.context.rpc_url);
        let tx_dep_provider = DefaultTransactionDependencyProvider::new(&self.context.rpc_url, 10);

        let (tx, _) = self.build_unlocked(
            &mut cell_collector,
            &cell_dep_resolver,
            &header_dep_resolver,
            &tx_dep_provider,
            &balancer,
            &unlockers,
        )?;

        let mut funding_tx = self.funding_tx;
        funding_tx.update_for_self(tx)?;
        Ok(funding_tx)
    }
}

impl FundingTx {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take(&mut self) -> Option<TransactionView> {
        self.tx.take()
    }

    pub fn as_ref(&self) -> Option<&TransactionView> {
        self.tx.as_ref()
    }

    pub fn into_inner(self) -> Option<TransactionView> {
        self.tx
    }

    pub fn fulfill(
        self,
        request: FundingRequest,
        context: FundingContext,
    ) -> Result<Self, FundingError> {
        let builder = FundingTxBuilder {
            funding_tx: self,
            request,
            context,
        };
        builder.build()
    }

    pub fn sign(
        mut self,
        secret_key: secp256k1::SecretKey,
        rpc_url: String,
    ) -> Result<Self, FundingError> {
        // Convert between different versions of secp256k1.
        // This app requires 0.28 because of:
        // ```
        // #[derive(Copy, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
        // pub struct Signature(pub Secp256k1Signature);
        // ```
        //
        // However, ckb-sdk-rust still uses 0.24.
        let signer = SecpCkbRawKeySigner::new_with_secret_keys(vec![std::str::FromStr::from_str(
            hex::encode(secret_key.as_ref()).as_ref(),
        )
        .unwrap()]);
        let sighash_unlocker = SecpSighashUnlocker::from(Box::new(signer) as Box<_>);
        let sighash_script_id = ScriptId::new_type(SIGHASH_TYPE_HASH.clone());
        let mut unlockers = HashMap::default();
        unlockers.insert(
            sighash_script_id,
            Box::new(sighash_unlocker) as Box<dyn ScriptUnlocker>,
        );
        let tx = self.take().ok_or(FundingError::AbsentTx)?;
        let tx_dep_provider = DefaultTransactionDependencyProvider::new(&rpc_url, 10);

        let (tx, _) = unlock_tx(tx.clone(), &tx_dep_provider, &unlockers)?;
        self.update_for_self(tx)?;
        Ok(self)
    }

    // TODO: verify the transaction
    pub fn update_for_self(&mut self, tx: TransactionView) -> Result<(), FundingError> {
        self.tx = Some(tx);
        Ok(())
    }

    // TODO: verify the transaction
    pub fn update_for_peer(&mut self, tx: TransactionView) -> Result<(), FundingError> {
        self.tx = Some(tx);
        Ok(())
    }
}
