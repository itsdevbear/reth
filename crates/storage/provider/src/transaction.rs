use itertools::{izip, Itertools};
use reth_db::{
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO},
    database::{Database, DatabaseGAT},
    models::{
        sharded_key,
        storage_sharded_key::{self, StorageShardedKey},
        ShardedKey, StoredBlockBody, TransitionIdAddress,
    },
    table::{KeyValue, Table},
    tables,
    transaction::{DbTx, DbTxMut},
    TransitionList,
};
use reth_interfaces::{db::Error as DbError, provider::ProviderError};
use reth_primitives::{
    keccak256, Account, Address, BlockHash, BlockNumber, ChainSpec, Hardfork, Header, Receipt,
    SealedBlock, SealedBlockWithSenders, StorageEntry, TransactionSignedEcRecovered, TransitionId,
    TxNumber, H256, U256,
};
use reth_tracing::tracing::{info, trace};
use std::{
    collections::{hash_map::Entry, BTreeMap, BTreeSet, HashMap},
    fmt::Debug,
    ops::{Deref, DerefMut, RangeBounds},
};

use crate::{
    execution_result::{AccountInfoChangeSet, TransactionChangeSet},
    insert_canonical_block,
    trie::{DBTrieLoader, TrieError},
};

use crate::execution_result::{AccountChangeSet, ExecutionResult};

/// A container for any DB transaction that will open a new inner transaction when the current
/// one is committed.
// NOTE: This container is needed since `Transaction::commit` takes `mut self`, so methods in
// the pipeline that just take a reference will not be able to commit their transaction and let
// the pipeline continue. Is there a better way to do this?
//
// TODO: Re-evaluate if this is actually needed, this was introduced as a way to manage the
// lifetime of the `TXMut` and having a nice API for re-opening a new transaction after `commit`
pub struct Transaction<'this, DB: Database> {
    /// A handle to the DB.
    pub(crate) db: &'this DB,
    tx: Option<<DB as DatabaseGAT<'this>>::TXMut>,
}

impl<'a, DB: Database> Debug for Transaction<'a, DB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction").finish()
    }
}

impl<'a, DB: Database> Deref for Transaction<'a, DB> {
    type Target = <DB as DatabaseGAT<'a>>::TXMut;

    /// Dereference as the inner transaction.
    ///
    /// # Panics
    ///
    /// Panics if an inner transaction does not exist. This should never be the case unless
    /// [Transaction::close] was called without following up with a call to [Transaction::open].
    fn deref(&self) -> &Self::Target {
        self.tx.as_ref().expect("Tried getting a reference to a non-existent transaction")
    }
}

impl<'a, DB: Database> DerefMut for Transaction<'a, DB> {
    /// Dereference as a mutable reference to the inner transaction.
    ///
    /// # Panics
    ///
    /// Panics if an inner transaction does not exist. This should never be the case unless
    /// [Transaction::close] was called without following up with a call to [Transaction::open].
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.tx.as_mut().expect("Tried getting a mutable reference to a non-existent transaction")
    }
}

impl<'this, DB> Transaction<'this, DB>
where
    DB: Database,
{
    /// Create a new container with the given database handle.
    ///
    /// A new inner transaction will be opened.
    pub fn new(db: &'this DB) -> Result<Self, DbError> {
        Ok(Self { db, tx: Some(db.tx_mut()?) })
    }

    /// Creates a new container with given database and transaction handles.
    pub fn new_raw(db: &'this DB, tx: <DB as DatabaseGAT<'this>>::TXMut) -> Self {
        Self { db, tx: Some(tx) }
    }

    /// Accessor to the internal Database
    pub fn inner(&self) -> &'this DB {
        self.db
    }

    /// Commit the current inner transaction and open a new one.
    ///
    /// # Panics
    ///
    /// Panics if an inner transaction does not exist. This should never be the case unless
    /// [Transaction::close] was called without following up with a call to [Transaction::open].
    pub fn commit(&mut self) -> Result<bool, DbError> {
        let success = if let Some(tx) = self.tx.take() { tx.commit()? } else { false };
        self.tx = Some(self.db.tx_mut()?);
        Ok(success)
    }

    /// Drops the current inner transaction and open a new one.
    pub fn drop(&mut self) -> Result<(), DbError> {
        if let Some(tx) = self.tx.take() {
            drop(tx);
        }

        self.tx = Some(self.db.tx_mut()?);

        Ok(())
    }

    /// Open a new inner transaction.
    pub fn open(&mut self) -> Result<(), DbError> {
        self.tx = Some(self.db.tx_mut()?);
        Ok(())
    }

    /// Close the current inner transaction.
    pub fn close(&mut self) {
        self.tx.take();
    }

    /// Query [tables::CanonicalHeaders] table for block hash by block number
    pub fn get_block_hash(&self, block_number: BlockNumber) -> Result<BlockHash, TransactionError> {
        let hash = self
            .get::<tables::CanonicalHeaders>(block_number)?
            .ok_or(ProviderError::CanonicalHeader { block_number })?;
        Ok(hash)
    }

    /// Query the block body by number.
    pub fn get_block_body(&self, number: BlockNumber) -> Result<StoredBlockBody, TransactionError> {
        let body =
            self.get::<tables::BlockBodies>(number)?.ok_or(ProviderError::BlockBody { number })?;
        Ok(body)
    }

    /// Query the last transition of the block by [BlockNumber] key
    pub fn get_block_transition(&self, key: BlockNumber) -> Result<TransitionId, TransactionError> {
        let last_transition_id = self
            .get::<tables::BlockTransitionIndex>(key)?
            .ok_or(ProviderError::BlockTransition { block_number: key })?;
        Ok(last_transition_id)
    }

    /// Get the next start transaction id and transition for the `block` by looking at the previous
    /// block. Returns Zero/Zero for Genesis.
    pub fn get_next_block_ids(
        &self,
        block: BlockNumber,
    ) -> Result<(TxNumber, TransitionId), TransactionError> {
        if block == 0 {
            return Ok((0, 0))
        }

        let prev_number = block - 1;
        let prev_body = self.get_block_body(prev_number)?;
        let last_transition = self
            .get::<tables::BlockTransitionIndex>(prev_number)?
            .ok_or(ProviderError::BlockTransition { block_number: prev_number })?;
        Ok((prev_body.start_tx_id + prev_body.tx_count, last_transition))
    }

    /// Query the block header by number
    pub fn get_header(&self, number: BlockNumber) -> Result<Header, TransactionError> {
        let header =
            self.get::<tables::Headers>(number)?.ok_or(ProviderError::Header { number })?;
        Ok(header)
    }

    /// Get the total difficulty for a block.
    pub fn get_td(&self, block: BlockNumber) -> Result<U256, TransactionError> {
        let td = self
            .get::<tables::HeaderTD>(block)?
            .ok_or(ProviderError::TotalDifficulty { number: block })?;
        Ok(td.into())
    }

    /// Unwind table by some number key
    #[inline]
    pub fn unwind_table_by_num<T>(&self, num: u64) -> Result<(), DbError>
    where
        DB: Database,
        T: Table<Key = u64>,
    {
        self.unwind_table::<T, _>(num, |key| key)
    }

    /// Unwind the table to a provided block
    pub(crate) fn unwind_table<T, F>(
        &self,
        block: BlockNumber,
        mut selector: F,
    ) -> Result<(), DbError>
    where
        DB: Database,
        T: Table,
        F: FnMut(T::Key) -> BlockNumber,
    {
        let mut cursor = self.cursor_write::<T>()?;
        let mut reverse_walker = cursor.walk_back(None)?;

        while let Some(Ok((key, _))) = reverse_walker.next() {
            if selector(key.clone()) <= block {
                break
            }
            self.delete::<T>(key, None)?;
        }
        Ok(())
    }

    /// Unwind a table forward by a [Walker][reth_db::abstraction::cursor::Walker] on another table
    pub fn unwind_table_by_walker<T1, T2>(&self, start_at: T1::Key) -> Result<(), DbError>
    where
        DB: Database,
        T1: Table,
        T2: Table<Key = T1::Value>,
    {
        let mut cursor = self.cursor_write::<T1>()?;
        let mut walker = cursor.walk(Some(start_at))?;
        while let Some((_, value)) = walker.next().transpose()? {
            self.delete::<T2>(value, None)?;
        }
        Ok(())
    }

    /// Load last shard and check if it is full and remove if it is not. If list is empty, last
    /// shard was full or there is no shards at all.
    fn take_last_account_shard(&self, address: Address) -> Result<Vec<u64>, TransactionError> {
        let mut cursor = self.cursor_read::<tables::AccountHistory>()?;
        let last = cursor.seek_exact(ShardedKey::new(address, u64::MAX))?;
        if let Some((shard_key, list)) = last {
            // delete old shard so new one can be inserted.
            self.delete::<tables::AccountHistory>(shard_key, None)?;
            let list = list.iter(0).map(|i| i as u64).collect::<Vec<_>>();
            return Ok(list)
        }
        Ok(Vec::new())
    }

    /// Load last shard and check if it is full and remove if it is not. If list is empty, last
    /// shard was full or there is no shards at all.
    pub fn take_last_storage_shard(
        &self,
        address: Address,
        storage_key: H256,
    ) -> Result<Vec<u64>, TransactionError> {
        let mut cursor = self.cursor_read::<tables::StorageHistory>()?;
        let last = cursor.seek_exact(StorageShardedKey::new(address, storage_key, u64::MAX))?;
        if let Some((storage_shard_key, list)) = last {
            // delete old shard so new one can be inserted.
            self.delete::<tables::StorageHistory>(storage_shard_key, None)?;
            let list = list.iter(0).map(|i| i as u64).collect::<Vec<_>>();
            return Ok(list)
        }
        Ok(Vec::new())
    }
}

/// Stages impl
impl<'this, DB> Transaction<'this, DB>
where
    DB: Database,
{
    /// Insert full block and make it canonical
    ///
    /// This is atomic operation and transaction will do one commit at the end of the function.
    pub fn insert_block(
        &mut self,
        block: &SealedBlock,
        chain_spec: &ChainSpec,
        changeset: ExecutionResult,
    ) -> Result<(), TransactionError> {
        // Header, Body, SenderRecovery, TD, TxLookup stages
        let (from, to) = insert_canonical_block(self.deref_mut(), block, false).unwrap();

        let parent_block_number = block.number - 1;

        // execution stage
        self.insert_execution_result(vec![changeset], chain_spec, parent_block_number)?;

        // storage hashing stage
        {
            let lists = self.get_addresses_and_keys_of_changed_storages(from, to)?;
            let storages = self.get_plainstate_storages(lists.into_iter())?;
            self.insert_storage_for_hashing(storages.into_iter())?;
        }

        // account hashing stage
        {
            let lists = self.get_addresses_of_changed_accounts(from, to)?;
            let accounts = self.get_plainstate_accounts(lists.into_iter())?;
            self.insert_account_for_hashing(accounts.into_iter())?;
        }

        // merkle tree
        {
            let current_root = self.get_header(parent_block_number)?.state_root;
            let loader = DBTrieLoader::default();
            let root = loader.update_root(self, current_root, from..to)?;
            if root != block.state_root {
                return Err(TransactionError::StateTrieRootMismatch {
                    got: root,
                    expected: block.state_root,
                    block_number: block.number,
                    block_hash: block.hash(),
                })
            }
        }

        // account history stage
        {
            let indices = self.get_account_transition_ids_from_changeset(from, to)?;
            self.insert_account_history_index(indices)?;
        }

        // storage history stage
        {
            let indices = self.get_storage_transition_ids_from_changeset(from, to)?;
            self.insert_storage_history_index(indices)?;
        }

        Ok(())
    }

    /// Return list of entries from table
    ///
    /// If TAKE is true, opened cursor would be write and it would delete all values from db.
    #[inline]
    pub fn get_or_take<T: Table, const TAKE: bool>(
        &self,
        range: impl RangeBounds<T::Key>,
    ) -> Result<Vec<KeyValue<T>>, DbError> {
        if TAKE {
            let mut cursor_write = self.cursor_write::<T>()?;
            let mut walker = cursor_write.walk_range(range)?;
            let mut items = Vec::new();
            while let Some(i) = walker.next().transpose()? {
                walker.delete_current()?;
                items.push(i)
            }
            Ok(items)
        } else {
            self.cursor_read::<T>()?.walk_range(range)?.collect::<Result<Vec<_>, _>>()
        }
    }

    /// Get requested blocks transaction with signer
    pub fn get_block_transaction_range<const TAKE: bool>(
        &self,
        range: impl RangeBounds<BlockNumber> + Clone,
    ) -> Result<Vec<(BlockNumber, Vec<TransactionSignedEcRecovered>)>, TransactionError> {
        // Just read block tx id from table. as it is needed to get execution results.
        let block_bodies = self.get_or_take::<tables::BlockBodies, false>(range)?;

        if block_bodies.is_empty() {
            return Ok(Vec::new())
        }

        // iterate over and get all transaction and signers
        let first_transaction =
            block_bodies.first().expect("If we have headers").1.first_tx_index();
        let last_transaction = block_bodies.last().expect("Not empty").1.last_tx_index();

        let transactions =
            self.get_or_take::<tables::Transactions, TAKE>(first_transaction..last_transaction)?;
        let senders =
            self.get_or_take::<tables::TxSenders, TAKE>(first_transaction..last_transaction)?;

        if TAKE {
            // rm TxHashNumber
            let mut tx_hash_cursor = self.cursor_write::<tables::TxHashNumber>()?;
            for (_, tx) in transactions.iter() {
                if tx_hash_cursor.seek_exact(tx.hash())?.is_some() {
                    tx_hash_cursor.delete_current()?;
                }
            }
        }

        // Merge transaction into blocks
        let mut block_tx = Vec::new();
        let mut senders = senders.into_iter();
        let mut transactions = transactions.into_iter();
        for (block_number, block_body) in block_bodies {
            let mut one_block_tx = Vec::new();
            for _ in 0..block_body.tx_count() {
                let tx = transactions.next();
                let sender = senders.next();

                let recovered = match (tx, sender) {
                    (Some((tx_id, tx)), Some((sender_tx_id, sender))) => {
                        if tx_id != sender_tx_id {
                            Err(ProviderError::MismatchOfTransactionAndSenderId { tx_id })
                        } else {
                            Ok(TransactionSignedEcRecovered::from_signed_transaction(tx, sender))
                        }
                    }
                    (Some((tx_id, _)), _) | (_, Some((tx_id, _))) => {
                        Err(ProviderError::MismatchOfTransactionAndSenderId { tx_id })
                    }
                    (None, None) => Err(ProviderError::BlockBodyTransactionCount),
                }?;
                one_block_tx.push(recovered)
            }
            block_tx.push((block_number, one_block_tx));
        }

        Ok(block_tx)
    }

    /// Return range of blocks and its execution result
    pub fn get_block_range<const TAKE: bool>(
        &self,
        range: impl RangeBounds<BlockNumber> + Clone,
    ) -> Result<Vec<SealedBlockWithSenders>, TransactionError> {
        // For block we need Headers, Bodies, Uncles, withdrawals, Transactions, Signers

        let block_headers = self.get_or_take::<tables::Headers, TAKE>(range.clone())?;
        if block_headers.is_empty() {
            return Ok(Vec::new())
        }

        let block_header_hashes =
            self.get_or_take::<tables::CanonicalHeaders, TAKE>(range.clone())?;
        let block_ommers = self.get_or_take::<tables::BlockOmmers, TAKE>(range.clone())?;
        let block_withdrawals =
            self.get_or_take::<tables::BlockWithdrawals, TAKE>(range.clone())?;

        let block_tx = self.get_block_transaction_range::<TAKE>(range.clone())?;

        if TAKE {
            // rm HeaderTD
            self.get_or_take::<tables::HeaderTD, TAKE>(range)?;
            // rm HeaderNumbers
            let mut header_number_cursor = self.cursor_write::<tables::HeaderNumbers>()?;
            for (_, hash) in block_header_hashes.iter() {
                if header_number_cursor.seek_exact(*hash)?.is_some() {
                    header_number_cursor.delete_current()?;
                }
            }
        }

        // merge all into block
        let block_header_iter = block_headers.into_iter();
        let block_header_hashes_iter = block_header_hashes.into_iter();
        let block_tx_iter = block_tx.into_iter();

        // can be not found in tables
        let mut block_ommers_iter = block_ommers.into_iter();
        let mut block_withdrawals_iter = block_withdrawals.into_iter();
        let mut block_ommers = block_ommers_iter.next();
        let mut block_withdrawals = block_withdrawals_iter.next();

        let mut blocks = Vec::new();
        for ((main_block_number, header), (_, header_hash), (_, tx)) in izip!(
            block_header_iter.into_iter(),
            block_header_hashes_iter.into_iter(),
            block_tx_iter.into_iter()
        ) {
            let header = header.seal(header_hash);

            let (body, senders) = tx.into_iter().map(|tx| tx.to_components()).unzip();

            // Ommers can be missing
            let mut ommers = Vec::new();
            if let Some((block_number, _)) = block_ommers.as_ref() {
                if *block_number == main_block_number {
                    // Seal ommers as they dont have hash.
                    ommers = block_ommers
                        .take()
                        .unwrap()
                        .1
                        .ommers
                        .into_iter()
                        .map(|h| h.seal_slow())
                        .collect();
                    block_ommers = block_ommers_iter.next();
                }
            };

            // withdrawal can be missing
            let shanghai_is_active = true;
            let mut withdrawals = Some(Vec::new());
            if shanghai_is_active {
                if let Some((block_number, _)) = block_withdrawals.as_ref() {
                    if *block_number == main_block_number {
                        withdrawals = Some(block_withdrawals.take().unwrap().1.withdrawals);
                        block_withdrawals = block_withdrawals_iter.next();
                    }
                }
            } else {
                withdrawals = None
            }

            blocks.push(SealedBlockWithSenders {
                block: SealedBlock { header, body, ommers, withdrawals },
                senders,
            })
        }

        Ok(blocks)
    }

    /// Transverse over changesets and plain state and recreated the execution results.
    pub fn get_block_execution_result_range<const TAKE: bool>(
        &self,
        range: impl RangeBounds<BlockNumber> + Clone,
    ) -> Result<Vec<ExecutionResult>, TransactionError> {
        let block_transition =
            self.get_or_take::<tables::BlockTransitionIndex, TAKE>(range.clone())?;

        if block_transition.is_empty() {
            return Ok(Vec::new())
        }
        // get block transitions
        let first_block_number =
            block_transition.first().expect("Check for empty is already done").0;

        let from = self.get_block_transition(first_block_number.saturating_sub(1))?;
        let to = block_transition.last().expect("Check for empty is already done").1;

        // NOTE: Just get block bodies dont remove them
        // it is connection point for bodies getter and execution result getter.
        let block_bodies = self.get_or_take::<tables::BlockBodies, false>(range)?;

        // get saved previous values
        let from_storage: TransitionIdAddress = (from, Address::zero()).into();
        let to_storage: TransitionIdAddress = (to, Address::zero()).into();

        let storage_changeset =
            self.get_or_take::<tables::StorageChangeSet, TAKE>(from_storage..to_storage)?;
        let account_changeset = self.get_or_take::<tables::AccountChangeSet, TAKE>(from..to)?;

        // iterate previous value and get plain state value to create changeset
        // Double option around Account represent if Account state is know (first option) and
        // account is removed (Second Option)
        type LocalPlainState = HashMap<Address, (Option<Option<Account>>, HashMap<H256, U256>)>;
        type Changesets = HashMap<
            TransitionId,
            BTreeMap<Address, (AccountInfoChangeSet, BTreeMap<H256, (U256, U256)>)>,
        >;

        let mut local_plain_state: LocalPlainState = HashMap::new();

        // iterate in reverse and get plain state.

        // Bundle execution changeset to its particular transaction and block
        let mut all_changesets: Changesets = HashMap::new();

        let mut plain_accounts_cursor = self.cursor_read::<tables::PlainAccountState>()?;
        let mut plain_storage_cursor = self.cursor_dup_read::<tables::PlainStorageState>()?;

        // add account changeset changes
        for (transition_id, account_before) in account_changeset.into_iter().rev() {
            let new_info = match local_plain_state.entry(account_before.address) {
                Entry::Vacant(entry) => {
                    let new_account =
                        plain_accounts_cursor.seek(account_before.address)?.map(|(_s, i)| i);
                    entry.insert((Some(account_before.info), HashMap::new()));
                    new_account
                }
                Entry::Occupied(mut entry) => {
                    let new_account =
                        std::mem::replace(&mut entry.get_mut().0, Some(account_before.info));
                    new_account.expect("As we are stacking account first, account would always be Some(Some) or Some(None)")
                }
            };
            let account_info_changeset = AccountInfoChangeSet::new(account_before.info, new_info);
            // insert changeset to transition id. Multiple account for same transition Id are not
            // possible.
            all_changesets
                .entry(transition_id)
                .or_default()
                .entry(account_before.address)
                .or_default()
                .0 = account_info_changeset
        }

        // add storage changeset changes
        for (transition_and_address, storage_entry) in storage_changeset.into_iter() {
            let TransitionIdAddress((transition_id, address)) = transition_and_address;
            let new_storage =
                match local_plain_state.entry(address).or_default().1.entry(storage_entry.key) {
                    Entry::Vacant(entry) => {
                        let new_storage = plain_storage_cursor
                            .seek_by_key_subkey(address, storage_entry.key)?
                            .filter(|storage| storage.key == storage_entry.key)
                            .unwrap_or_default();
                        entry.insert(storage_entry.value);
                        new_storage.value
                    }
                    Entry::Occupied(mut entry) => {
                        std::mem::replace(entry.get_mut(), storage_entry.value)
                    }
                };
            all_changesets
                .entry(transition_id)
                .or_default()
                .entry(address)
                .or_default()
                .1
                .insert(storage_entry.key, (storage_entry.value, new_storage));
        }

        if TAKE {
            // TODO iterate over local plain state
            // remove all account
            // and all storages.
        }

        //
        // NOTE: Some storage changesets can be empty,
        // all account changeset have at least beneficiary fee transfer.

        // iterate over block body and create ExecutionResult
        let mut block_exec_results = Vec::new();

        let mut changeset_iter = all_changesets.into_iter();
        let mut block_body_iter = block_bodies.into_iter();
        let mut block_transition_iter = block_transition.into_iter();
        let mut last_transition_id = from;
        loop {
            // loop break if we are at the end of the blocks.
            let Some((_,block_body)) = block_body_iter.next() else { break };
            let mut block_exec_res = ExecutionResult::default(); //TODO ExecResult

            for _ in 0..block_body.tx_count {
                let Some((transition_id, changeset)) = changeset_iter.next() else { break};
                last_transition_id = transition_id;
                block_exec_res.tx_changesets.push(TransactionChangeSet {
                    receipt: Receipt::default(), /* TODO(receipt) when they are saved, load them
                                                  * from db */
                    changeset: changeset
                        .into_iter()
                        .map(|(address, (account, storage))| {
                            (
                                address,
                                AccountChangeSet {
                                    account,
                                    storage: storage
                                        .into_iter()
                                        .map(|(key, val)| (U256::from_be_bytes(key.0), val))
                                        .collect(),
                                    wipe_storage: false, /* it is always false as all storage
                                                          * changesets for selfdestruct are
                                                          * already accounted. */
                                },
                            )
                        })
                        .collect(),
                    new_bytecodes: Default::default(), /* TODO(bytecode), bytecode is not cleared
                                                        * so it is same sa previous. */
                });
            }

            let Some((_,block_transition)) = block_transition_iter.next() else { break};
            if block_transition != last_transition_id {
                // take block changeset
                let Some((transition_id, changeset)) = changeset_iter.next() else { break};
                last_transition_id = transition_id;
                block_exec_res.block_changesets = changeset
                    .into_iter()
                    .map(|(address, (account, _))| (address, account))
                    .collect();
            }
            block_exec_results.push(block_exec_res)
        }
        Ok(block_exec_results)
    }

    /// Return range of blocks and its execution result
    pub fn get_block_and_execution_range<const TAKE: bool>(
        &self,
        range: impl RangeBounds<BlockNumber> + Clone,
    ) -> Result<Vec<(SealedBlockWithSenders, ExecutionResult)>, TransactionError> {
        // get blocks
        let blocks = self.get_block_range::<TAKE>(range.clone())?;
        // get execution res
        let execution_res = self.get_block_execution_result_range::<TAKE>(range.clone())?;
        // combine them
        let res = blocks.into_iter().zip(execution_res.into_iter()).collect();

        // remove block bodies it is needed for both get block range and get block execution results
        // that is why it is deleted afterwards.
        if TAKE {
            self.get_or_take::<tables::BlockBodies, TAKE>(range)?;
        }

        // return them
        Ok(res)
    }

    /// Iterate over account changesets and return all account address that were changed.
    pub fn get_addresses_and_keys_of_changed_storages(
        &self,
        from: TransitionId,
        to: TransitionId,
    ) -> Result<BTreeMap<Address, BTreeSet<H256>>, TransactionError> {
        Ok(self
            .cursor_read::<tables::StorageChangeSet>()?
            .walk_range(
                TransitionIdAddress((from, Address::zero()))..
                    TransitionIdAddress((to, Address::zero())),
            )?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            // fold all storages and save its old state so we can remove it from HashedStorage
            // it is needed as it is dup table.
            .fold(
                BTreeMap::new(),
                |mut accounts: BTreeMap<Address, BTreeSet<H256>>,
                 (TransitionIdAddress((_, address)), storage_entry)| {
                    accounts.entry(address).or_default().insert(storage_entry.key);
                    accounts
                },
            ))
    }

    ///  Get plainstate storages
    #[allow(clippy::type_complexity)]
    pub fn get_plainstate_storages(
        &self,
        iter: impl IntoIterator<Item = (Address, impl IntoIterator<Item = H256>)>,
    ) -> Result<Vec<(Address, Vec<(H256, U256)>)>, TransactionError> {
        let mut plain_storage = self.cursor_dup_read::<tables::PlainStorageState>()?;

        iter.into_iter()
            .map(|(address, storage)| {
                storage
                    .into_iter()
                    .map(|key| -> Result<_, TransactionError> {
                        let ret = plain_storage
                            .seek_by_key_subkey(address, key)?
                            .filter(|v| v.key == key)
                            .unwrap_or_default();
                        Ok((key, ret.value))
                    })
                    .collect::<Result<Vec<(_, _)>, _>>()
                    .map(|storage| (address, storage))
            })
            .collect::<Result<Vec<(_, _)>, _>>()
    }

    /// iterate over storages and insert them to hashing table
    pub fn insert_storage_for_hashing(
        &self,
        storages: impl IntoIterator<Item = (Address, impl IntoIterator<Item = (H256, U256)>)>,
    ) -> Result<(), TransactionError> {
        // hash values
        let hashed = storages.into_iter().fold(BTreeMap::new(), |mut map, (address, storage)| {
            let storage = storage.into_iter().fold(BTreeMap::new(), |mut map, (key, value)| {
                map.insert(keccak256(key), value);
                map
            });
            map.insert(keccak256(address), storage);
            map
        });

        let mut hashed_storage = self.cursor_dup_write::<tables::HashedStorage>()?;
        // Hash the address and key and apply them to HashedStorage (if Storage is None
        // just remove it);
        hashed.into_iter().try_for_each(|(hashed_address, storage)| {
            storage.into_iter().try_for_each(|(key, value)| -> Result<(), TransactionError> {
                if hashed_storage
                    .seek_by_key_subkey(hashed_address, key)?
                    .filter(|entry| entry.key == key)
                    .is_some()
                {
                    hashed_storage.delete_current()?;
                }

                if value != U256::ZERO {
                    hashed_storage.upsert(hashed_address, StorageEntry { key, value })?;
                }
                Ok(())
            })
        })?;
        Ok(())
    }

    /// Iterate over account changesets and return all account address that were changed.
    pub fn get_addresses_of_changed_accounts(
        &self,
        from: TransitionId,
        to: TransitionId,
    ) -> Result<BTreeSet<Address>, TransactionError> {
        Ok(self
            .cursor_read::<tables::AccountChangeSet>()?
            .walk_range(from..to)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            // fold all account to one set of changed accounts
            .fold(BTreeSet::new(), |mut accounts: BTreeSet<Address>, (_, account_before)| {
                accounts.insert(account_before.address);
                accounts
            }))
    }

    /// Get plainstate account from iterator
    pub fn get_plainstate_accounts(
        &self,
        iter: impl IntoIterator<Item = Address>,
    ) -> Result<Vec<(Address, Option<Account>)>, TransactionError> {
        let mut plain_accounts = self.cursor_read::<tables::PlainAccountState>()?;
        Ok(iter
            .into_iter()
            .map(|address| plain_accounts.seek_exact(address).map(|a| (address, a.map(|(_, v)| v))))
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// iterate over accounts and insert them to hashing table
    pub fn insert_account_for_hashing(
        &self,
        accounts: impl IntoIterator<Item = (Address, Option<Account>)>,
    ) -> Result<(), TransactionError> {
        let mut hashed_accounts = self.cursor_write::<tables::HashedAccount>()?;

        let hashes_accounts = accounts.into_iter().fold(
            BTreeMap::new(),
            |mut map: BTreeMap<H256, Option<Account>>, (address, account)| {
                map.insert(keccak256(address), account);
                map
            },
        );

        hashes_accounts.into_iter().try_for_each(
            |(hashed_address, account)| -> Result<(), TransactionError> {
                if let Some(account) = account {
                    hashed_accounts.upsert(hashed_address, account)?
                } else if hashed_accounts.seek_exact(hashed_address)?.is_some() {
                    hashed_accounts.delete_current()?;
                }
                Ok(())
            },
        )?;
        Ok(())
    }

    /// Get all transaction ids where account got changed.
    pub fn get_storage_transition_ids_from_changeset(
        &self,
        from: TransitionId,
        to: TransitionId,
    ) -> Result<BTreeMap<(Address, H256), Vec<u64>>, TransactionError> {
        let storage_changeset = self
            .cursor_read::<tables::StorageChangeSet>()?
            .walk(Some((from, Address::zero()).into()))?
            .take_while(|res| res.as_ref().map(|(k, _)| k.transition_id() < to).unwrap_or_default())
            .collect::<Result<Vec<_>, _>>()?;

        // fold all storages to one set of changes
        let storage_changeset_lists = storage_changeset.into_iter().fold(
            BTreeMap::new(),
            |mut storages: BTreeMap<(Address, H256), Vec<u64>>, (index, storage)| {
                storages
                    .entry((index.address(), storage.key))
                    .or_default()
                    .push(index.transition_id());
                storages
            },
        );

        Ok(storage_changeset_lists)
    }

    /// Get all transaction ids where account got changed.
    pub fn get_account_transition_ids_from_changeset(
        &self,
        from: TransitionId,
        to: TransitionId,
    ) -> Result<BTreeMap<Address, Vec<u64>>, TransactionError> {
        let account_changesets = self
            .cursor_read::<tables::AccountChangeSet>()?
            .walk(Some(from))?
            .take_while(|res| res.as_ref().map(|(k, _)| *k < to).unwrap_or_default())
            .collect::<Result<Vec<_>, _>>()?;

        let account_transtions = account_changesets
            .into_iter()
            // fold all account to one set of changed accounts
            .fold(
                BTreeMap::new(),
                |mut accounts: BTreeMap<Address, Vec<u64>>, (index, account)| {
                    accounts.entry(account.address).or_default().push(index);
                    accounts
                },
            );

        Ok(account_transtions)
    }

    /// Insert storage change index to database. Used inside StorageHistoryIndex stage
    pub fn insert_storage_history_index(
        &self,
        storage_transitions: BTreeMap<(Address, H256), Vec<u64>>,
    ) -> Result<(), TransactionError> {
        for ((address, storage_key), mut indices) in storage_transitions {
            let mut last_shard = self.take_last_storage_shard(address, storage_key)?;
            last_shard.append(&mut indices);

            // chunk indices and insert them in shards of N size.
            let mut chunks = last_shard
                .iter()
                .chunks(storage_sharded_key::NUM_OF_INDICES_IN_SHARD)
                .into_iter()
                .map(|chunks| chunks.map(|i| *i as usize).collect::<Vec<usize>>())
                .collect::<Vec<_>>();
            let last_chunk = chunks.pop();

            // chunk indices and insert them in shards of N size.
            chunks.into_iter().try_for_each(|list| {
                self.put::<tables::StorageHistory>(
                    StorageShardedKey::new(
                        address,
                        storage_key,
                        *list.last().expect("Chuck does not return empty list") as TransitionId,
                    ),
                    TransitionList::new(list).expect("Indices are presorted and not empty"),
                )
            })?;
            // Insert last list with u64::MAX
            if let Some(last_list) = last_chunk {
                self.put::<tables::StorageHistory>(
                    StorageShardedKey::new(address, storage_key, u64::MAX),
                    TransitionList::new(last_list).expect("Indices are presorted and not empty"),
                )?;
            }
        }
        Ok(())
    }

    /// Insert account change index to database. Used inside AccountHistoryIndex stage
    pub fn insert_account_history_index(
        &self,
        account_transitions: BTreeMap<Address, Vec<u64>>,
    ) -> Result<(), TransactionError> {
        // insert indexes to AccountHistory.
        for (address, mut indices) in account_transitions {
            let mut last_shard = self.take_last_account_shard(address)?;
            last_shard.append(&mut indices);
            // chunk indices and insert them in shards of N size.
            let mut chunks = last_shard
                .iter()
                .chunks(sharded_key::NUM_OF_INDICES_IN_SHARD)
                .into_iter()
                .map(|chunks| chunks.map(|i| *i as usize).collect::<Vec<usize>>())
                .collect::<Vec<_>>();
            let last_chunk = chunks.pop();

            chunks.into_iter().try_for_each(|list| {
                self.put::<tables::AccountHistory>(
                    ShardedKey::new(
                        address,
                        *list.last().expect("Chuck does not return empty list") as TransitionId,
                    ),
                    TransitionList::new(list).expect("Indices are presorted and not empty"),
                )
            })?;
            // Insert last list with u64::MAX
            if let Some(last_list) = last_chunk {
                self.put::<tables::AccountHistory>(
                    ShardedKey::new(address, u64::MAX),
                    TransitionList::new(last_list).expect("Indices are presorted and not empty"),
                )?
            }
        }
        Ok(())
    }

    /// Used inside execution stage to commit created account storage changesets for transaction or
    /// block state change.
    pub fn insert_execution_result(
        &self,
        changesets: Vec<ExecutionResult>,
        chain_spec: &ChainSpec,
        parent_block_number: u64,
    ) -> Result<(), TransactionError> {
        // Get last tx count so that we can know amount of transaction in the block.
        let mut current_transition_id = self
            .get::<tables::BlockTransitionIndex>(parent_block_number)?
            .ok_or(ProviderError::BlockTransition { block_number: parent_block_number })?;

        info!(target: "sync::stages::execution", current_transition_id, blocks = changesets.len(), "Inserting execution results");

        // apply changes to plain database.
        let mut block_number = parent_block_number;
        for results in changesets.into_iter() {
            block_number += 1;
            let spurious_dragon_active =
                chain_spec.fork(Hardfork::SpuriousDragon).active_at_block(block_number);
            // insert state change set
            for result in results.tx_changesets.into_iter() {
                for (address, account_change_set) in result.changeset.into_iter() {
                    let AccountChangeSet { account, wipe_storage, storage } = account_change_set;
                    // apply account change to db. Updates AccountChangeSet and PlainAccountState
                    // tables.
                    trace!(target: "sync::stages::execution", ?address, current_transition_id, ?account, wipe_storage, "Applying account changeset");
                    account.apply_to_db(
                        &**self,
                        address,
                        current_transition_id,
                        spurious_dragon_active,
                    )?;

                    let storage_id = TransitionIdAddress((current_transition_id, address));

                    // cast key to H256 and trace the change
                    let storage = storage
                                .into_iter()
                                .map(|(key, (old_value,new_value))| {
                                    let hkey = H256(key.to_be_bytes());
                                    trace!(target: "sync::stages::execution", ?address, current_transition_id, ?hkey, ?old_value, ?new_value, "Applying storage changeset");
                                    (hkey, old_value,new_value)
                                })
                                .collect::<Vec<_>>();

                    let mut cursor_storage_changeset =
                        self.cursor_write::<tables::StorageChangeSet>()?;
                    cursor_storage_changeset.seek_exact(storage_id)?;

                    if wipe_storage {
                        // iterate over storage and save them before entry is deleted.
                        self.cursor_read::<tables::PlainStorageState>()?
                            .walk(Some(address))?
                            .take_while(|res| {
                                res.as_ref().map(|(k, _)| *k == address).unwrap_or_default()
                            })
                            .try_for_each(|entry| {
                                let (_, old_value) = entry?;
                                cursor_storage_changeset.append(storage_id, old_value)
                            })?;

                        // delete all entries
                        self.delete::<tables::PlainStorageState>(address, None)?;

                        // insert storage changeset
                        for (key, _, new_value) in storage {
                            // old values are already cleared.
                            if new_value != U256::ZERO {
                                self.put::<tables::PlainStorageState>(
                                    address,
                                    StorageEntry { key, value: new_value },
                                )?;
                            }
                        }
                    } else {
                        // insert storage changeset
                        for (key, old_value, new_value) in storage {
                            let old_entry = StorageEntry { key, value: old_value };
                            let new_entry = StorageEntry { key, value: new_value };
                            // insert into StorageChangeSet
                            cursor_storage_changeset.append(storage_id, old_entry)?;

                            // Always delete old value as duplicate table, put will not override it
                            self.delete::<tables::PlainStorageState>(address, Some(old_entry))?;
                            if new_value != U256::ZERO {
                                self.put::<tables::PlainStorageState>(address, new_entry)?;
                            }
                        }
                    }
                }
                // insert bytecode
                for (hash, bytecode) in result.new_bytecodes.into_iter() {
                    // make different types of bytecode. Checked and maybe even analyzed (needs to
                    // be packed). Currently save only raw bytes.
                    let bytecode = bytecode.bytes();
                    trace!(target: "sync::stages::execution", ?hash, ?bytecode, len = bytecode.len(), "Inserting bytecode");
                    self.put::<tables::Bytecodes>(hash, bytecode[..bytecode.len()].to_vec())?;

                    // NOTE: bytecode bytes are not inserted in change set and can be found in
                    // separate table
                }
                current_transition_id += 1;
            }

            // If there are any post block changes, we will add account changesets to db.
            for (address, changeset) in results.block_changesets.into_iter() {
                trace!(target: "sync::stages::execution", ?address, current_transition_id, "Applying block reward");
                changeset.apply_to_db(
                    &**self,
                    address,
                    current_transition_id,
                    spurious_dragon_active,
                )?;
            }
            current_transition_id += 1;
        }
        Ok(())
    }
}

/// An error that can occur when using the transaction container
#[derive(Debug, thiserror::Error)]
pub enum TransactionError {
    /// The transaction encountered a database error.
    #[error("Database error: {0}")]
    Database(#[from] DbError),
    /// The transaction encountered a database integrity error.
    #[error("A database integrity error occurred: {0}")]
    DatabaseIntegrity(#[from] ProviderError),
    /// The transaction encountered merkle trie error.
    #[error("Merkle trie calculation error: {0}")]
    MerkleTrie(#[from] TrieError),
    /// Root mismatch
    #[error("Merkle trie root mismatch on block: #{block_number:?} {block_hash:?}. got: {got:?} expected:{got:?}")]
    StateTrieRootMismatch {
        /// Expected root
        expected: H256,
        /// Calculated root
        got: H256,
        /// Block number
        block_number: BlockNumber,
        /// Block hash
        block_hash: BlockHash,
    },
}
