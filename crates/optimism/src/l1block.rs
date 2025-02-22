use crate::{transaction::estimate_tx_compressed_size, OpSpecId};
use auto_impl::auto_impl;
use core::ops::Mul;
use inspector::inspector_context::InspectorContext;
use revm::{
    context_interface::{DatabaseGetter, Journal},
    database_interface::Database,
    primitives::{address, Address, U256},
    specification::hardfork::SpecId,
    Context,
};

use super::OpSpec;

pub const ZERO_BYTE_COST: u64 = 4;
pub const NON_ZERO_BYTE_COST: u64 = 16;

/// The two 4-byte Ecotone fee scalar values are packed into the same storage slot as the 8-byte sequence number.
/// Byte offset within the storage slot of the 4-byte baseFeeScalar attribute.
pub const BASE_FEE_SCALAR_OFFSET: usize = 16;
/// The two 4-byte Ecotone fee scalar values are packed into the same storage slot as the 8-byte sequence number.
/// Byte offset within the storage slot of the 4-byte blobBaseFeeScalar attribute.
pub const BLOB_BASE_FEE_SCALAR_OFFSET: usize = 20;

pub const L1_BASE_FEE_SLOT: U256 = U256::from_limbs([1u64, 0, 0, 0]);
pub const L1_OVERHEAD_SLOT: U256 = U256::from_limbs([5u64, 0, 0, 0]);
pub const L1_SCALAR_SLOT: U256 = U256::from_limbs([6u64, 0, 0, 0]);

/// [ECOTONE_L1_BLOB_BASE_FEE_SLOT] was added in the Ecotone upgrade and stores the L1 blobBaseFee attribute.
pub const ECOTONE_L1_BLOB_BASE_FEE_SLOT: U256 = U256::from_limbs([7u64, 0, 0, 0]);

/// As of the ecotone upgrade, this storage slot stores the 32-bit basefeeScalar and blobBaseFeeScalar attributes at
/// offsets [BASE_FEE_SCALAR_OFFSET] and [BLOB_BASE_FEE_SCALAR_OFFSET] respectively.
pub const ECOTONE_L1_FEE_SCALARS_SLOT: U256 = U256::from_limbs([3u64, 0, 0, 0]);

/// An empty 64-bit set of scalar values.
const EMPTY_SCALARS: [u8; 8] = [0u8; 8];

/// The address of L1 fee recipient.
pub const L1_FEE_RECIPIENT: Address = address!("420000000000000000000000000000000000001A");

/// The address of the base fee recipient.
pub const BASE_FEE_RECIPIENT: Address = address!("4200000000000000000000000000000000000019");

/// The address of the L1Block contract.
pub const L1_BLOCK_CONTRACT: Address = address!("4200000000000000000000000000000000000015");

/// L1 block info
///
/// We can extract L1 epoch data from each L2 block, by looking at the `setL1BlockValues`
/// transaction data. This data is then used to calculate the L1 cost of a transaction.
///
/// Here is the format of the `setL1BlockValues` transaction data:
///
/// setL1BlockValues(uint64 _number, uint64 _timestamp, uint256 _basefee, bytes32 _hash,
/// uint64 _sequenceNumber, bytes32 _batcherHash, uint256 _l1FeeOverhead, uint256 _l1FeeScalar)
///
/// For now, we only care about the fields necessary for L1 cost calculation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct L1BlockInfo {
    /// The base fee of the L1 origin block.
    pub l1_base_fee: U256,
    /// The current L1 fee overhead. None if Ecotone is activated.
    pub l1_fee_overhead: Option<U256>,
    /// The current L1 fee scalar.
    pub l1_base_fee_scalar: U256,
    /// The current L1 blob base fee. None if Ecotone is not activated, except if `empty_scalars` is `true`.
    pub l1_blob_base_fee: Option<U256>,
    /// The current L1 blob base fee scalar. None if Ecotone is not activated.
    pub l1_blob_base_fee_scalar: Option<U256>,
    /// True if Ecotone is activated, but the L1 fee scalars have not yet been set.
    pub(crate) empty_scalars: bool,
}

impl L1BlockInfo {
    /// Try to fetch the L1 block info from the database.
    pub fn try_fetch<DB: Database>(db: &mut DB, spec_id: OpSpec) -> Result<L1BlockInfo, DB::Error> {
        // Ensure the L1 Block account is loaded into the cache after Ecotone. With EIP-4788, it is no longer the case
        // that the L1 block account is loaded into the cache prior to the first inquiry for the L1 block info.
        if spec_id.is_enabled_in(SpecId::CANCUN) {
            let _ = db.basic(L1_BLOCK_CONTRACT)?;
        }

        let l1_base_fee = db.storage(L1_BLOCK_CONTRACT, L1_BASE_FEE_SLOT)?;

        if !spec_id.is_enabled_in(OpSpecId::ECOTONE) {
            let l1_fee_overhead = db.storage(L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT)?;
            let l1_fee_scalar = db.storage(L1_BLOCK_CONTRACT, L1_SCALAR_SLOT)?;

            Ok(L1BlockInfo {
                l1_base_fee,
                l1_fee_overhead: Some(l1_fee_overhead),
                l1_base_fee_scalar: l1_fee_scalar,
                ..Default::default()
            })
        } else {
            let l1_blob_base_fee = db.storage(L1_BLOCK_CONTRACT, ECOTONE_L1_BLOB_BASE_FEE_SLOT)?;
            let l1_fee_scalars = db
                .storage(L1_BLOCK_CONTRACT, ECOTONE_L1_FEE_SCALARS_SLOT)?
                .to_be_bytes::<32>();

            let l1_base_fee_scalar = U256::from_be_slice(
                l1_fee_scalars[BASE_FEE_SCALAR_OFFSET..BASE_FEE_SCALAR_OFFSET + 4].as_ref(),
            );
            let l1_blob_base_fee_scalar = U256::from_be_slice(
                l1_fee_scalars[BLOB_BASE_FEE_SCALAR_OFFSET..BLOB_BASE_FEE_SCALAR_OFFSET + 4]
                    .as_ref(),
            );

            // Check if the L1 fee scalars are empty. If so, we use the Bedrock cost function.
            // The L1 fee overhead is only necessary if `empty_scalars` is true, as it was deprecated in Ecotone.
            let empty_scalars = l1_blob_base_fee.is_zero()
                && l1_fee_scalars[BASE_FEE_SCALAR_OFFSET..BLOB_BASE_FEE_SCALAR_OFFSET + 4]
                    == EMPTY_SCALARS;
            let l1_fee_overhead = empty_scalars
                .then(|| db.storage(L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT))
                .transpose()?;

            Ok(L1BlockInfo {
                l1_base_fee,
                l1_base_fee_scalar,
                l1_blob_base_fee: Some(l1_blob_base_fee),
                l1_blob_base_fee_scalar: Some(l1_blob_base_fee_scalar),
                empty_scalars,
                l1_fee_overhead,
            })
        }
    }

    /// Calculate the data gas for posting the transaction on L1. Calldata costs 16 gas per byte
    /// after compression.
    ///
    /// Prior to fjord, calldata costs 16 gas per non-zero byte and 4 gas per zero byte.
    ///
    /// Prior to regolith, an extra 68 non-zero bytes were included in the rollup data costs to
    /// account for the empty signature.
    pub fn data_gas(&self, input: &[u8], spec_id: OpSpec) -> U256 {
        if spec_id.is_enabled_in(OpSpecId::FJORD) {
            let estimated_size = self.tx_estimated_size_fjord(input);

            return estimated_size
                .saturating_mul(U256::from(NON_ZERO_BYTE_COST))
                .wrapping_div(U256::from(1_000_000));
        };

        let mut rollup_data_gas_cost = U256::from(input.iter().fold(0, |acc, byte| {
            acc + if *byte == 0x00 {
                ZERO_BYTE_COST
            } else {
                NON_ZERO_BYTE_COST
            }
        }));

        // Prior to regolith, an extra 68 non zero bytes were included in the rollup data costs.
        if !spec_id.is_enabled_in(OpSpecId::REGOLITH) {
            rollup_data_gas_cost += U256::from(NON_ZERO_BYTE_COST).mul(U256::from(68));
        }

        rollup_data_gas_cost
    }

    // Calculate the estimated compressed transaction size in bytes, scaled by 1e6.
    // This value is computed based on the following formula:
    // max(minTransactionSize, intercept + fastlzCoef*fastlzSize)
    fn tx_estimated_size_fjord(&self, input: &[u8]) -> U256 {
        U256::from(estimate_tx_compressed_size(input))
    }

    /// Calculate the gas cost of a transaction based on L1 block data posted on L2, depending on the [OpSpec] passed.
    pub fn calculate_tx_l1_cost(&self, input: &[u8], spec_id: OpSpec) -> U256 {
        // If the input is a deposit transaction or empty, the default value is zero.
        if input.is_empty() || input.first() == Some(&0x7F) {
            return U256::ZERO;
        }

        if spec_id.is_enabled_in(OpSpecId::FJORD) {
            self.calculate_tx_l1_cost_fjord(input)
        } else if spec_id.is_enabled_in(OpSpecId::ECOTONE) {
            self.calculate_tx_l1_cost_ecotone(input, spec_id)
        } else {
            self.calculate_tx_l1_cost_bedrock(input, spec_id)
        }
    }

    /// Calculate the gas cost of a transaction based on L1 block data posted on L2, pre-Ecotone.
    fn calculate_tx_l1_cost_bedrock(&self, input: &[u8], spec_id: OpSpec) -> U256 {
        let rollup_data_gas_cost = self.data_gas(input, spec_id);
        rollup_data_gas_cost
            .saturating_add(self.l1_fee_overhead.unwrap_or_default())
            .saturating_mul(self.l1_base_fee)
            .saturating_mul(self.l1_base_fee_scalar)
            .wrapping_div(U256::from(1_000_000))
    }

    /// Calculate the gas cost of a transaction based on L1 block data posted on L2, post-Ecotone.
    ///
    /// [OpSpecId::ECOTONE] L1 cost function:
    /// `(calldataGas/16)*(l1BaseFee*16*l1BaseFeeScalar + l1BlobBaseFee*l1BlobBaseFeeScalar)/1e6`
    ///
    /// We divide "calldataGas" by 16 to change from units of calldata gas to "estimated # of bytes when compressed".
    /// Known as "compressedTxSize" in the spec.
    ///
    /// Function is actually computed as follows for better precision under integer arithmetic:
    /// `calldataGas*(l1BaseFee*16*l1BaseFeeScalar + l1BlobBaseFee*l1BlobBaseFeeScalar)/16e6`
    fn calculate_tx_l1_cost_ecotone(&self, input: &[u8], spec_id: OpSpec) -> U256 {
        // There is an edgecase where, for the very first Ecotone block (unless it is activated at Genesis), we must
        // use the Bedrock cost function. To determine if this is the case, we can check if the Ecotone parameters are
        // unset.
        if self.empty_scalars {
            return self.calculate_tx_l1_cost_bedrock(input, spec_id);
        }

        let rollup_data_gas_cost = self.data_gas(input, spec_id);
        let l1_fee_scaled = self.calculate_l1_fee_scaled_ecotone();

        l1_fee_scaled
            .saturating_mul(rollup_data_gas_cost)
            .wrapping_div(U256::from(1_000_000 * NON_ZERO_BYTE_COST))
    }

    /// Calculate the gas cost of a transaction based on L1 block data posted on L2, post-Fjord.
    ///
    /// [OpSpecId::FJORD] L1 cost function:
    /// `estimatedSize*(baseFeeScalar*l1BaseFee*16 + blobFeeScalar*l1BlobBaseFee)/1e12`
    fn calculate_tx_l1_cost_fjord(&self, input: &[u8]) -> U256 {
        let l1_fee_scaled = self.calculate_l1_fee_scaled_ecotone();
        let estimated_size = self.tx_estimated_size_fjord(input);

        estimated_size
            .saturating_mul(l1_fee_scaled)
            .wrapping_div(U256::from(1_000_000_000_000u64))
    }

    // l1BaseFee*16*l1BaseFeeScalar + l1BlobBaseFee*l1BlobBaseFeeScalar
    fn calculate_l1_fee_scaled_ecotone(&self) -> U256 {
        let calldata_cost_per_byte = self
            .l1_base_fee
            .saturating_mul(U256::from(NON_ZERO_BYTE_COST))
            .saturating_mul(self.l1_base_fee_scalar);
        let blob_cost_per_byte = self
            .l1_blob_base_fee
            .unwrap_or_default()
            .saturating_mul(self.l1_blob_base_fee_scalar.unwrap_or_default());

        calldata_cost_per_byte.saturating_add(blob_cost_per_byte)
    }
}

#[auto_impl(&mut, Box)]
pub trait L1BlockInfoGetter {
    fn l1_block_info(&self) -> &L1BlockInfo;
    fn l1_block_info_mut(&mut self) -> &mut L1BlockInfo;
}

impl<BLOCK, TX, SPEC, DB: Database, JOURNAL: Journal<Database = DB>> L1BlockInfoGetter
    for Context<BLOCK, TX, SPEC, DB, JOURNAL, L1BlockInfo>
{
    fn l1_block_info(&self) -> &L1BlockInfo {
        &self.chain
    }

    fn l1_block_info_mut(&mut self) -> &mut L1BlockInfo {
        &mut self.chain
    }
}

impl<INSP, DB, CTX: DatabaseGetter<Database = DB> + L1BlockInfoGetter> L1BlockInfoGetter
    for InspectorContext<INSP, DB, CTX>
{
    fn l1_block_info(&self) -> &L1BlockInfo {
        self.inner.l1_block_info()
    }

    fn l1_block_info_mut(&mut self) -> &mut L1BlockInfo {
        self.inner.l1_block_info_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::primitives::{bytes, hex};

    #[test]
    fn test_data_gas_non_zero_bytes() {
        let l1_block_info = L1BlockInfo {
            l1_base_fee: U256::from(1_000_000),
            l1_fee_overhead: Some(U256::from(1_000_000)),
            l1_base_fee_scalar: U256::from(1_000_000),
            ..Default::default()
        };

        // 0xFACADE = 6 nibbles = 3 bytes
        // 0xFACADE = 1111 1010 . 1100 1010 . 1101 1110

        // Pre-regolith (ie bedrock) has an extra 68 non-zero bytes
        // gas cost = 3 non-zero bytes * NON_ZERO_BYTE_COST + NON_ZERO_BYTE_COST * 68
        // gas cost = 3 * 16 + 68 * 16 = 1136
        let input = bytes!("FACADE");
        let bedrock_data_gas = l1_block_info.data_gas(&input, OpSpecId::BEDROCK.into());
        assert_eq!(bedrock_data_gas, U256::from(1136));

        // Regolith has no added 68 non zero bytes
        // gas cost = 3 * 16 = 48
        let regolith_data_gas = l1_block_info.data_gas(&input, OpSpecId::REGOLITH.into());
        assert_eq!(regolith_data_gas, U256::from(48));

        // Fjord has a minimum compressed size of 100 bytes
        // gas cost = 100 * 16 = 1600
        let fjord_data_gas = l1_block_info.data_gas(&input, OpSpecId::FJORD.into());
        assert_eq!(fjord_data_gas, U256::from(1600));
    }

    #[test]
    fn test_data_gas_zero_bytes() {
        let l1_block_info = L1BlockInfo {
            l1_base_fee: U256::from(1_000_000),
            l1_fee_overhead: Some(U256::from(1_000_000)),
            l1_base_fee_scalar: U256::from(1_000_000),
            ..Default::default()
        };

        // 0xFA00CA00DE = 10 nibbles = 5 bytes
        // 0xFA00CA00DE = 1111 1010 . 0000 0000 . 1100 1010 . 0000 0000 . 1101 1110

        // Pre-regolith (ie bedrock) has an extra 68 non-zero bytes
        // gas cost = 3 non-zero * NON_ZERO_BYTE_COST + 2 * ZERO_BYTE_COST + NON_ZERO_BYTE_COST * 68
        // gas cost = 3 * 16 + 2 * 4 + 68 * 16 = 1144
        let input = bytes!("FA00CA00DE");
        let bedrock_data_gas = l1_block_info.data_gas(&input, OpSpecId::BEDROCK.into());
        assert_eq!(bedrock_data_gas, U256::from(1144));

        // Regolith has no added 68 non zero bytes
        // gas cost = 3 * 16 + 2 * 4 = 56
        let regolith_data_gas = l1_block_info.data_gas(&input, OpSpecId::REGOLITH.into());
        assert_eq!(regolith_data_gas, U256::from(56));

        // Fjord has a minimum compressed size of 100 bytes
        // gas cost = 100 * 16 = 1600
        let fjord_data_gas = l1_block_info.data_gas(&input, OpSpecId::FJORD.into());
        assert_eq!(fjord_data_gas, U256::from(1600));
    }

    #[test]
    fn test_calculate_tx_l1_cost() {
        let l1_block_info = L1BlockInfo {
            l1_base_fee: U256::from(1_000),
            l1_fee_overhead: Some(U256::from(1_000)),
            l1_base_fee_scalar: U256::from(1_000),
            ..Default::default()
        };

        let input = bytes!("FACADE");
        let gas_cost = l1_block_info.calculate_tx_l1_cost(&input, OpSpecId::REGOLITH.into());
        assert_eq!(gas_cost, U256::from(1048));

        // Zero rollup data gas cost should result in zero
        let input = bytes!("");
        let gas_cost = l1_block_info.calculate_tx_l1_cost(&input, OpSpecId::REGOLITH.into());
        assert_eq!(gas_cost, U256::ZERO);

        // Deposit transactions with the EIP-2718 type of 0x7F should result in zero
        let input = bytes!("7FFACADE");
        let gas_cost = l1_block_info.calculate_tx_l1_cost(&input, OpSpecId::REGOLITH.into());
        assert_eq!(gas_cost, U256::ZERO);
    }

    #[test]
    fn test_calculate_tx_l1_cost_ecotone() {
        let mut l1_block_info = L1BlockInfo {
            l1_base_fee: U256::from(1_000),
            l1_base_fee_scalar: U256::from(1_000),
            l1_blob_base_fee: Some(U256::from(1_000)),
            l1_blob_base_fee_scalar: Some(U256::from(1_000)),
            l1_fee_overhead: Some(U256::from(1_000)),
            ..Default::default()
        };

        // calldataGas * (l1BaseFee * 16 * l1BaseFeeScalar + l1BlobBaseFee * l1BlobBaseFeeScalar) / (16 * 1e6)
        // = (16 * 3) * (1000 * 16 * 1000 + 1000 * 1000) / (16 * 1e6)
        // = 51
        let input = bytes!("FACADE");
        let gas_cost = l1_block_info.calculate_tx_l1_cost(&input, OpSpecId::ECOTONE.into());
        assert_eq!(gas_cost, U256::from(51));

        // Zero rollup data gas cost should result in zero
        let input = bytes!("");
        let gas_cost = l1_block_info.calculate_tx_l1_cost(&input, OpSpecId::ECOTONE.into());
        assert_eq!(gas_cost, U256::ZERO);

        // Deposit transactions with the EIP-2718 type of 0x7F should result in zero
        let input = bytes!("7FFACADE");
        let gas_cost = l1_block_info.calculate_tx_l1_cost(&input, OpSpecId::ECOTONE.into());
        assert_eq!(gas_cost, U256::ZERO);

        // If the scalars are empty, the bedrock cost function should be used.
        l1_block_info.empty_scalars = true;
        let input = bytes!("FACADE");
        let gas_cost = l1_block_info.calculate_tx_l1_cost(&input, OpSpecId::ECOTONE.into());
        assert_eq!(gas_cost, U256::from(1048));
    }

    #[test]
    fn calculate_tx_l1_cost_ecotone() {
        // rig

        // l1 block info for OP mainnet ecotone block 118024092
        // 1710374401 (ecotone timestamp)
        // 1711603765 (block 118024092 timestamp)
        // 1720627201 (fjord timestamp)
        // <https://optimistic.etherscan.io/block/118024092>
        // decoded from
        let l1_block_info = L1BlockInfo {
            l1_base_fee: U256::from_be_bytes(hex!(
                "0000000000000000000000000000000000000000000000000000000af39ac327"
            )), // 47036678951
            l1_base_fee_scalar: U256::from(1368),
            l1_blob_base_fee: Some(U256::from_be_bytes(hex!(
                "0000000000000000000000000000000000000000000000000000000d5ea528d2"
            ))), // 57422457042
            l1_blob_base_fee_scalar: Some(U256::from(810949)),
            ..Default::default()
        };

        // second tx in OP mainnet ecotone block 118024092
        // <https://optimistic.etherscan.io/tx/0xa75ef696bf67439b4d5b61da85de9f3ceaa2e145abe982212101b244b63749c2>
        const TX: &[u8] = &hex!("02f8b30a832253fc8402d11f39842c8a46398301388094dc6ff44d5d932cbd77b52e5612ba0529dc6226f180b844a9059cbb000000000000000000000000d43e02db81f4d46cdf8521f623d21ea0ec7562a50000000000000000000000000000000000000000000000008ac7230489e80000c001a02947e24750723b48f886931562c55d9e07f856d8e06468e719755e18bbc3a570a0784da9ce59fd7754ea5be6e17a86b348e441348cd48ace59d174772465eadbd1");

        // l1 gas used for tx and l1 fee for tx, from OP mainnet block scanner
        // <https://optimistic.etherscan.io/tx/0xa75ef696bf67439b4d5b61da85de9f3ceaa2e145abe982212101b244b63749c2>
        let expected_l1_gas_used = U256::from(2456);
        let expected_l1_fee = U256::from_be_bytes(hex!(
            "000000000000000000000000000000000000000000000000000006a510bd7431" // 7306020222001 wei
        ));

        // test

        let gas_used = l1_block_info.data_gas(TX, OpSpecId::ECOTONE.into());

        assert_eq!(gas_used, expected_l1_gas_used);

        let l1_fee = l1_block_info.calculate_tx_l1_cost_ecotone(TX, OpSpecId::ECOTONE.into());

        assert_eq!(l1_fee, expected_l1_fee)
    }

    #[test]
    fn test_calculate_tx_l1_cost_fjord() {
        // l1FeeScaled = baseFeeScalar*l1BaseFee*16 + blobFeeScalar*l1BlobBaseFee
        //             = 1000 * 1000 * 16 + 1000 * 1000
        //             = 17e6
        let l1_block_info = L1BlockInfo {
            l1_base_fee: U256::from(1_000),
            l1_base_fee_scalar: U256::from(1_000),
            l1_blob_base_fee: Some(U256::from(1_000)),
            l1_blob_base_fee_scalar: Some(U256::from(1_000)),
            ..Default::default()
        };

        // fastLzSize = 4
        // estimatedSize = max(minTransactionSize, intercept + fastlzCoef*fastlzSize)
        //               = max(100e6, 836500*4 - 42585600)
        //               = 100e6
        let input = bytes!("FACADE");
        // l1Cost = estimatedSize * l1FeeScaled / 1e12
        //        = 100e6 * 17 / 1e6
        //        = 1700
        let gas_cost = l1_block_info.calculate_tx_l1_cost(&input, OpSpecId::FJORD.into());
        assert_eq!(gas_cost, U256::from(1700));

        // fastLzSize = 202
        // estimatedSize = max(minTransactionSize, intercept + fastlzCoef*fastlzSize)
        //               = max(100e6, 836500*202 - 42585600)
        //               = 126387400
        let input = bytes!("02f901550a758302df1483be21b88304743f94f80e51afb613d764fa61751affd3313c190a86bb870151bd62fd12adb8e41ef24f3f000000000000000000000000000000000000000000000000000000000000006e000000000000000000000000af88d065e77c8cc2239327c5edb3a432268e5831000000000000000000000000000000000000000000000000000000000003c1e5000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000148c89ed219d02f1a5be012c689b4f5b731827bebe000000000000000000000000c001a033fd89cb37c31b2cba46b6466e040c61fc9b2a3675a7f5f493ebd5ad77c497f8a07cdf65680e238392693019b4092f610222e71b7cec06449cb922b93b6a12744e");
        // l1Cost = estimatedSize * l1FeeScaled / 1e12
        //        = 126387400 * 17 / 1e6
        //        = 2148
        let gas_cost = l1_block_info.calculate_tx_l1_cost(&input, OpSpecId::FJORD.into());
        assert_eq!(gas_cost, U256::from(2148));

        // Zero rollup data gas cost should result in zero
        let input = bytes!("");
        let gas_cost = l1_block_info.calculate_tx_l1_cost(&input, OpSpecId::FJORD.into());
        assert_eq!(gas_cost, U256::ZERO);

        // Deposit transactions with the EIP-2718 type of 0x7F should result in zero
        let input = bytes!("7FFACADE");
        let gas_cost = l1_block_info.calculate_tx_l1_cost(&input, OpSpecId::FJORD.into());
        assert_eq!(gas_cost, U256::ZERO);
    }

    #[test]
    fn calculate_tx_l1_cost_fjord() {
        // rig

        // L1 block info for OP mainnet fjord block 124665056
        // <https://optimistic.etherscan.io/block/124665056>
        let l1_block_info = L1BlockInfo {
            l1_base_fee: U256::from(1055991687),
            l1_base_fee_scalar: U256::from(5227),
            l1_blob_base_fee_scalar: Some(U256::from(1014213)),
            l1_blob_base_fee: Some(U256::from(1)),
            ..Default::default() // L1 fee overhead (l1 gas used) deprecated since Fjord
        };

        // Second tx in OP mainnet Fjord block 124665056
        // <https://optimistic.etherscan.io/tx/0x1059e8004daff32caa1f1b1ef97fe3a07a8cf40508f5b835b66d9420d87c4a4a>
        const TX: &[u8] = &hex!("02f904940a8303fba78401d6d2798401db2b6d830493e0943e6f4f7866654c18f536170780344aa8772950b680b904246a761202000000000000000000000000087000a300de7200382b55d40045000000e5d60e0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003a0000000000000000000000000000000000000000000000000000000000000022482ad56cb0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000120000000000000000000000000dc6ff44d5d932cbd77b52e5612ba0529dc6226f1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000044095ea7b300000000000000000000000021c4928109acb0659a88ae5329b5374a3024694c0000000000000000000000000000000000000000000000049b9ca9a6943400000000000000000000000000000000000000000000000000000000000000000000000000000000000021c4928109acb0659a88ae5329b5374a3024694c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000024b6b55f250000000000000000000000000000000000000000000000049b9ca9a694340000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000415ec214a3950bea839a7e6fbb0ba1540ac2076acd50820e2d5ef83d0902cdffb24a47aff7de5190290769c4f0a9c6fabf63012986a0d590b1b571547a8c7050ea1b00000000000000000000000000000000000000000000000000000000000000c080a06db770e6e25a617fe9652f0958bd9bd6e49281a53036906386ed39ec48eadf63a07f47cf51a4a40b4494cf26efc686709a9b03939e20ee27e59682f5faa536667e");

        // L1 gas used for tx and L1 fee for tx, from OP mainnet block scanner
        // https://optimistic.etherscan.io/tx/0x1059e8004daff32caa1f1b1ef97fe3a07a8cf40508f5b835b66d9420d87c4a4a
        let expected_data_gas = U256::from(4471);
        let expected_l1_fee = U256::from_be_bytes(hex!(
            "00000000000000000000000000000000000000000000000000000005bf1ab43d"
        ));

        // test

        let data_gas = l1_block_info.data_gas(TX, OpSpecId::FJORD.into());

        assert_eq!(data_gas, expected_data_gas);

        let l1_fee = l1_block_info.calculate_tx_l1_cost_fjord(TX);

        assert_eq!(l1_fee, expected_l1_fee)
    }
}
