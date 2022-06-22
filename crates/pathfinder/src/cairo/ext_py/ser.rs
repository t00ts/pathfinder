//! The json serializable types

use crate::core::{CallParam, ContractAddress, EntryPoint};
use crate::rpc::types::BlockHashOrTag;

/// The command we send to the python loop.
#[serde_with::serde_as]
#[derive(serde::Serialize, Debug)]
pub(crate) struct ChildCommand<'a> {
    pub command: Verb,
    pub contract_address: &'a ContractAddress,
    pub calldata: &'a [CallParam],
    pub entry_point_selector: &'a EntryPoint,
    pub at_block: &'a BlockHashOrTag,
    #[serde_as(as = "Option<&crate::rpc::serde::H256AsHexStr>")]
    pub gas_price: Option<&'a web3::types::H256>,
    pub caller_address: Option<&'a crate::core::ContractAddress>,
    pub signature: &'a [crate::core::CallSignatureElem],
    pub max_fee: &'a crate::core::Fee,
    #[serde_as(as = "crate::rpc::serde::TransactionVersionAsHexStr")]
    pub version: &'a crate::core::TransactionVersion,
}

#[derive(serde::Serialize, Debug)]
pub(crate) enum Verb {
    #[serde(rename = "call")]
    Call,
    #[serde(rename = "estimate_fee")]
    EstimateFee,
}
