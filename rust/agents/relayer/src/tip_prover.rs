use crate::prover::{Prover, ProverError};
use abacus_core::{
    accumulator::incremental::IncrementalMerkle,
    db::{AbacusDB, DbError},
    ChainCommunicationError, CommittedMessage, SignedCheckpoint,
};
use color_eyre::eyre::Result;
use ethers::core::types::H256;
use std::fmt::Display;

use tracing::{debug, error, info, instrument};

/// Struct to update prover
pub struct MessageBatch {
    /// Messages
    pub messages: Vec<CommittedMessage>,
    current_checkpoint_index: u32,
    signed_target_checkpoint: SignedCheckpoint,
}

impl MessageBatch {
    pub fn new(
        messages: Vec<CommittedMessage>,
        current_checkpoint_index: u32,
        signed_target_checkpoint: SignedCheckpoint,
    ) -> Self {
        Self {
            messages,
            current_checkpoint_index,
            signed_target_checkpoint,
        }
    }
}

/// Struct to sync prover.
#[derive(Debug)]
pub struct TipProver {
    db: AbacusDB,
    prover: Prover,
    incremental: IncrementalMerkle,
}

impl Display for TipProver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TipProver {{ ")?;
        write!(
            f,
            "incremental: {{ root: {:?}, size: {} }}, ",
            self.incremental.root(),
            self.incremental.count()
        )?;
        write!(
            f,
            "prover: {{ root: {:?}, size: {} }} ",
            self.prover.root(),
            self.prover.count()
        )?;
        write!(f, "}}")?;
        Ok(())
    }
}

/// TipProver errors
#[derive(Debug, thiserror::Error)]
pub enum TipProverError {
    /// Local tree up-to-date but root does not match signed checkpoint"
    #[error("Local tree up-to-date but root does not match checkpoint. Local root: {prover_root}. checkpoint root: {checkpoint_root}. WARNING: this could indicate malicious validator and/or long reorganization process!")]
    MismatchedRoots {
        /// Root of prover's local merkle tree
        prover_root: H256,
        /// Root of the incremental merkle tree
        incremental_root: H256,
        /// New root contained in signed checkpoint
        checkpoint_root: H256,
    },
    /// Leaf index was not found in DB, despite batch providing messages after
    #[error("Leaf index was not found {leaf_index:?}")]
    UnavailableLeaf {
        /// Root of prover's local merkle tree
        leaf_index: u32,
    },
    /// TipProver attempts Prover operation and receives ProverError
    #[error(transparent)]
    ProverError(#[from] ProverError),
    /// TipProver receives ChainCommunicationError from chain API
    #[error(transparent)]
    ChainCommunicationError(#[from] ChainCommunicationError),
    /// DB Error
    #[error("{0}")]
    DbError(#[from] DbError),
}

impl TipProver {
    fn store_proof(&self, leaf_index: u32) -> Result<(), TipProverError> {
        match self.prover.prove(leaf_index as usize) {
            Ok(proof) => {
                self.db.store_proof(leaf_index, &proof)?;
                info!(
                    leaf_index,
                    root = ?self.prover.root(),
                    "Storing proof for leaf {}",
                    leaf_index
                );
                Ok(())
            }
            // ignore the storage request if it's out of range (e.g. leaves
            // up-to-date but no update containing leaves produced yet)
            Err(ProverError::ZeroProof { index: _, count: _ }) => Ok(()),
            // bubble up any other errors
            Err(e) => Err(e.into()),
        }
    }

    /// Given rocksdb handle `db` containing merkle tree leaves,
    /// instantiates new `TipProver` and fills prover's merkle tree
    #[instrument(level = "debug", skip(db))]
    pub fn from_disk(db: AbacusDB) -> Self {
        // Ingest all leaves in db into prover tree
        let mut prover = Prover::default();
        let mut incremental = IncrementalMerkle::default();

        if let Some(root) = db.retrieve_latest_root().expect("db error") {
            for i in 0.. {
                match db.leaf_by_leaf_index(i) {
                    Ok(Some(leaf)) => {
                        debug!(leaf_index = i, "Ingesting leaf from_disk");
                        prover.ingest(leaf).expect("!tree full");
                        incremental.ingest(leaf);
                        assert_eq!(prover.root(), incremental.root());
                        if prover.root() == root {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        error!(error = %e, "Error in TipProver::from_disk");
                        panic!("Error in TipProver::from_disk");
                    }
                }
            }
            info!(target_latest_root = ?root, root = ?incremental.root(), "Reloaded TipProver from disk");
        }

        let sync = Self {
            prover,
            incremental,
            db,
        };

        // Ensure proofs exist for all leaves
        for i in 0..sync.prover.count() as u32 {
            match (
                sync.db.leaf_by_leaf_index(i).expect("db error"),
                sync.db.proof_by_leaf_index(i).expect("db error"),
            ) {
                (Some(_), None) => sync.store_proof(i).expect("db error"),
                (None, _) => break,
                _ => {}
            }
        }

        sync
    }

    fn ingest_leaf_index(&mut self, leaf_index: u32) -> Result<(), TipProverError> {
        match self.db.leaf_by_leaf_index(leaf_index) {
            Ok(Some(leaf)) => {
                debug!(leaf_index = leaf_index, "Ingesting leaf update_from_batch");
                self.prover.ingest(leaf).expect("!tree full");
                self.incremental.ingest(leaf);
                assert_eq!(self.prover.root(), self.incremental.root());
                Ok(())
            }
            Ok(None) => {
                error!("We should not arrive here");
                Err(TipProverError::UnavailableLeaf { leaf_index })
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Update the prover with a message batch
    pub fn update_from_batch(&mut self, batch: &MessageBatch) -> Result<(), TipProverError> {
        // TODO:: If we are ahead already, something went wrong
        // if we are somehow behind the current index, prove until then

        for i in (self.prover.count() as u32)..batch.current_checkpoint_index + 1 {
            self.ingest_leaf_index(i)?;
        }

        info!(
            count = self.prover.count(),
            "update_from_batch fast forward"
        );
        // prove the until target (checkpoints are 1-indexed)
        for i in
            (batch.current_checkpoint_index + 1)..batch.signed_target_checkpoint.checkpoint.index
        {
            self.ingest_leaf_index(i)?;
        }

        let prover_root = self.prover.root();
        let incremental_root = self.incremental.root();
        let checkpoint_root = batch.signed_target_checkpoint.checkpoint.root;
        if prover_root != incremental_root || prover_root != checkpoint_root {
            return Err(TipProverError::MismatchedRoots {
                prover_root,
                incremental_root,
                checkpoint_root,
            });
        }

        info!(
            count = self.prover.count(),
            "update_from_batch batch proving"
        );
        // store proofs in DB

        for message in &batch.messages {
            self.store_proof(message.leaf_index)?;
        }
        // TODO: push proofs to S3

        Ok(())
    }
}