// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::GenesisMaterial;

use crate::{
    dbc_id::PublicAddress,
    transaction::{BlindedOutput, DbcTransaction},
    BlindedAmount, DbcId, Error, Hash, Result, SignedSpend,
};

use bulletproofs::PedersenGens;
use std::collections::{BTreeMap, HashMap};

/// This is a mock SpentBook used for our test cases. A proper implementation
/// will be distributed, persistent, and auditable.
///
/// This impl attempts to be reasonably efficient.  In particular
/// it stores only a single copy of each Tx and includes indexes:
///     tx_hash    --> Tx
///     public_key  --> tx_hash
///     public_key --> BlindedOutput
///
/// The public_key map eliminates a full table scan when matching
/// public keys for each input of logged Tx to public key of BlindedOutput in
/// already-spent Txs.
///
/// This impl does duplicate the BlindedOutputs in the public_key index, which
/// is not ideal and should not be done for a "real" system.
///
/// Another approach would be to map public_key --> tx_hash. This eliminates
/// the need to store duplicate BlindedOutput. One could lookup the Tx with
/// the desired BlindedOutput, and then iterate through outputs to actually find it.
///
/// See the very first commit of this file For a naive impl that uses only
/// a single map<public_key, tx>.
#[derive(Debug, Clone)]
pub struct SpentbookNode {
    pub id: PublicAddress,
    pub transactions: HashMap<Hash, DbcTransaction>,
    pub dbc_ids: BTreeMap<DbcId, Hash>,
    pub outputs_by_input_id: BTreeMap<DbcId, BlindedOutput>,
    pub genesis: (DbcId, BlindedAmount),
}

impl Default for SpentbookNode {
    fn default() -> Self {
        let genesis_material = GenesisMaterial::default();
        let blinded_amount = genesis_material.genesis_tx.inputs[0]
            .input
            .revealed_amount()
            .blinded_amount(&PedersenGens::default());

        Self {
            id: PublicAddress::new(blsttc::SecretKey::random().public_key()),
            transactions: Default::default(),
            dbc_ids: Default::default(),
            outputs_by_input_id: Default::default(),
            genesis: (genesis_material.input_dbc_id, blinded_amount),
        }
    }
}

impl SpentbookNode {
    pub fn iter(&self) -> impl Iterator<Item = (&DbcId, &DbcTransaction)> + '_ {
        self.dbc_ids.iter().map(move |(k, h)| {
            (
                k,
                match self.transactions.get(h) {
                    Some(tx) => tx,
                    // todo: something better.
                    None => panic!("Spentbook is in an inconsistent state"),
                },
            )
        })
    }

    pub fn is_spent(&self, dbc_id: &DbcId) -> bool {
        self.dbc_ids.contains_key(dbc_id)
    }

    pub fn log_spent(&mut self, tx: &DbcTransaction, signed_spend: &SignedSpend) -> Result<()> {
        self.log_spent_worker(tx, signed_spend, true)
    }

    // This is invalid behavior, however we provide this method for test cases
    // that need to write an invalid Tx to spentbook in order to test reissue
    // behavior.
    #[cfg(test)]
    pub fn log_spent_and_skip_tx_verification(
        &mut self,
        spent_tx: &DbcTransaction,
        signed_spend: &SignedSpend,
    ) -> Result<()> {
        self.log_spent_worker(spent_tx, signed_spend, false)
    }

    fn log_spent_worker(
        &mut self,
        spent_tx: &DbcTransaction,
        signed_spend: &SignedSpend,
        verify_tx: bool,
    ) -> Result<()> {
        let input_id = signed_spend.dbc_id();
        let spent_tx_hash = signed_spend.spent_tx_hash();
        let tx_hash = spent_tx.hash();

        if tx_hash != spent_tx_hash {
            return Err(Error::InvalidTransactionHash);
        }

        // If this is the very first tx logged and genesis dbc_id was not
        // provided, then it becomes the genesis tx.
        let (genesis_dbc_id, genesis_blinded_amount) = &self.genesis;

        // Input amounts are not available in spentbook for genesis transaction.
        let blinded_amounts_by_input_ids: Vec<(DbcId, BlindedAmount)> =
            if input_id == genesis_dbc_id {
                vec![(*input_id, *genesis_blinded_amount)]
            } else {
                spent_tx
                    .inputs
                    .iter()
                    .map(|input| {
                        // look up matching BlindedOutput
                        let input_id = input.dbc_id();
                        let blinded_output = self.outputs_by_input_id.get(&input_id);
                        match blinded_output {
                            Some(p) => Ok((input_id, p.blinded_amount())),
                            None => Err(Error::MissingAmountForDbcId(input_id)),
                        }
                    })
                    .collect::<Result<_>>()?
            };

        // Grab all blinded amounts, grouped by input PublicKey
        // Needed for Tx verification.
        let tx_blinded_amounts: Vec<BlindedAmount> = blinded_amounts_by_input_ids
            .into_iter()
            .map(|(_, c)| c)
            .collect();

        if verify_tx {
            // Do not permit invalid tx to be logged.
            spent_tx.verify(&tx_blinded_amounts)?;
        }

        // Add dbc_id:tx_hash to dbc_id index.
        let existing_tx_hash = self.dbc_ids.entry(*input_id).or_insert_with(|| tx_hash);

        if *existing_tx_hash == tx_hash {
            // Add tx_hash:tx to transaction entries. (primary data store)
            let existing_tx = self
                .transactions
                .entry(tx_hash)
                .or_insert_with(|| spent_tx.clone());

            // Add dbc_id:blinded_output to dbc_id index.
            for output in existing_tx.outputs.iter() {
                let output_id = *output.dbc_id();
                // This will make later inputs able to find its previous
                // entry as an output, i.e. from when it was created.
                self.outputs_by_input_id
                    .entry(output_id)
                    .or_insert_with(|| output.clone());
            }

            Ok(())
        } else {
            Err(crate::mock::Error::DbcAlreadySpent.into())
        }
    }
}
