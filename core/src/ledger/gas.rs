//! Gas accounting module to track the gas usage in a block for transactions and
//! validity predicates triggered by transactions.

use borsh::{BorshDeserialize, BorshSchema, BorshSerialize};
use thiserror::Error;

#[allow(missing_docs)]
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum Error {
    #[error("Transaction gas limit exceeded")]
    TransactionGasExceededError,
    #[error("Block gas limit exceeded")]
    BlockGasExceeded,
    #[error("Overflow during gas operations")]
    GasOverflow,
    #[error("Error converting to u64")]
    ConversionError,
}

const TX_SIZE_GAS_PER_BYTE: u64 = 10;
const COMPILE_GAS_PER_BYTE: u64 = 1;
const PARALLEL_GAS_DIVIDER: u64 = 10;

/// The cost of accessing the storage, per byte
pub const STORAGE_ACCESS_GAS_PER_BYTE: u64 = 1;
/// The cost of writing to storage, per byte
pub const STORAGE_WRITE_GAS_PER_BYTE: u64 = 100;
/// The cost of verifying the signature of a transaction
pub const VERIFY_TX_SIG_GAS_COST: u64 = 10;
/// The cost of validating wasm vp code
pub const WASM_VALIDATION_GAS_PER_BYTE: u64 = 1;
/// The cost of accessing the WASM memory, per byte
pub const VM_MEMORY_ACCESS_GAS_PER_BYTE: u64 = 1;

/// Gas module result for functions that may fail
pub type Result<T> = std::result::Result<T, Error>;

/// Trait to share gas operations for transactions and validity predicates
pub trait GasMetering {
    /// Add gas cost. It will return error when the
    /// consumed gas exceeds the provided transaction gas limit, but the state
    /// will still be updated
    fn consume(&mut self, gas: u64) -> Result<()>;

    /// Add the compiling cost proportionate to the code length
    fn add_compiling_gas(&mut self, bytes_len: u64) -> Result<()> {
        tracing::error!(
            "Adding compile cost: {}",
            bytes_len * COMPILE_GAS_PER_BYTE
        ); //FIXME: remove
        self.consume(
            bytes_len
                .checked_mul(COMPILE_GAS_PER_BYTE)
                .ok_or(Error::GasOverflow)?,
        )
    }

    /// Add the gas for loading the wasm code from storage
    fn add_wasm_load_from_storage_gas(&mut self, bytes_len: u64) -> Result<()> {
        tracing::error!(
            "Adding load from storage cost: {}",
            bytes_len * STORAGE_ACCESS_GAS_PER_BYTE
        ); //FIXME: remove
        self.consume(
            bytes_len
                .checked_mul(STORAGE_ACCESS_GAS_PER_BYTE)
                .ok_or(Error::GasOverflow)?,
        )
    }

    /// Get the gas consumed by the tx alone
    fn get_tx_gas(&self) -> u64;

    /// Get the gas limit
    fn get_gas_limit(&self) -> u64;
}

/// Gas metering in a transaction
#[derive(Debug)]
pub struct TxGasMeter {
    /// The gas limit for a transaction
    pub tx_gas_limit: u64,
    transaction_gas: u64,
}

/// Gas metering in a validity predicate
#[derive(Debug, Clone)]
pub struct VpGasMeter {
    /// The transaction gas limit
    tx_gas_limit: u64,
    /// The gas used in the transaction before the VP run
    initial_gas: u64,
    /// The current gas usage in the VP
    pub current_gas: u64,
}

/// Gas meter for VPs parallel runs
#[derive(
    Clone, Debug, Default, BorshSerialize, BorshDeserialize, BorshSchema,
)]
pub struct VpsGas {
    max: Option<u64>,
    rest: Vec<u64>,
}

impl GasMetering for TxGasMeter {
    fn consume(&mut self, gas: u64) -> Result<()> {
        self.transaction_gas = self
            .transaction_gas
            .checked_add(gas)
            .ok_or(Error::GasOverflow)?;

        if self.transaction_gas > self.tx_gas_limit {
            return Err(Error::TransactionGasExceededError);
        }

        Ok(())
    }

    fn get_tx_gas(&self) -> u64 {
        self.transaction_gas
    }

    fn get_gas_limit(&self) -> u64 {
        self.tx_gas_limit
    }
}

impl TxGasMeter {
    /// Initialize a new Tx gas meter. Requires the gas limit for the specific
    /// transaction
    pub fn new(tx_gas_limit: u64) -> Self {
        Self {
            tx_gas_limit,
            transaction_gas: 0,
        }
    }

    /// Add the gas for the space that the transaction requires in the block
    pub fn add_tx_size_gas(&mut self, tx_bytes: &[u8]) -> Result<()> {
        let bytes_len: u64 = tx_bytes
            .len()
            .try_into()
            .map_err(|_| Error::ConversionError)?;
        self.consume(
            bytes_len
                .checked_mul(TX_SIZE_GAS_PER_BYTE)
                .ok_or(Error::GasOverflow)?,
        )
    }

    /// Add the gas cost used in validity predicates to the current transaction.
    pub fn add_vps_gas(&mut self, vps_gas: &VpsGas) -> Result<()> {
        tracing::error!(
            "Adding vp gas: {}",
            vps_gas.get_current_gas().unwrap()
        ); //FIXME: remove
        self.consume(vps_gas.get_current_gas()?)
    }

    /// Get the total gas used in the current transaction.
    pub fn get_current_transaction_gas(&self) -> u64 {
        self.transaction_gas
    }
}

impl GasMetering for VpGasMeter {
    fn consume(&mut self, gas: u64) -> Result<()> {
        self.current_gas = self
            .current_gas
            .checked_add(gas)
            .ok_or(Error::GasOverflow)?;

        let current_total = self
            .initial_gas
            .checked_add(self.current_gas)
            .ok_or(Error::GasOverflow)?;

        if current_total > self.tx_gas_limit {
            return Err(Error::TransactionGasExceededError);
        }

        Ok(())
    }

    fn get_tx_gas(&self) -> u64 {
        self.initial_gas
    }

    fn get_gas_limit(&self) -> u64 {
        self.tx_gas_limit
    }
}

impl VpGasMeter {
    /// Initialize a new VP gas meter from the `TxGasMeter`
    pub fn new_from_tx_meter(tx_gas_meter: &TxGasMeter) -> Self {
        Self {
            tx_gas_limit: tx_gas_meter.tx_gas_limit,
            initial_gas: tx_gas_meter.transaction_gas,
            current_gas: 0,
        }
    }
}

impl VpsGas {
    /// Set the gas cost from a single VP run. It consumes the [`VpGasMeter`]
    /// instance which shouldn't be accessed passed this point.
    pub fn set(&mut self, vp_gas_meter: VpGasMeter) -> Result<()> {
        debug_assert_eq!(self.max, None);
        debug_assert!(self.rest.is_empty());
        self.max = Some(vp_gas_meter.current_gas);
        self.check_limit(&vp_gas_meter)
    }

    /// Merge validity predicates gas meters from parallelized runs.
    pub fn merge(
        &mut self,
        other: &mut VpsGas,
        tx_gas_meter: &TxGasMeter,
    ) -> Result<()> {
        match (self.max, other.max) {
            (None, Some(_)) => {
                self.max = other.max;
            }
            (Some(this_max), Some(other_max)) => {
                if this_max < other_max {
                    self.rest.push(this_max);
                    self.max = other.max;
                } else {
                    self.rest.push(other_max);
                }
            }
            _ => {}
        }
        self.rest.append(&mut other.rest);

        self.check_limit(tx_gas_meter)
    }

    fn check_limit(&self, gas_meter: &impl GasMetering) -> Result<()> {
        let total = gas_meter
            .get_tx_gas()
            .checked_add(self.get_current_gas()?)
            .ok_or(Error::GasOverflow)?;
        if total > gas_meter.get_gas_limit() {
            return Err(Error::TransactionGasExceededError);
        }
        Ok(())
    }

    /// Get the gas consumed by the parallelized VPs
    fn get_current_gas(&self) -> Result<u64> {
        let parallel_gas = self.rest.iter().sum::<u64>() / PARALLEL_GAS_DIVIDER;
        self.max
            .unwrap_or_default()
            .checked_add(parallel_gas)
            .ok_or(Error::GasOverflow)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    const BLOCK_GAS_LIMIT: u64 = 10_000_000_000;
    const TX_GAS_LIMIT: u64 = 1_000_000;

    proptest! {
        #[test]
        fn test_vp_gas_meter_add(gas in 0..BLOCK_GAS_LIMIT) {
        let tx_gas_meter = TxGasMeter {
            tx_gas_limit: BLOCK_GAS_LIMIT,
            transaction_gas: 0,
        };
            let mut meter = VpGasMeter::new_from_tx_meter(&tx_gas_meter);
            meter.consume(gas).expect("cannot add the gas");
        }

    }

    #[test]
    fn test_vp_gas_overflow() {
        let tx_gas_meter = TxGasMeter {
            tx_gas_limit: BLOCK_GAS_LIMIT,
            transaction_gas: TX_GAS_LIMIT - 1,
        };
        let mut meter = VpGasMeter::new_from_tx_meter(&tx_gas_meter);
        assert_matches!(
            meter.consume(u64::MAX).expect_err("unexpectedly succeeded"),
            Error::GasOverflow
        );
    }

    #[test]
    fn test_vp_gas_limit() {
        let tx_gas_meter = TxGasMeter {
            tx_gas_limit: TX_GAS_LIMIT,
            transaction_gas: TX_GAS_LIMIT - 1,
        };
        let mut meter = VpGasMeter::new_from_tx_meter(&tx_gas_meter);
        assert_matches!(
            meter
                .consume(TX_GAS_LIMIT)
                .expect_err("unexpectedly succeeded"),
            Error::TransactionGasExceededError
        );
    }

    #[test]
    fn test_tx_gas_overflow() {
        let mut meter = TxGasMeter::new(BLOCK_GAS_LIMIT);
        meter.consume(1).expect("cannot add the gas");
        assert_matches!(
            meter.consume(u64::MAX).expect_err("unexpectedly succeeded"),
            Error::GasOverflow
        );
    }

    #[test]
    fn test_tx_gas_limit() {
        let mut meter = TxGasMeter::new(TX_GAS_LIMIT);
        assert_matches!(
            meter
                .consume(TX_GAS_LIMIT + 1)
                .expect_err("unexpectedly succeeded"),
            Error::TransactionGasExceededError
        );
    }
}
