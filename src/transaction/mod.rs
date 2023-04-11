// Copyright (c) 2023, MaidSafe.
// All rights reserved.
//
// This SAFE Network Software is licensed under the BSD-3-Clause license.
// Please see the LICENSE file for more details.

mod amount;
mod error;
mod input;
mod output;

use crate::rand::{CryptoRng, RngCore};
use crate::{BlindedAmount, DbcId};

use bulletproofs::{BulletproofGens, PedersenGens, RangeProof};
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::scalar::Scalar;
use merlin::Transcript;
use std::{cmp::Ordering, collections::BTreeSet};
use tiny_keccak::{Hasher, Sha3};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

pub use amount::{Amount, RevealedAmount};
pub(crate) use error::Error;
pub use input::{BlindedInput, RevealedInput};
pub use output::{BlindedOutput, Output, RevealedOutput};

pub(super) const RANGE_PROOF_BITS: usize = 64; // note: Range Proof max-bits is 64. allowed are: 8, 16, 32, 64 (only)
                                               //       This limits our amount field to 64 bits also.
pub(super) const RANGE_PROOF_PARTIES: usize = 1; // The maximum number of parties that can produce an aggregated proof
pub(super) const MERLIN_TRANSCRIPT_LABEL: &[u8] = b"SN_DBC";

type Result<T> = std::result::Result<T, Error>;

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone)]
pub struct DbcTransaction {
    pub inputs: Vec<BlindedInput>,
    pub outputs: Vec<BlindedOutput>,
}

impl PartialEq for DbcTransaction {
    fn eq(&self, other: &Self) -> bool {
        self.hash().eq(&other.hash())
    }
}

impl Eq for DbcTransaction {}

impl PartialOrd for DbcTransaction {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DbcTransaction {
    fn cmp(&self, other: &Self) -> Ordering {
        self.hash().cmp(&other.hash())
    }
}

impl DbcTransaction {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v: Vec<u8> = Default::default();
        v.extend("inputs".as_bytes());
        for m in self.inputs.iter() {
            v.extend(&m.to_bytes());
        }
        v.extend("outputs".as_bytes());
        for o in self.outputs.iter() {
            v.extend(&o.to_bytes());
        }
        v.extend("end".as_bytes());
        v
    }

    pub fn hash(&self) -> crate::Hash {
        let mut sha3 = Sha3::v256();
        sha3.update(&self.to_bytes());
        let mut hash = [0; 32];
        sha3.finalize(&mut hash);
        crate::Hash::from(hash)
    }

    // Note: must match result generated by RevealedTx::sign().
    pub fn serialize_tx(&self) -> Vec<u8> {
        // All dbc ids
        let dbc_ids: Vec<DbcId> = self.inputs.iter().map(|m| m.dbc_id).collect();
        // All input blinded amounts
        let input_amounts: Vec<BlindedAmount> =
            self.inputs.iter().map(|i| i.blinded_amount).collect();
        serialize(&dbc_ids, &input_amounts, &self.outputs)
    }

    /// Verify if the blinded amounts of the inputs, are
    /// the same as the set of blinded amounts you know of.
    /// This also checks that every input has the signature over this very tx,
    /// and that each public key of the inputs was the signer.
    pub fn verify(&self, blinded_amounts: &[BlindedAmount]) -> Result<()> {
        // check input sigs
        let msg = self.serialize_tx();
        for (input, blinded_amount) in self.inputs.iter().zip(blinded_amounts) {
            input.verify(&msg, *blinded_amount)?
        }

        let mut prover_ts = Transcript::new(MERLIN_TRANSCRIPT_LABEL);
        let bp_gens = RevealedTx::bp_gens();

        for output in self.outputs.iter() {
            // Verification requires a transcript with identical initial state:
            output.range_proof.verify_single(
                &bp_gens,
                &RevealedTx::pc_gens(),
                &mut prover_ts,
                &output.blinded_amount.compress(),
                RANGE_PROOF_BITS,
            )?;
        }

        // Verify that the tx has at least one input
        if self.inputs.is_empty() {
            return Err(Error::MissingTxInputs);
        }

        // Verify that each dbc id is unique.
        let id_count = self.inputs.len();
        let unique_ids: BTreeSet<_> = self.inputs.iter().map(|input| input.dbc_id).collect();
        if unique_ids.len() != id_count {
            return Err(Error::DbcIdNotUniqueAcrossInputs);
        }

        // Check that the input and output blinded amounts are equal.
        let input_sum: RistrettoPoint = self
            .inputs
            .iter()
            .map(|i| i.blinded_amount)
            .map(RistrettoPoint::from)
            .sum();
        let output_sum: RistrettoPoint = self
            .outputs
            .iter()
            .map(BlindedOutput::blinded_amount)
            .map(RistrettoPoint::from)
            .sum();

        if input_sum != output_sum {
            Err(Error::InconsistentDbcTransaction)
        } else {
            Ok(())
        }
    }
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone)]
pub struct InputHistory {
    pub input: RevealedInput,
    pub input_src_tx: DbcTransaction,
}

impl InputHistory {
    fn dbc_id(&self) -> DbcId {
        self.input.dbc_id()
    }
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Default)]
pub struct RevealedTx {
    pub inputs: Vec<InputHistory>,
    pub outputs: Vec<Output>,
}

impl RevealedTx {
    pub fn sign(
        &self,
        mut rng: impl RngCore + CryptoRng,
    ) -> Result<(DbcTransaction, Vec<RevealedOutput>)> {
        // We need to gather a bunch of things for our message to sign.
        //   All public keys in all inputs
        //   All input blinded amounts
        //   All output public keys.
        //   All output blinded amounts
        //   All output range proofs
        //
        //   notes:
        //     1. output blinded amounts, range_proofs, and public_keys are bundled
        //        together in BlindedOutputs
        let revealed_input_amounts = self.revealed_input_amounts();
        let input_amounts = self.blinded_input_amounts();

        // Adjust the outputs so that summed blinding factors of inputs and outputs are equal.
        let adjusted_revealed_outputs =
            self.adjusted_revealed_outputs(&revealed_input_amounts, &mut rng);
        let blinded_outputs = self.blinded_outputs(&adjusted_revealed_outputs, &mut rng)?;

        // Generate message to sign.
        // note: must match message generated by DbcTransaction::verify()
        let msg = serialize(&self.input_ids(), &input_amounts, &blinded_outputs);

        // We create a signature for each input
        let blinded_inputs: Vec<BlindedInput> = self
            .inputs
            .iter()
            .map(|input_history| input_history.input.sign(&msg, &Self::pc_gens()))
            .collect();

        Ok((
            DbcTransaction {
                inputs: blinded_inputs,
                outputs: blinded_outputs,
            },
            adjusted_revealed_outputs,
        ))
    }

    fn bp_gens() -> BulletproofGens {
        BulletproofGens::new(RANGE_PROOF_BITS, RANGE_PROOF_PARTIES)
    }

    fn pc_gens() -> PedersenGens {
        Default::default()
    }

    pub fn input_ids(&self) -> Vec<DbcId> {
        self.inputs
            .iter()
            .map(|input_history| input_history.dbc_id())
            .collect()
    }

    fn revealed_input_amounts(&self) -> Vec<RevealedAmount> {
        self.inputs
            .iter()
            .map(|input_history| *input_history.input.revealed_amount())
            .collect()
    }

    fn blinded_input_amounts(&self) -> Vec<BlindedAmount> {
        self.inputs
            .iter()
            .map(|input_history| input_history.input.blinded_amount(&Self::pc_gens()))
            .collect()
    }

    /// This produces outputs where blinding factors sum to the input blinding factors,
    /// by adjusting the blinding factor of the last output to make up for the difference in sums.
    /// The reason for doing this is that if the sum of amounts in inputs and the sum amounts of outputs
    /// are equal, then having equal sum of blinding factors, will lead to the sum of outputs BlindedAmounts
    /// and sum of inputs BlindedAmounts also to be the same. That way, others can compare and verify the amounts
    /// even though having no idea what the actual amounts are.
    fn adjusted_revealed_outputs(
        &self,
        revealed_input_amounts: &[RevealedAmount],
        mut rng: impl RngCore + CryptoRng,
    ) -> Vec<RevealedOutput> {
        // Avoid subtraction underflow in next step.
        if self.outputs.is_empty() {
            return vec![];
        }

        let mut revealed_outputs: Vec<_> = self
            .outputs
            .iter()
            .map(|out| RevealedOutput {
                dbc_id: out.dbc_id,
                revealed_amount: out.revealed_amount(&mut rng),
            })
            .take(self.outputs.len() - 1)
            .collect();

        // todo: replace fold() with sum() when supported in blstrs
        let input_summed_blinding_factors: Scalar = revealed_input_amounts
            .iter()
            .map(RevealedAmount::blinding_factor)
            .fold(Scalar::zero(), |sum, x| sum + x);

        // todo: replace fold() with sum() when supported in blstrs
        let output_summed_blinding_factors: Scalar = revealed_outputs
            .iter()
            .map(|r| r.revealed_amount.blinding_factor())
            .fold(Scalar::zero(), |sum, x| sum + x);

        let output_blinding_correction =
            input_summed_blinding_factors - output_summed_blinding_factors;

        if let Some(last_output) = self.outputs.last() {
            revealed_outputs.push(RevealedOutput {
                dbc_id: last_output.dbc_id,
                revealed_amount: RevealedAmount {
                    value: last_output.amount,
                    blinding_factor: output_blinding_correction,
                },
            });
        } else {
            panic!("Expected at least one output")
        }
        revealed_outputs
    }

    fn blinded_outputs(
        &self,
        revealed_outputs: &[RevealedOutput],
        mut rng: impl RngCore + CryptoRng,
    ) -> Result<Vec<BlindedOutput>> {
        let mut prover_ts = Transcript::new(MERLIN_TRANSCRIPT_LABEL);

        let bp_gens = Self::bp_gens();

        revealed_outputs
            .iter()
            .map(|c| {
                let (range_proof, compressed_blinded_amount) = RangeProof::prove_single_with_rng(
                    &bp_gens,
                    &Self::pc_gens(),
                    &mut prover_ts,
                    c.revealed_amount.value,
                    &c.revealed_amount.blinding_factor,
                    RANGE_PROOF_BITS,
                    &mut rng,
                )?;
                let blinded_amount = compressed_blinded_amount
                    .decompress()
                    .ok_or(Error::FailedToDecompressBlindedAmount)?;

                Ok(BlindedOutput {
                    dbc_id: c.dbc_id,
                    range_proof,
                    blinded_amount,
                })
            })
            .collect::<Result<Vec<_>>>()
    }
}

// note: used by both RevealedTx::sign and DbcTransaction::verify()
//       which must match.
fn serialize(
    dbc_ids: &[DbcId],
    input_amounts: &[BlindedAmount],
    blinded_outputs: &[BlindedOutput],
) -> Vec<u8> {
    // Generate message to sign.
    let mut msg: Vec<u8> = Default::default();
    msg.extend("dbc_ids".as_bytes());
    for id in dbc_ids.iter() {
        msg.extend(id.to_bytes().as_ref());
    }
    msg.extend("input_amounts".as_bytes());
    for r in input_amounts.iter() {
        msg.extend(r.compress().as_bytes());
    }
    msg.extend("blinded_outputs".as_bytes());
    for o in blinded_outputs.iter() {
        msg.extend(o.to_bytes());
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::DerivedKey;

    use blsttc::{rand::rngs::OsRng, SecretKey};
    use std::{collections::BTreeMap, iter::FromIterator};

    #[test]
    fn test_input_sign() {
        let mut rng = OsRng::default();
        let pc_gens = PedersenGens::default();
        let tx_amount = 3;

        let revealed_input = RevealedInput {
            derived_key: DerivedKey::new(SecretKey::random()),
            revealed_amount: RevealedAmount {
                value: tx_amount,
                blinding_factor: 5u32.into(),
            },
        };
        // The input src tx is a dummy here.
        let input_history = InputHistory {
            input: revealed_input,
            input_src_tx: DbcTransaction {
                inputs: vec![],
                outputs: vec![],
            },
        };

        let mut blinded_amounts = BTreeMap::new();
        blinded_amounts.insert(
            input_history.dbc_id(),
            input_history.input.revealed_amount.blinded_amount(&pc_gens),
        );
        blinded_amounts.insert(
            DerivedKey::new(SecretKey::random()).dbc_id(),
            RistrettoPoint::random(&mut rng),
        );
        blinded_amounts.insert(
            DerivedKey::new(SecretKey::random()).dbc_id(),
            RistrettoPoint::random(&mut rng),
        );

        let revealed_tx = RevealedTx {
            inputs: vec![input_history],
            outputs: vec![Output {
                dbc_id: DerivedKey::new(SecretKey::random()).dbc_id(),
                amount: tx_amount,
            }],
        };

        let (signed_tx, _revealed_output_amounts) =
            revealed_tx.sign(rng).expect("Failed to sign transaction");

        let blinded_amounts = Vec::from_iter(
            signed_tx
                .inputs
                .iter()
                .map(|input| *blinded_amounts.get(&input.dbc_id()).unwrap()),
        );

        assert!(signed_tx.verify(&blinded_amounts).is_ok());
    }
}
