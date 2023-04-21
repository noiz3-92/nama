/// Integration of Ferveo cryptographic primitives
/// to enable encrypted txs inside of normal txs.
/// *Not wasm compatible*
#[cfg(feature = "ferveo-tpke")]
pub mod wrapper_tx {
    use std::convert::TryFrom;
    use std::num::ParseIntError;
    use std::str::FromStr;

    pub use ark_bls12_381::Bls12_381 as EllipticCurve;
    pub use ark_ec::{AffineCurve, PairingEngine};
    use borsh::{BorshDeserialize, BorshSchema, BorshSerialize};
    use masp_primitives::transaction::Transaction;
    use serde::{Deserialize, Serialize};
    use thiserror::Error;

    use crate::proto::Tx;
    use crate::types::address::{masp, Address};
    use crate::types::chain::ChainId;
    use crate::types::key::common::SecretKey;
    use crate::types::key::*;
    use crate::types::storage::Epoch;
    use crate::types::time::DateTimeUtc;
    use crate::types::token::{Amount, Transfer};
    use crate::types::transaction::encrypted::EncryptedTx;
    use crate::types::transaction::{EncryptionKey, Hash, TxError, TxType};

    /// TODO: Determine a sane number for this
    const GAS_LIMIT_RESOLUTION: u64 = 1_000_000;

    /// Errors relating to decrypting a wrapper tx and its
    /// encrypted payload from a Tx type
    #[allow(missing_docs)]
    #[derive(Error, Debug)]
    pub enum WrapperTxErr {
        #[error(
            "The hash of the decrypted tx does not match the hash commitment"
        )]
        DecryptedHash,
        #[error("The decryption did not produce a valid Tx")]
        InvalidTx,
        #[error("The given Tx data did not contain a valid WrapperTx")]
        InvalidWrapperTx,
        #[error(
            "Attempted to sign WrapperTx with keypair whose public key \
             differs from that in the WrapperTx"
        )]
        InvalidKeyPair,
        #[error("The provided unshielding tx is invalid: {0}")]
        InvalidUnshield(String),
        #[error("The given Tx fee amount overflowed")]
        OverflowingFee,
    }

    /// A fee is an amount of a specified token
    #[derive(
        Debug,
        Clone,
        PartialEq,
        BorshSerialize,
        BorshDeserialize,
        BorshSchema,
        Serialize,
        Deserialize,
    )]
    pub struct Fee {
        /// amount of fee per gas unit
        pub amount_per_gas_unit: Amount,
        /// address of the token
        pub token: Address,
    }

    /// Gas limits must be multiples of GAS_LIMIT_RESOLUTION
    /// This is done to minimize the amount of information leak from
    /// a wrapper tx. The larger the GAS_LIMIT_RESOLUTION, the
    /// less info leaked.
    ///
    /// This struct only stores the multiple of GAS_LIMIT_RESOLUTION,
    /// not the raw amount
    #[derive(
        Debug,
        Clone,
        PartialEq,
        Serialize,
        Deserialize,
        BorshSerialize,
        BorshDeserialize,
        BorshSchema,
    )]
    #[serde(from = "u64")]
    #[serde(into = "u64")]
    pub struct GasLimit {
        multiplier: u64,
    }

    impl GasLimit {
        /// We refund unused gas up to GAS_LIMIT_RESOLUTION
        pub fn refund_amount(&self, used_gas: u64) -> Amount {
            if used_gas < (u64::from(self) - GAS_LIMIT_RESOLUTION) {
                // we refund only up to GAS_LIMIT_RESOLUTION
                GAS_LIMIT_RESOLUTION
            } else if used_gas >= u64::from(self) {
                // Gas limit was under estimated, no refund
                0
            } else {
                // compute refund
                u64::from(self) - used_gas
            }
            .into()
        }
    }

    /// Round the input number up to the next highest multiple
    /// of GAS_LIMIT_RESOLUTION
    impl From<u64> for GasLimit {
        fn from(amount: u64) -> GasLimit {
            // we could use the ceiling function but this way avoids casts to
            // floats
            if GAS_LIMIT_RESOLUTION * (amount / GAS_LIMIT_RESOLUTION) < amount {
                GasLimit {
                    multiplier: (amount / GAS_LIMIT_RESOLUTION) + 1,
                }
            } else {
                GasLimit {
                    multiplier: (amount / GAS_LIMIT_RESOLUTION),
                }
            }
        }
    }

    /// Get back the gas limit as a raw number
    impl From<&GasLimit> for u64 {
        fn from(limit: &GasLimit) -> u64 {
            limit.multiplier * GAS_LIMIT_RESOLUTION
        }
    }

    /// Get back the gas limit as a raw number
    impl From<GasLimit> for u64 {
        fn from(limit: GasLimit) -> u64 {
            limit.multiplier * GAS_LIMIT_RESOLUTION
        }
    }

    impl FromStr for GasLimit {
        type Err = ParseIntError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            // Expect input to be the multiplier
            Ok(Self {
                multiplier: s.parse()?,
            })
        }
    }

    /// A transaction with an encrypted payload, an optional shielded pool
    /// unshielding tx for fee payment and some non-encrypted metadata for
    /// inclusion and / or verification purposes
    #[derive(
        Debug,
        Clone,
        BorshSerialize,
        BorshDeserialize,
        BorshSchema,
        Serialize,
        Deserialize,
    )]
    pub struct WrapperTx {
        /// The fee to be payed for including the tx
        pub fee: Fee,
        /// Used to determine an implicit account of the fee payer
        pub pk: common::PublicKey,
        /// The epoch in which the tx is to be submitted. This determines
        /// which decryption key will be used
        pub epoch: Epoch,
        /// Max amount of gas that can be used when executing the inner tx
        pub gas_limit: GasLimit,
        /// The optional, unencrypted, unshielding transaction for fee payment
        pub unshield: Option<Transaction>,
        /// the encrypted payload
        pub inner_tx: EncryptedTx,
        /// sha-2 hash of the inner transaction acting as a commitment
        /// the contents of the encrypted payload
        pub tx_hash: Hash,
        #[cfg(not(feature = "mainnet"))]
        /// A PoW solution can be used to allow zero-fee testnet transactions
        pub pow_solution: Option<crate::ledger::testnet_pow::Solution>,
    }

    impl WrapperTx {
        /// Create a new wrapper tx from unencrypted tx, the personal keypair,
        /// an optional unshielding tx, and the metadata surrounding the
        /// inclusion of the tx. This method constructs the signature of
        /// relevant data and encrypts the transaction
        #[allow(clippy::too_many_arguments)]
        pub fn new(
            fee: Fee,
            keypair: &common::SecretKey,
            epoch: Epoch,
            gas_limit: GasLimit,
            tx: Tx,
            encryption_key: EncryptionKey,
            #[cfg(not(feature = "mainnet"))] pow_solution: Option<
                crate::ledger::testnet_pow::Solution,
            >,
            unshield: Option<Transaction>,
        ) -> WrapperTx {
            let inner_tx = EncryptedTx::encrypt(&tx.to_bytes(), encryption_key);
            Self {
                fee,
                pk: keypair.ref_to(),
                epoch,
                gas_limit,
                unshield,
                inner_tx,
                tx_hash: Hash(tx.unsigned_hash()),
                #[cfg(not(feature = "mainnet"))]
                pow_solution,
            }
        }

        /// Get the address of the implicit account associated
        /// with the public key
        pub fn fee_payer(&self) -> Address {
            Address::from(&self.pk)
        }

        /// A validity check on the ciphertext.
        pub fn validate_ciphertext(&self) -> bool {
            self.inner_tx.0.check(&<EllipticCurve as PairingEngine>::G1Prepared::from(
                -<EllipticCurve as PairingEngine>::G1Affine::prime_subgroup_generator(),
            ))
        }

        /// Decrypt the wrapped transaction.
        ///
        /// Will fail if the inner transaction doesn't match the
        /// hash commitment or we are unable to recover a
        /// valid Tx from the decoded byte stream.
        pub fn decrypt(
            &self,
            privkey: <EllipticCurve as PairingEngine>::G2Affine,
        ) -> Result<Tx, WrapperTxErr> {
            // decrypt the inner tx
            let decrypted = self.inner_tx.decrypt(privkey);
            let decrypted_tx = Tx::try_from(decrypted.as_ref())
                .map_err(|_| WrapperTxErr::InvalidTx)?;

            // check that the hash equals commitment
            if decrypted_tx.unsigned_hash() != self.tx_hash.0 {
                return Err(WrapperTxErr::DecryptedHash);
            }

            Ok(decrypted_tx)
        }

        /// Sign the wrapper transaction and convert to a normal Tx type
        pub fn sign(
            &self,
            keypair: &common::SecretKey,
            chain_id: ChainId,
            expiration: Option<DateTimeUtc>,
        ) -> Result<Tx, WrapperTxErr> {
            if self.pk != keypair.ref_to() {
                return Err(WrapperTxErr::InvalidKeyPair);
            }
            Ok(Tx::new(
                vec![],
                Some(
                    TxType::Wrapper(self.clone())
                        .try_to_vec()
                        .expect("Could not serialize WrapperTx"),
                ),
                chain_id,
                expiration,
            )
            .sign(keypair))
        }

        /// Validate the signature of a wrapper tx
        pub fn validate_sig(
            &self,
            signed_data: [u8; 32],
            sig: &common::Signature,
        ) -> Result<(), TxError> {
            common::SigScheme::verify_signature(&self.pk, &signed_data, sig)
                .map_err(|err| {
                    TxError::SigError(format!(
                        "WrapperTx signature verification failed: {}",
                        err
                    ))
                })
        }

        /// Performs validation on the optional fee unshielding data carried by
        /// the wrapper and generates the tx for execution. The provided
        /// `expiration` and `chain_id` should be the same as the wrapper for
        /// safety reasons.
        pub fn check_and_generate_fee_unshielding(
            &self,
            transparent_balance: Amount,
            chain_id: ChainId,
            expiration: Option<DateTimeUtc>,
            transfer_code: Vec<u8>,
            descriptions_limit: u64,
        ) -> Result<Option<Tx>, WrapperTxErr> {
            // Check that the number of descriptions is within a certain limit
            // to avoid a possible DOS vector
            if let Some(ref unshield) = self.unshield {
                let spends = unshield.shielded_spends.len();
                let converts = unshield.shielded_converts.len();
                let outs = unshield.shielded_outputs.len();

                let descriptions = spends
                    .checked_add(converts)
                    .ok_or_else(|| {
                        WrapperTxErr::InvalidUnshield(
                            "Descriptions overflow".to_string(),
                        )
                    })?
                    .checked_add(outs)
                    .ok_or_else(|| {
                        WrapperTxErr::InvalidUnshield(
                            "Descriptions overflow".to_string(),
                        )
                    })?;

                if u64::try_from(descriptions)
                    .map_err(|e| WrapperTxErr::InvalidUnshield(e.to_string()))?
                    > descriptions_limit
                {
                    return Err(WrapperTxErr::InvalidUnshield(
                        "Descriptions exceed the maximum amount allowed"
                            .to_string(),
                    ));
                }
                return self.generate_fee_unshielding(
                    transparent_balance,
                    chain_id,
                    expiration,
                    transfer_code,
                );
            }

            Ok(None)
        }

        /// Generates the optional fee unshielding tx for execution. The
        /// provided `expiration` and `chain_id` should be the same as the
        /// wrapper for safety reasons.
        pub fn generate_fee_unshielding(
            &self,
            transparent_balance: Amount,
            chain_id: ChainId,
            expiration: Option<DateTimeUtc>,
            transfer_code: Vec<u8>,
        ) -> Result<Option<Tx>, WrapperTxErr> {
            if self.unshield.is_some() {
                let amount = self
                    .fee
                    .amount
                    .checked_sub(transparent_balance)
                    .ok_or_else(|| {
                    WrapperTxErr::InvalidUnshield(
                        "The transparent balance of the fee payer is enough \
                         to pay fees, no need for unshielding"
                            .to_string(),
                    )
                })?;

                let transfer = Transfer {
                    source: masp(),
                    target: self.fee_payer(),
                    token: self.fee.token.clone(),
                    sub_prefix: None,
                    amount,
                    key: None,
                    shielded: self.unshield.clone(),
                };

                let tx = Tx::new(
                    transfer_code,
                    Some(transfer.try_to_vec().map_err(|_| {
                        WrapperTxErr::InvalidUnshield(
                            "Error while serializing the unshield transfer \
                             data"
                                .to_string(),
                        )
                    })?),
                    chain_id,
                    expiration,
                );

                // Mock a signature. We don't have the signign key of masp in
                // the ledger but the masp vp does not check it
                let mock_sigkey = SecretKey::Ed25519(ed25519::SecretKey(
                    Box::new([0; 32].into()),
                ));

                return Ok(Some(tx.sign(&mock_sigkey)));
            }

            Ok(None)
        }

        /// Get the [`Amount`] of fees to be paid by the given wrapper. Returns an error if the amount overflows
        //FIXME: unit test this
        pub fn get_tx_fee(&self) -> Result<Amount, WrapperTxErr> {
            u64::checked_mul(
                u64::from(&self.gas_limit),
                self.fee.amount_per_gas_unit.into(),
            )
            .map(|v| v.into())
            .ok_or_else(|| WrapperTxErr::OverflowingFee)
        }
    }

    #[cfg(test)]
    mod test_gas_limits {
        use super::*;

        /// Test serializing and deserializing again gives back original object
        /// Test that serializing converts GasLimit to u64 correctly
        #[test]
        fn test_gas_limit_roundtrip() {
            let limit = GasLimit { multiplier: 1 };
            // Test serde roundtrip
            let js = serde_json::to_string(&limit).expect("Test failed");
            assert_eq!(js, format!("{}", GAS_LIMIT_RESOLUTION));
            let new_limit: GasLimit =
                serde_json::from_str(&js).expect("Test failed");
            assert_eq!(new_limit, limit);

            // Test borsh roundtrip
            let borsh = limit.try_to_vec().expect("Test failed");
            assert_eq!(
                limit,
                BorshDeserialize::deserialize(&mut borsh.as_ref())
                    .expect("Test failed")
            );
        }

        /// Test that when we deserialize a u64 that is not a multiple of
        /// GAS_LIMIT_RESOLUTION to a GasLimit, it rounds up to the next
        /// multiple
        #[test]
        fn test_deserialize_not_multiple_of_resolution() {
            let js = serde_json::to_string(&(GAS_LIMIT_RESOLUTION + 1))
                .expect("Test failed");
            let limit: GasLimit =
                serde_json::from_str(&js).expect("Test failed");
            assert_eq!(limit, GasLimit { multiplier: 2 });
        }

        /// Test that refund is calculated correctly
        #[test]
        fn test_gas_limit_refund() {
            let limit = GasLimit { multiplier: 1 };
            let refund = limit.refund_amount(GAS_LIMIT_RESOLUTION - 1);
            assert_eq!(refund, Amount::from(1u64));
        }

        /// Test that we don't refund more than GAS_LIMIT_RESOLUTION
        #[test]
        fn test_gas_limit_too_high_no_refund() {
            let limit = GasLimit { multiplier: 2 };
            let refund = limit.refund_amount(GAS_LIMIT_RESOLUTION - 1);
            assert_eq!(refund, Amount::from(GAS_LIMIT_RESOLUTION));
        }

        /// Test that if gas usage was underestimated, we issue no refund
        #[test]
        fn test_gas_limit_too_low_no_refund() {
            let limit = GasLimit { multiplier: 1 };
            let refund = limit.refund_amount(GAS_LIMIT_RESOLUTION + 1);
            assert_eq!(refund, Amount::from(0u64));
        }
    }

    #[cfg(test)]
    mod test_wrapper_tx {
        use super::*;
        use crate::proto::SignedTxData;
        use crate::types::address::nam;

        fn gen_keypair() -> common::SecretKey {
            use rand::prelude::ThreadRng;
            use rand::thread_rng;

            use crate::types::key::SecretKey;

            let mut rng: ThreadRng = thread_rng();
            ed25519::SigScheme::generate(&mut rng).try_to_sk().unwrap()
        }

        /// We test that when we feed in a Tx and then decrypt it again
        /// that we get what we started with.
        #[test]
        fn test_encryption_round_trip() {
            let keypair = gen_keypair();
            let tx = Tx::new(
                "wasm code".as_bytes().to_owned(),
                Some("transaction data".as_bytes().to_owned()),
                ChainId::default(),
                Some(DateTimeUtc::now()),
            );

            let wrapper = WrapperTx::new(
                Fee {
                    amount_per_gas_unit: 10.into(),
                    token: nam(),
                },
                &keypair,
                Epoch(0),
                0.into(),
                tx.clone(),
                Default::default(),
                #[cfg(not(feature = "mainnet"))]
                None,
                None,
            );
            assert!(wrapper.validate_ciphertext());
            let privkey = <EllipticCurve as PairingEngine>::G2Affine::prime_subgroup_generator();
            let decrypted = wrapper.decrypt(privkey).expect("Test failed");
            assert_eq!(tx, decrypted);
        }

        /// We test that when we try to decrypt a tx and it
        /// does not match the commitment, an error is returned
        #[test]
        fn test_decryption_invalid_hash() {
            let tx = Tx::new(
                "wasm code".as_bytes().to_owned(),
                Some("transaction data".as_bytes().to_owned()),
                ChainId::default(),
                Some(DateTimeUtc::now()),
            );

            let mut wrapper = WrapperTx::new(
                Fee {
                    amount_per_gas_unit: 10.into(),
                    token: nam(),
                },
                &gen_keypair(),
                Epoch(0),
                0.into(),
                tx,
                Default::default(),
                #[cfg(not(feature = "mainnet"))]
                None,
                None,
            );
            // give a incorrect commitment to the decrypted contents of the tx
            wrapper.tx_hash = Hash([0u8; 32]);
            assert!(wrapper.validate_ciphertext());
            let privkey = <EllipticCurve as PairingEngine>::G2Affine::prime_subgroup_generator();
            let err = wrapper.decrypt(privkey).expect_err("Test failed");
            assert_matches!(err, WrapperTxErr::DecryptedHash);
        }

        /// We check that even if the encrypted payload and hash of its
        /// contents are correctly changed, we detect fraudulent activity
        /// via the signature.
        #[test]
        fn test_malleability_attack_detection() {
            let pubkey = <EllipticCurve as PairingEngine>::G1Affine::prime_subgroup_generator();
            let keypair = gen_keypair();
            // The intended tx
            let tx = Tx::new(
                "wasm code".as_bytes().to_owned(),
                Some("transaction data".as_bytes().to_owned()),
                ChainId::default(),
                Some(DateTimeUtc::now()),
            );
            // the signed tx
            let mut tx = WrapperTx::new(
                Fee {
                    amount_per_gas_unit: 10.into(),
                    token: nam(),
                },
                &keypair,
                Epoch(0),
                0.into(),
                tx,
                Default::default(),
                #[cfg(not(feature = "mainnet"))]
                None,
                None,
            )
            .sign(&keypair, ChainId::default(), None)
            .expect("Test failed");

            // we now try to alter the inner tx maliciously
            let mut wrapper = if let TxType::Wrapper(wrapper) =
                crate::types::transaction::process_tx(tx.clone())
                    .expect("Test failed")
            {
                wrapper
            } else {
                panic!("Test failed")
            };

            let mut signed_tx_data =
                SignedTxData::try_from_slice(&tx.data.unwrap()[..])
                    .expect("Test failed");

            // malicious transaction
            let malicious = Tx::new(
                "Give me all the money".as_bytes().to_owned(),
                None,
                ChainId::default(),
                None,
            );

            // We replace the inner tx with a malicious one
            wrapper.inner_tx = EncryptedTx::encrypt(
                &malicious.to_bytes(),
                EncryptionKey(pubkey),
            );

            // We change the commitment appropriately
            wrapper.tx_hash = Hash(malicious.unsigned_hash());

            // we check ciphertext validity still passes
            assert!(wrapper.validate_ciphertext());
            // we check that decryption still succeeds
            let decrypted = wrapper.decrypt(
                <EllipticCurve as PairingEngine>::G2Affine::prime_subgroup_generator()
            )
                .expect("Test failed");
            assert_eq!(decrypted, malicious);

            // we substitute in the modified wrapper
            signed_tx_data.data = Some(
                TxType::Wrapper(wrapper).try_to_vec().expect("Test failed"),
            );
            tx.data = Some(signed_tx_data.try_to_vec().expect("Test failed"));

            // check that the signature is not valid
            tx.verify_sig(&keypair.ref_to(), &signed_tx_data.sig)
                .expect_err("Test failed");
            // check that the try from method also fails
            let err = crate::types::transaction::process_tx(tx)
                .expect_err("Test failed");
            assert_matches!(err, TxError::SigError(_));
        }
    }
}

#[cfg(feature = "ferveo-tpke")]
pub use wrapper_tx::*;
