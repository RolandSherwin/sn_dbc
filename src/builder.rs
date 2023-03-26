// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use blsttc::{PublicKey, SecretKey};
use bulletproofs::PedersenGens;
use std::{
    borrow::Borrow,
    collections::{BTreeMap, BTreeSet, HashSet},
};

use crate::transaction::{
    DbcTransaction, Output, RevealedAmount, RevealedInput, RevealedOutput, RevealedTransaction,
};
use crate::{
    rand::{CryptoRng, RngCore},
    BlindedAmount, Dbc, DbcContent, Error, Hash, OwnerOnce, Result, SpentProof,
    SpentProofKeyVerifier, SpentProofShare, Token, TransactionVerifier,
};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

pub type OutputOwnerMap = BTreeMap<PublicKey, OwnerOnce>;

/// A builder to create a DBC transaction from
/// inputs and outputs.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Default)]
pub struct TransactionBuilder {
    revealed_tx: RevealedTransaction,
    output_owner_map: OutputOwnerMap,
}

impl TransactionBuilder {
    /// add an input given an RevealedInput
    pub fn add_input(mut self, input: RevealedInput) -> Self {
        self.revealed_tx.inputs.push(input);
        self
    }

    /// add an input given an iterator over RevealedInput
    pub fn add_inputs(mut self, inputs: impl IntoIterator<Item = RevealedInput>) -> Self {
        self.revealed_tx.inputs.extend(inputs);
        self
    }

    /// add an input given a Dbc and SecretKey
    pub fn add_input_dbc(mut self, dbc: &Dbc, base_sk: &SecretKey) -> Result<Self> {
        self = self.add_input(dbc.as_revealed_input(base_sk)?);
        Ok(self)
    }

    /// add an input given a list of Dbcs and associated SecretKey and decoys
    pub fn add_inputs_dbc(
        mut self,
        dbcs: impl IntoIterator<Item = (Dbc, SecretKey)>,
    ) -> Result<Self> {
        for (dbc, base_sk) in dbcs.into_iter() {
            self = self.add_input_dbc(&dbc, &base_sk)?;
        }
        Ok(self)
    }

    /// add an input given a bearer Dbc
    pub fn add_input_dbc_bearer(mut self, dbc: &Dbc) -> Result<Self> {
        self = self.add_input(dbc.as_revealed_input_bearer()?);
        Ok(self)
    }

    /// Add an input given a list of bearer Dbcs and associated SecretKey.
    pub fn add_inputs_dbc_bearer<D>(mut self, dbcs: impl Iterator<Item = D>) -> Result<Self>
    where
        D: Borrow<Dbc>,
    {
        for dbc in dbcs {
            self = self.add_input_dbc_bearer(dbc.borrow())?;
        }
        Ok(self)
    }

    /// Add an input given a SecretKey and a RevealedAmount.
    pub fn add_input_by_secrets(
        mut self,
        secret_key: SecretKey,
        revealed_amount: RevealedAmount,
    ) -> Self {
        let revealed_input = RevealedInput::new(secret_key, revealed_amount);
        self = self.add_input(revealed_input);
        self
    }

    /// Add an input given a list of (SecretKey, RevealedAmount).
    pub fn add_inputs_by_secrets(mut self, secrets: Vec<(SecretKey, RevealedAmount)>) -> Self {
        for (secret_key, revealed_amount) in secrets.into_iter() {
            self = self.add_input_by_secrets(secret_key, revealed_amount);
        }
        self
    }

    /// add an output
    pub fn add_output(mut self, output: Output, owner: OwnerOnce) -> Self {
        self.output_owner_map.insert(output.public_key(), owner);
        self.revealed_tx.outputs.push(output);
        self
    }

    /// add a list of outputs
    pub fn add_outputs(mut self, outputs: impl IntoIterator<Item = (Output, OwnerOnce)>) -> Self {
        for (output, owner) in outputs.into_iter() {
            self = self.add_output(output, owner);
        }
        self
    }

    /// add an output by providing Token and OwnerOnce
    pub fn add_output_by_amount(mut self, amount: Token, owner: OwnerOnce) -> Self {
        let pk = owner.as_owner().public_key();
        let output = Output::new(pk, amount.as_nano());
        self.output_owner_map.insert(pk, owner);
        self.revealed_tx.outputs.push(output);
        self
    }

    /// add an output by providing iter of (Token, OwnerOnce)
    pub fn add_outputs_by_amount(
        mut self,
        outputs: impl IntoIterator<Item = (Token, OwnerOnce)>,
    ) -> Self {
        for (amount, owner) in outputs.into_iter() {
            self = self.add_output_by_amount(amount, owner);
        }
        self
    }

    /// get a list of input (true) owners
    pub fn input_owners(&self) -> Vec<PublicKey> {
        self.revealed_tx
            .inputs
            .iter()
            .map(|t| t.public_key())
            .collect()
    }

    /// get sum of input amounts
    pub fn inputs_amount_sum(&self) -> Token {
        let amount = self
            .revealed_tx
            .inputs
            .iter()
            .map(|t| t.revealed_amount.value)
            .sum();

        Token::from_nano(amount)
    }

    /// get sum of output amounts
    pub fn outputs_amount_sum(&self) -> Token {
        let amount = self.revealed_tx.outputs.iter().map(|o| o.amount).sum();
        Token::from_nano(amount)
    }

    /// get true inputs
    pub fn inputs(&self) -> &Vec<RevealedInput> {
        &self.revealed_tx.inputs
    }

    /// get outputs
    pub fn outputs(&self) -> &Vec<Output> {
        &self.revealed_tx.outputs
    }

    /// Build the DbcTransaction by signing the inputs,
    /// and generating the blinded outputs. Return a DbcBuilder.
    pub fn build(self, rng: impl RngCore + CryptoRng) -> Result<DbcBuilder> {
        let (transaction, revealed_outputs) = self.revealed_tx.sign(rng)?;

        Ok(DbcBuilder::new(
            transaction,
            revealed_outputs,
            self.output_owner_map,
            self.revealed_tx,
        ))
    }
}

/// A Builder for aggregating SpentProofs and generating the final Dbc outputs.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone)]
pub struct DbcBuilder {
    pub transaction: DbcTransaction,
    pub revealed_outputs: Vec<RevealedOutput>,
    pub output_owner_map: OutputOwnerMap,
    pub revealed_tx: RevealedTransaction,
    pub spent_proofs: HashSet<SpentProof>,
    pub spent_proof_shares: BTreeMap<PublicKey, HashSet<SpentProofShare>>,
    pub spent_transactions: BTreeMap<Hash, DbcTransaction>,
}

impl DbcBuilder {
    /// Create a new DbcBuilder
    pub fn new(
        transaction: DbcTransaction,
        revealed_outputs: Vec<RevealedOutput>,
        output_owner_map: OutputOwnerMap,
        revealed_tx: RevealedTransaction,
    ) -> Self {
        Self {
            transaction,
            revealed_outputs,
            output_owner_map,
            revealed_tx,
            spent_proofs: Default::default(),
            spent_proof_shares: Default::default(),
            spent_transactions: Default::default(),
        }
    }

    /// returns Vec of public keys and tx intended for use as inputs
    /// to Spendbook::log_spent().
    pub fn inputs(&self) -> Vec<(PublicKey, DbcTransaction)> {
        self.transaction
            .inputs
            .iter()
            .map(|input| (input.public_key(), self.transaction.clone()))
            .collect()
    }

    /// Add a SpentProof.
    pub fn add_spent_proof(mut self, proof: SpentProof) -> Self {
        let _ = self.spent_proofs.insert(proof);
        self
    }

    /// Add a SpentProofShare for the given input index
    pub fn add_spent_proof_share(mut self, share: SpentProofShare) -> Self {
        let shares = self
            .spent_proof_shares
            .entry(*share.public_key())
            .or_default();
        shares.insert(share);
        self
    }

    /// Add a list of SpentProofShare for the given input index
    pub fn add_spent_proof_shares(
        mut self,
        shares: impl IntoIterator<Item = SpentProofShare>,
    ) -> Self {
        for share in shares.into_iter() {
            self = self.add_spent_proof_share(share);
        }
        self
    }

    /// Add a transaction which spent one of the inputs
    pub fn add_spent_transaction(mut self, spent_tx: DbcTransaction) -> Self {
        let tx_hash = Hash::from(spent_tx.hash());
        self.spent_transactions
            .entry(tx_hash)
            .or_insert_with(|| spent_tx);
        self
    }

    /// Build the output DBCs, verifying the transaction and spentproofs.
    ///
    /// see TransactionVerifier::verify() for a description of
    /// verifier requirements.
    pub fn build<K: SpentProofKeyVerifier>(
        self,
        verifier: &K,
    ) -> Result<Vec<(Dbc, OwnerOnce, RevealedAmount)>> {
        let spent_proofs = self.spent_proofs()?;

        // verify the Tx, along with spent proofs.
        // note that we do this just once for entire Tx, not once per output Dbc.
        TransactionVerifier::verify(verifier, &self.transaction, &spent_proofs)?;

        // verify there is a matching spent transaction for each spent_proof
        if !spent_proofs.iter().all(|proof| {
            self.spent_transactions
                .contains_key(&proof.transaction_hash())
        }) {
            return Err(Error::MissingSpentTransaction);
        }

        // build output DBCs
        self.build_output_dbcs(spent_proofs)
    }

    /// Build the output DBCs (no verification over Tx or spentproof is performed).
    pub fn build_without_verifying(self) -> Result<Vec<(Dbc, OwnerOnce, RevealedAmount)>> {
        let spent_proofs = self.spent_proofs()?;
        self.build_output_dbcs(spent_proofs)
    }

    // Private helper to build output DBCs
    fn build_output_dbcs(
        self,
        spent_proofs: BTreeSet<SpentProof>,
    ) -> Result<Vec<(Dbc, OwnerOnce, RevealedAmount)>> {
        let pc_gens = PedersenGens::default();
        let output_blinded_and_revealed_amounts: Vec<(BlindedAmount, RevealedAmount)> = self
            .revealed_outputs
            .iter()
            .map(|output| output.revealed_amount)
            .map(|r| (r.blinded_amount(&pc_gens), r))
            .collect();

        let owner_once_list: Vec<&OwnerOnce> = self
            .transaction
            .outputs
            .iter()
            .map(|output| {
                self.output_owner_map
                    .get(output.public_key())
                    .ok_or(Error::PublicKeyNotFound)
            })
            .collect::<Result<_>>()?;

        // Form the final output DBCs
        let output_dbcs: Vec<(Dbc, OwnerOnce, RevealedAmount)> = self
            .transaction
            .outputs
            .iter()
            .zip(owner_once_list)
            .map(|(output, owner_once)| {
                let revealed_amounts: Vec<RevealedAmount> = output_blinded_and_revealed_amounts
                    .iter()
                    .filter(|(c, _)| *c == output.blinded_amount())
                    .map(|(_, r)| *r)
                    .collect();
                assert_eq!(revealed_amounts.len(), 1);

                let content = DbcContent::from((
                    owner_once.owner_base.clone(),
                    owner_once.derivation_index,
                    revealed_amounts[0],
                ));
                let public_key = owner_once
                    .owner_base
                    .derive(&owner_once.derivation_index)
                    .public_key();
                let dbc = Dbc {
                    content,
                    public_key,
                    transaction: self.transaction.clone(),
                    inputs_spent_proofs: spent_proofs.clone(),
                    inputs_spent_transactions: self.spent_transactions.values().cloned().collect(),
                };
                (dbc, owner_once.clone(), revealed_amounts[0])
            })
            .collect();

        Ok(output_dbcs)
    }

    /// build spent proofs from shares.
    pub fn spent_proofs(&self) -> Result<BTreeSet<SpentProof>> {
        let transaction_hash = Hash::from(self.transaction.hash());
        let spent_proofs: BTreeSet<SpentProof> = self
            .spent_proof_shares
            .iter()
            .map(|(public_key, shares)| {
                SpentProof::try_from_proof_shares(*public_key, transaction_hash, shares)
            })
            .chain(self.spent_proofs.iter().cloned().map(Ok))
            .collect::<Result<_>>()?;

        Ok(spent_proofs)
    }
}
