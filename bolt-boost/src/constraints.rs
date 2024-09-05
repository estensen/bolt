use alloy::eips::eip2718::Eip2718Error;
use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};
use tracing::error;

use super::types::{ConstraintsMessage, ConstraintsWithProofData};

/// A concurrent cache of constraints.
#[derive(Clone, Default, Debug)]
pub struct ConstraintsCache {
    cache: Arc<RwLock<HashMap<u64, Vec<ConstraintsWithProofData>>>>,
}

#[derive(Debug, thiserror::Error)]
pub enum Conflict {
    #[error("Multiple ToB constraints per slot")]
    TopOfBlock,
    #[error("Duplicate transaction in the same slot")]
    DuplicateTransaction,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Conflict(#[from] Conflict),
    #[error(transparent)]
    Decode(#[from] Eip2718Error),
}

impl ConstraintsCache {
    pub fn new() -> Self {
        Self {
            cache: Default::default(),
        }
    }

    /// Checks if the constraints for the given slot conflict with the existing constraints.
    /// Returns a [Conflict] in case of a conflict, None otherwise.
    ///
    /// # Possible conflicts
    /// - Multiple ToB constraints per slot
    /// - Duplicates of the same transaction per slot
    pub fn conflicts_with(&self, slot: &u64, constraints: &ConstraintsMessage) -> Option<Conflict> {
        if let Some(saved_constraints) = self.cache.read().get(slot) {
            for saved_constraint in saved_constraints {
                // Only 1 ToB constraint per slot
                if constraints.top && saved_constraint.message.top {
                    return Some(Conflict::TopOfBlock);
                }

                // Check if the transactions are the same
                for tx in &constraints.transactions {
                    if saved_constraint
                        .message
                        .transactions
                        .iter()
                        .any(|existing| tx == existing)
                    {
                        return Some(Conflict::DuplicateTransaction);
                    }
                }
            }
        }

        None
    }

    /// Inserts the constraints for the given slot. Also decodes the raw transactions to save their
    /// transaction hashes and hash tree roots for later use. Will first check for conflicts, and return
    /// an error if there are any.
    pub fn insert(&self, slot: u64, constraints: ConstraintsMessage) -> Result<(), Error> {
        if let Some(conflict) = self.conflicts_with(&slot, &constraints) {
            return Err(conflict.into());
        }

        let message_with_data = ConstraintsWithProofData::try_from(constraints)?;

        if let Some(cs) = self.cache.write().get_mut(&slot) {
            cs.push(message_with_data);
        } else {
            self.cache.write().insert(slot, vec![message_with_data]);
        }

        Ok(())
    }

    /// Removes all constraints before the given slot.
    pub fn remove_before(&self, slot: u64) {
        self.cache.write().retain(|k, _| *k >= slot);
    }

    /// Gets and removes the constraints for the given slot.
    pub fn remove(&self, slot: u64) -> Option<Vec<ConstraintsWithProofData>> {
        self.cache.write().remove(&slot)
    }
}
