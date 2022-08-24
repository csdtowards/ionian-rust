use crate::log_store::flow_store::{FlowConfig, FlowStore};
use crate::log_store::tx_store::TransactionStore;
use crate::log_store::{
    FlowRead, FlowWrite, LogStoreChunkRead, LogStoreChunkWrite, LogStoreRead, LogStoreWrite,
};
use crate::try_option;
use anyhow::{anyhow, bail, Result};
use append_merkle::{Algorithm, AppendMerkleTree, Sha3Algorithm};
use ethereum_types::H256;
use kvdb_rocksdb::{Database, DatabaseConfig};
use merkle_light::merkle::{log2_pow2, MerkleTree};
use merkle_tree::RawLeafSha3Algorithm;
use rayon::iter::ParallelIterator;
use rayon::prelude::ParallelSlice;
use shared_types::{
    Chunk, ChunkArray, ChunkArrayWithProof, ChunkWithProof, DataRoot, FlowProof, FlowRangeProof,
    Transaction,
};
use std::path::Path;
use std::sync::Arc;

/// 256 Bytes
pub const ENTRY_SIZE: usize = 256;
/// 1024 Entries.
pub const PORA_CHUNK_SIZE: usize = 1024;

pub const COL_TX: u32 = 0;
pub const COL_ENTRY_BATCH: u32 = 1;
pub const COL_TX_DATA_ROOT_INDEX: u32 = 2;
pub const COL_ENTRY_BATCH_ROOT: u32 = 3;
pub const COL_TX_COMPLETED: u32 = 4;
pub const COL_NUM: u32 = 5;

type Merkle = AppendMerkleTree<H256, Sha3Algorithm>;

pub struct LogManager {
    tx_store: TransactionStore,
    flow_store: FlowStore,
    // TODO(zz): Refactor the in-memory merkle and in-disk storage together.
    pora_chunks_merkle: Merkle,
    /// The in-memory structure of the sub merkle tree of the last chunk.
    /// The size is always less than `PORA_CHUNK_SIZE`.
    last_chunk_merkle: Merkle,
}

#[derive(Clone, Default)]
pub struct LogConfig {
    pub flow: FlowConfig,
}

impl LogStoreChunkWrite for LogManager {
    fn put_chunks(&mut self, tx_seq: u64, chunks: ChunkArray) -> Result<()> {
        let tx = self
            .tx_store
            .get_tx_by_seq_number(tx_seq)?
            .ok_or_else(|| anyhow!("put chunks with missing tx: tx_seq={}", tx_seq))?;
        if chunks.start_index.saturating_mul(ENTRY_SIZE as u64) + chunks.data.len() as u64 > tx.size
        {
            bail!(
                "put chunks with data out of tx range: tx_seq={} start_index={} data_len={}",
                tx_seq,
                chunks.start_index,
                chunks.data.len()
            );
        }
        // TODO: Use another struct to avoid confusion.
        let mut flow_entry_array = chunks;
        flow_entry_array.start_index += tx.start_entry_index;
        self.append_entries(flow_entry_array)?;
        Ok(())
    }

    fn remove_all_chunks(&self, _tx_seq: u64) -> crate::error::Result<()> {
        todo!()
    }
}

impl LogStoreWrite for LogManager {
    fn put_tx(&mut self, tx: Transaction) -> Result<()> {
        // TODO(zz): Should we validate received tx?
        self.append_subtree_list(tx.merkle_nodes.clone())?;
        self.tx_store.put_tx(tx)?;
        // TODO(zz): This assumes that transactions are inserted in order.
        // Double check if this always holds.
        self.pora_chunks_merkle.commit();
        Ok(())
    }

    fn finalize_tx(&self, tx_seq: u64) -> Result<()> {
        let tx = self
            .tx_store
            .get_tx_by_seq_number(tx_seq)?
            .ok_or_else(|| anyhow!("finalize_tx with tx missing: tx_seq={}", tx_seq))?;
        let tx_end_index = tx.start_entry_index + bytes_to_entries(tx.size);
        // TODO: Check completeness without loading all data in memory.
        // TODO: Should we double check the tx merkle root?
        if self
            .flow_store
            .get_entries(tx.start_entry_index, tx_end_index)?
            .is_some()
        {
            self.tx_store.finalize_tx(tx_seq)
        } else {
            bail!("finalize tx with data missing: tx_seq={}", tx_seq)
        }
    }
}

impl LogStoreChunkRead for LogManager {
    fn get_chunk_by_tx_and_index(
        &self,
        tx_seq: u64,
        index: usize,
    ) -> crate::error::Result<Option<Chunk>> {
        // TODO(zz): This is not needed?
        let single_chunk_array =
            try_option!(self.get_chunks_by_tx_and_index_range(tx_seq, index, index + 1)?);
        Ok(Some(Chunk(single_chunk_array.data.as_slice().try_into()?)))
    }

    fn get_chunks_by_tx_and_index_range(
        &self,
        tx_seq: u64,
        index_start: usize,
        index_end: usize,
    ) -> crate::error::Result<Option<ChunkArray>> {
        let tx = try_option!(self.get_tx_by_seq_number(tx_seq)?);
        let start_flow_index = tx.start_entry_index + index_start as u64;
        let end_flow_index = tx.start_entry_index + index_end as u64;
        // TODO: Use another struct.
        // Set returned chunk start index as the offset in the tx data.
        let mut tx_chunk = try_option!(self
            .flow_store
            .get_entries(start_flow_index, end_flow_index)?);
        tx_chunk.start_index -= tx.start_entry_index;
        Ok(Some(tx_chunk))
    }

    fn get_chunk_by_data_root_and_index(
        &self,
        _data_root: &DataRoot,
        _index: usize,
    ) -> crate::error::Result<Option<Chunk>> {
        todo!()
    }

    fn get_chunks_by_data_root_and_index_range(
        &self,
        data_root: &DataRoot,
        index_start: usize,
        index_end: usize,
    ) -> crate::error::Result<Option<ChunkArray>> {
        let tx_seq = try_option!(self.get_tx_seq_by_data_root(data_root)?);
        self.get_chunks_by_tx_and_index_range(tx_seq, index_start, index_end)
    }

    fn get_chunk_index_list(&self, _tx_seq: u64) -> crate::error::Result<Vec<usize>> {
        todo!()
    }
}

impl LogStoreRead for LogManager {
    fn get_tx_by_seq_number(&self, seq: u64) -> crate::error::Result<Option<Transaction>> {
        self.tx_store.get_tx_by_seq_number(seq)
    }

    fn get_tx_seq_by_data_root(&self, data_root: &DataRoot) -> crate::error::Result<Option<u64>> {
        self.tx_store.get_tx_seq_by_data_root(data_root)
    }

    fn get_chunk_with_proof_by_tx_and_index(
        &self,
        tx_seq: u64,
        index: usize,
    ) -> crate::error::Result<Option<ChunkWithProof>> {
        // TODO(zz): Optimize for mining.
        let single_chunk_array = try_option!(self.get_chunks_with_proof_by_tx_and_index_range(
            tx_seq,
            index,
            index + 1
        )?);
        Ok(Some(ChunkWithProof {
            chunk: Chunk(single_chunk_array.chunks.data.as_slice().try_into()?),
            proof: single_chunk_array.proof.left_proof,
        }))
    }

    fn get_chunks_with_proof_by_tx_and_index_range(
        &self,
        tx_seq: u64,
        index_start: usize,
        index_end: usize,
    ) -> crate::error::Result<Option<ChunkArrayWithProof>> {
        let tx = try_option!(self.tx_store.get_tx_by_seq_number(tx_seq)?);
        let chunks =
            try_option!(self.get_chunks_by_tx_and_index_range(tx_seq, index_start, index_end)?);
        let left_proof = self.gen_proof(tx.start_entry_index + index_start as u64)?;
        let right_proof = self.gen_proof(tx.start_entry_index + index_end as u64 - 1)?;
        Ok(Some(ChunkArrayWithProof {
            chunks,
            proof: FlowRangeProof {
                left_proof,
                right_proof,
            },
        }))
    }

    fn check_tx_completed(&self, tx_seq: u64) -> crate::error::Result<bool> {
        self.tx_store.check_tx_completed(tx_seq)
    }

    fn next_tx_seq(&self) -> crate::error::Result<u64> {
        self.tx_store.next_tx_seq()
    }

    fn validate_range_proof(&self, tx_seq: u64, data: &ChunkArrayWithProof) -> Result<bool> {
        let tx = self
            .get_tx_by_seq_number(tx_seq)?
            .ok_or_else(|| anyhow!("tx missing"))?;
        let leaves = data_to_merkle_leaves(&data.chunks.data)?;
        data.proof.validate::<Sha3Algorithm>(
            &leaves,
            (data.chunks.start_index + tx.start_entry_index) as usize,
        )?;
        Ok(self.pora_chunks_merkle.check_root(&data.proof.root()))
    }
}

impl LogManager {
    #[allow(unused)]
    pub fn rocksdb(config: LogConfig, path: impl AsRef<Path>) -> Result<Self> {
        let mut db_config = DatabaseConfig::with_columns(COL_NUM);
        db_config.enable_statistics = true;
        let db = Arc::new(Database::open(&db_config, path)?);
        let tx_store = TransactionStore::new(db.clone());
        let flow_store = FlowStore::new(db, config.flow);
        // FIXME(zz): Recovery with incomplete data has not been handled.
        let chunk_roots = flow_store.get_chunk_root_list()?;
        let last_chunk_data = flow_store.get_entries_to_end(
            (chunk_roots.len() * PORA_CHUNK_SIZE) as u64,
            ((chunk_roots.len() + 1) * PORA_CHUNK_SIZE) as u64,
        )?;
        let mut pora_chunks_merkle = Merkle::new(chunk_roots);
        let last_chunk_leaves = data_to_merkle_leaves(&last_chunk_data.data)?;
        let last_chunk_merkle = if pora_chunks_merkle.leaves() >= 1 {
            Merkle::new_with_depth(last_chunk_leaves, log2_pow2(PORA_CHUNK_SIZE) + 1)
        } else {
            Merkle::new(last_chunk_leaves)
        };
        if last_chunk_merkle.leaves() != 0 {
            pora_chunks_merkle.append(*last_chunk_merkle.root());
        }
        let mut log_manager = Self {
            tx_store,
            flow_store,
            pora_chunks_merkle,
            last_chunk_merkle,
        };
        log_manager.try_initialize();
        Ok(log_manager)
    }

    #[allow(unused)]
    pub fn memorydb(config: LogConfig) -> Result<Self> {
        let db = Arc::new(kvdb_memorydb::create(COL_NUM));
        let tx_store = TransactionStore::new(db.clone());
        let flow_store = FlowStore::new(db, config.flow);
        let chunk_roots = flow_store.get_chunk_root_list()?;
        let last_chunk_data = flow_store.get_entries_to_end(
            (chunk_roots.len() * PORA_CHUNK_SIZE) as u64,
            ((chunk_roots.len() + 1) * PORA_CHUNK_SIZE) as u64,
        )?;
        debug!(
            "Load {} chunk roots and {} last chunk entries",
            chunk_roots.len(),
            last_chunk_data.data.len() / ENTRY_SIZE
        );
        let mut pora_chunks_merkle = Merkle::new(chunk_roots);
        let last_chunk_leaves = data_to_merkle_leaves(&last_chunk_data.data)?;
        let last_chunk_merkle = if pora_chunks_merkle.leaves() >= 1 {
            Merkle::new_with_depth(last_chunk_leaves, log2_pow2(PORA_CHUNK_SIZE) + 1)
        } else {
            Merkle::new(last_chunk_leaves)
        };
        if last_chunk_merkle.leaves() != 0 {
            pora_chunks_merkle.append(*last_chunk_merkle.root());
        }
        let mut log_manager = Self {
            tx_store,
            flow_store,
            pora_chunks_merkle,
            last_chunk_merkle,
        };
        log_manager.try_initialize();
        Ok(log_manager)
    }

    fn try_initialize(&mut self) {
        if self.pora_chunks_merkle.leaves() == 0 && self.last_chunk_merkle.leaves() == 0 {
            self.last_chunk_merkle.append(H256::zero());
            self.pora_chunks_merkle
                .update_last(*self.last_chunk_merkle.root());
        }
    }

    fn gen_proof(&self, flow_index: u64) -> Result<FlowProof> {
        let chunk_index = flow_index / PORA_CHUNK_SIZE as u64;
        // FIXME(zz): for the last chunk which is not full, its root in `chunk_merkle` may not be
        // in sync with the data in `flow store`. We may need lock for this case.
        let top_proof = self.pora_chunks_merkle.gen_proof(chunk_index as usize)?;

        // TODO(zz): Maybe we can decide that all proofs are at the PoRA chunk level, so
        // we do not need to maintain the proof at the entry level below.
        let sub_proof = if chunk_index as usize != self.pora_chunks_merkle.leaves() - 1 {
            // TODO(zz）: Even if the data is incomplete, given the intermediate merkle roots
            // it's still possible to generate needed proofs. These merkle roots may be stored
            // within `EntryBatch::Incomplete`.
            let pora_chunk = self
                .flow_store
                .get_entries(
                    chunk_index * PORA_CHUNK_SIZE as u64,
                    (chunk_index + 1) * PORA_CHUNK_SIZE as u64,
                )?
                .ok_or_else(|| {
                    anyhow!(
                        "data incomplete for generating proof of index {}",
                        flow_index
                    )
                })?;
            let leaves = data_to_merkle_leaves(&pora_chunk.data)?;
            let chunk_merkle = Merkle::new_with_depth(leaves, log2_pow2(PORA_CHUNK_SIZE) + 1);
            chunk_merkle.gen_proof(flow_index as usize % PORA_CHUNK_SIZE)?
        } else {
            self.last_chunk_merkle
                .gen_proof(flow_index as usize % PORA_CHUNK_SIZE)?
        };
        entry_proof(&top_proof, &sub_proof)
    }

    fn append_subtree_list(&mut self, merkle_list: Vec<(usize, DataRoot)>) -> Result<()> {
        if merkle_list.is_empty() {
            return Ok(());
        }

        self.pad_tx(1 << (merkle_list[0].0 - 1))?;
        for (subtree_depth, subtree_root) in merkle_list {
            let subtree_size = 1 << (subtree_depth - 1);
            if self.last_chunk_merkle.leaves() == 0 && subtree_size == PORA_CHUNK_SIZE {
                self.pora_chunks_merkle.append_subtree(1, subtree_root)?;
                self.flow_store.put_batch_root(
                    (self.pora_chunks_merkle.leaves() - 1) as u64,
                    subtree_root,
                    1,
                )?;
            } else if self.last_chunk_merkle.leaves() + subtree_size <= PORA_CHUNK_SIZE {
                self.last_chunk_merkle
                    .append_subtree(subtree_depth, subtree_root)?;
                if self.last_chunk_merkle.leaves() == subtree_size {
                    // `last_chunk_merkle` was empty, so this is a new leaf in the top_tree.
                    self.pora_chunks_merkle
                        .append_subtree(1, *self.last_chunk_merkle.root())?;
                } else {
                    self.pora_chunks_merkle
                        .update_last(*self.last_chunk_merkle.root());
                }
                if self.last_chunk_merkle.leaves() == PORA_CHUNK_SIZE {
                    self.flow_store.put_batch_root(
                        (self.pora_chunks_merkle.leaves() - 1) as u64,
                        *self.last_chunk_merkle.root(),
                        1,
                    )?;
                    self.last_chunk_merkle =
                        Merkle::new_with_depth(vec![], log2_pow2(PORA_CHUNK_SIZE) + 1);
                }
            } else {
                // `last_chunk_merkle` has been padded here, so a subtree should not be across
                // the chunks boundary.
                assert_eq!(self.last_chunk_merkle.leaves(), 0);
                assert!(subtree_size >= PORA_CHUNK_SIZE);
                self.pora_chunks_merkle
                    .append_subtree(subtree_depth - log2_pow2(PORA_CHUNK_SIZE), subtree_root)?;
                self.flow_store.put_batch_root(
                    (self.pora_chunks_merkle.leaves() - 1) as u64,
                    subtree_root,
                    subtree_size / PORA_CHUNK_SIZE,
                )?;
            }
        }
        Ok(())
    }

    fn pad_tx(&mut self, first_subtree_size: u64) -> Result<()> {
        // Check if we need to pad the flow.
        let tx_start_flow_index = if self.pora_chunks_merkle.leaves() != 0 {
            (self.pora_chunks_merkle.leaves() - 1) as u64 * PORA_CHUNK_SIZE as u64
                + self.last_chunk_merkle.leaves() as u64
        } else {
            assert_eq!(self.last_chunk_merkle.leaves(), 0);
            0
        };
        let extra = tx_start_flow_index % first_subtree_size;
        if extra != 0 {
            let pad_data = Self::padding((first_subtree_size - extra) as usize);
            let last_chunk_pad = (PORA_CHUNK_SIZE - self.last_chunk_merkle.leaves()) * ENTRY_SIZE;
            if pad_data.len() < last_chunk_pad {
                self.last_chunk_merkle
                    .append_list(data_to_merkle_leaves(&pad_data)?);
                self.pora_chunks_merkle
                    .update_last(*self.last_chunk_merkle.root());
                self.flow_store.append_entries(ChunkArray {
                    data: pad_data,
                    start_index: tx_start_flow_index,
                })?;
            } else {
                self.last_chunk_merkle
                    .append_list(data_to_merkle_leaves(&pad_data[..last_chunk_pad])?);
                self.pora_chunks_merkle
                    .update_last(*self.last_chunk_merkle.root());
                self.flow_store.append_entries(ChunkArray {
                    data: pad_data[..last_chunk_pad].to_vec(),
                    start_index: tx_start_flow_index as u64,
                })?;

                self.last_chunk_merkle =
                    Merkle::new_with_depth(vec![], log2_pow2(PORA_CHUNK_SIZE) + 1);
                let mut start_index = last_chunk_pad / ENTRY_SIZE;

                // Pad with more complete chunks.
                while pad_data.len() >= (start_index + PORA_CHUNK_SIZE) * ENTRY_SIZE {
                    let data = pad_data
                        [start_index * ENTRY_SIZE..(start_index + PORA_CHUNK_SIZE) * ENTRY_SIZE]
                        .to_vec();
                    self.pora_chunks_merkle
                        .append(*Merkle::new(data_to_merkle_leaves(&data)?).root());
                    self.flow_store.append_entries(ChunkArray {
                        data,
                        start_index: start_index as u64 + tx_start_flow_index,
                    })?;
                    start_index += PORA_CHUNK_SIZE;
                }
                assert_eq!(pad_data.len(), start_index * ENTRY_SIZE);
            }
        }
        Ok(())
    }

    fn append_entries(&mut self, flow_entry_array: ChunkArray) -> Result<()> {
        if flow_entry_array.start_index >= self.last_chunk_start_index() {
            // Update `last_chunk_merkle` with real data.
            let chunk_start_index =
                (flow_entry_array.start_index - self.last_chunk_start_index()) as usize;
            for (local_index, entry) in flow_entry_array.data[..self
                .last_chunk_merkle
                .leaves()
                .saturating_sub(chunk_start_index)
                * ENTRY_SIZE]
                .chunks_exact(ENTRY_SIZE)
                .enumerate()
            {
                self.last_chunk_merkle
                    .fill_leaf(chunk_start_index + local_index, Sha3Algorithm::leaf(entry));
            }
        }
        let chunk_roots = self.flow_store.append_entries(flow_entry_array)?;
        for (chunk_index, chunk_root) in chunk_roots {
            if chunk_index < self.pora_chunks_merkle.leaves() as u64 - 1 {
                self.pora_chunks_merkle
                    .fill_leaf(chunk_index as usize, chunk_root);
            } else {
                // TODO(zz): This assumption may be false in the future.
                unreachable!("We always insert tx nodes before put_chunks");
            }
        }
        Ok(())
    }

    // FIXME(zz): Implement padding.
    pub fn padding(len: usize) -> Vec<u8> {
        vec![0; len * ENTRY_SIZE]
    }

    fn last_chunk_start_index(&self) -> u64 {
        if self.pora_chunks_merkle.leaves() == 0 {
            0
        } else {
            PORA_CHUNK_SIZE as u64
                * if self.last_chunk_merkle.leaves() == 0 {
                    // The last chunk is empty and its root hash is not in `pora_chunk_merkle`,
                    // so all chunks in `pora_chunk_merkle` is complete.
                    self.pora_chunks_merkle.leaves()
                } else {
                    // The last chunk has data, so we need to exclude it from `pora_chunks_merkle`.
                    self.pora_chunks_merkle.leaves() - 1
                } as u64
        }
    }
}

/// This represents the subtree of a chunk or the whole data merkle tree.
pub type FileMerkleTree = MerkleTree<[u8; 32], RawLeafSha3Algorithm>;

#[macro_export]
macro_rules! try_option {
    ($r: ident) => {
        match $r {
            Some(v) => v,
            None => return Ok(None),
        }
    };
    ($e: expr) => {
        match $e {
            Some(v) => v,
            None => return Ok(None),
        }
    };
}

/// This should be called with input checked.
pub fn sub_merkle_tree(leaf_data: &[u8]) -> Result<FileMerkleTree> {
    Ok(FileMerkleTree::new(
        data_to_merkle_leaves(leaf_data)?
            .into_iter()
            .map(|h| h.0)
            .collect::<Vec<[u8; 32]>>(),
    ))
}

pub fn data_to_merkle_leaves(leaf_data: &[u8]) -> Result<Vec<H256>> {
    if leaf_data.len() % ENTRY_SIZE != 0 {
        bail!("merkle_tree: unmatch data size");
    }
    Ok(leaf_data
        .par_chunks_exact(ENTRY_SIZE)
        .map(Sha3Algorithm::leaf)
        .collect())
}

pub fn bytes_to_entries(size_bytes: u64) -> u64 {
    if size_bytes % ENTRY_SIZE as u64 == 0 {
        size_bytes / ENTRY_SIZE as u64
    } else {
        size_bytes / ENTRY_SIZE as u64 + 1
    }
}

fn entry_proof(top_proof: &FlowProof, sub_proof: &FlowProof) -> Result<FlowProof> {
    if top_proof.item() != sub_proof.root() {
        bail!(
            "top tree and sub tree mismatch: top_leaf={:?}, sub_root={:?}",
            top_proof.item(),
            sub_proof.root()
        );
    }
    let mut lemma = sub_proof.lemma().to_vec();
    let mut path = sub_proof.path().to_vec();
    assert!(lemma.pop().is_some());
    lemma.extend_from_slice(&top_proof.lemma()[1..]);
    path.extend_from_slice(top_proof.path());
    Ok(FlowProof::new(lemma, path))
}