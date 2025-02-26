// Copyright (c) 2023, MaidSafe.
// All rights reserved.
//
// This SAFE Network Software is licensed under the BSD-3-Clause license.
// Please see the LICENSE file for more details.

use blsttc::Signature;
use bulletproofs::PedersenGens;

#[cfg(feature = "serde")]
use serde::{self, Deserialize, Serialize};

use super::{Error, Result, RevealedAmount};
use crate::{BlindedAmount, DbcId, DerivedKey};

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone)]
pub struct RevealedInput {
    #[cfg_attr(feature = "serde", serde(skip_serializing))]
    pub derived_key: DerivedKey,
    pub revealed_amount: RevealedAmount,
}

impl RevealedInput {
    pub fn new(derived_key: DerivedKey, revealed_amount: RevealedAmount) -> Self {
        Self {
            derived_key,
            revealed_amount,
        }
    }

    pub fn dbc_id(&self) -> DbcId {
        self.derived_key.dbc_id()
    }

    pub fn revealed_amount(&self) -> &RevealedAmount {
        &self.revealed_amount
    }

    pub fn blinded_amount(&self, pc_gens: &PedersenGens) -> BlindedAmount {
        self.revealed_amount.blinded_amount(pc_gens)
    }

    pub fn sign(&self, msg: &[u8], pc_gens: &PedersenGens) -> BlindedInput {
        BlindedInput {
            dbc_id: self.dbc_id(),
            blinded_amount: self.blinded_amount(pc_gens),
            signature: self.derived_key.sign(msg),
        }
    }
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Eq, PartialEq, Debug, Clone)]
pub struct BlindedInput {
    pub dbc_id: DbcId,
    pub blinded_amount: BlindedAmount,
    /// This is the signature of the `DerivedKey`
    /// corresponding to this `dbc_id`
    pub signature: Signature,
}

impl BlindedInput {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v: Vec<u8> = Default::default();
        v.extend(self.dbc_id.to_bytes().as_ref());
        v.extend(self.blinded_amount.compress().as_bytes());
        v.extend(self.signature.to_bytes().as_ref());
        v
    }

    pub fn dbc_id(&self) -> DbcId {
        self.dbc_id
    }

    /// Verify if a blinded amount you know of, is the same as the one in the input,
    /// and that the bytes passed in, are what the signature of this input was made over,
    /// and that the public key of this input was the signer.
    pub fn verify(&self, msg: &[u8], blinded_amount: BlindedAmount) -> Result<()> {
        if self.blinded_amount != blinded_amount {
            return Err(Error::InvalidInputBlindedAmount);
        }

        // check the signature
        if !self.dbc_id.verify(&self.signature, msg) {
            return Err(Error::InvalidSignature);
        }

        Ok(())
    }
}
