// This file is part of Substrate.

// Copyright (C) 2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! State backend that's useful for benchmarking

use std::sync::Arc;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use hash_db::{Prefix, Hasher};
use sp_trie::{MemoryDB, prefixed_key};
use sp_core::{storage::ChildInfo, hexdisplay::HexDisplay};
use sp_runtime::traits::{Block as BlockT, HashFor};
use sp_runtime::Storage;
use sp_state_machine::{DBValue, backend::Backend as StateBackend, StorageCollection};
use kvdb::{KeyValueDB, DBTransaction};
use crate::storage_cache::{CachingState, SharedCache, new_shared_cache};

type DbState<B> = sp_state_machine::TrieBackend<
	Arc<dyn sp_state_machine::Storage<HashFor<B>>>, HashFor<B>
>;

type State<B> = CachingState<DbState<B>, B>;

struct StorageDb<Block: BlockT> {
	db: Arc<dyn KeyValueDB>,
	_block: std::marker::PhantomData<Block>,
}

impl<Block: BlockT> sp_state_machine::Storage<HashFor<Block>> for StorageDb<Block> {
	fn get(&self, key: &Block::Hash, prefix: Prefix) -> Result<Option<DBValue>, String> {
		let key = prefixed_key::<HashFor<Block>>(key, prefix);
		self.db.get(0, &key)
			.map_err(|e| format!("Database backend error: {:?}", e))
	}
}

/// Track whether a specific key has already been read or written to.
#[derive(Default, Clone, Copy)]
pub struct KeyTracker {
	has_been_read: bool,
	has_been_written: bool,
}

/// A simple object that counts the reads and writes at the key level to the underlying state db.
#[derive(Default, Clone, Copy, Debug)]
pub struct ReadWriteTracker {
	reads: u32,
	repeat_reads: u32,
	writes: u32,
	repeat_writes: u32,
}

impl ReadWriteTracker {
	fn add_read(&mut self) {
		self.reads += 1;
	}

	fn add_repeat_read(&mut self) {
		self.repeat_reads += 1;
	}

	fn add_write(&mut self) {
		self.writes += 1;
	}

	fn add_repeat_write(&mut self) {
		self.repeat_writes += 1;
	}
}

/// State that manages the backend database reference. Allows runtime to control the database.
pub struct BenchmarkingState<B: BlockT> {
	root: Cell<B::Hash>,
	genesis_root: B::Hash,
	state: RefCell<Option<State<B>>>,
	db: Cell<Option<Arc<dyn KeyValueDB>>>,
	genesis: HashMap<Vec<u8>, (Vec<u8>, i32)>,
	record: Cell<Vec<Vec<u8>>>,
	shared_cache: SharedCache<B>, // shared cache is always empty
	key_tracker: RefCell<HashMap<Vec<u8>, KeyTracker>>,
	read_write_tracker: RefCell<ReadWriteTracker>,
}

impl<B: BlockT> BenchmarkingState<B> {
	/// Create a new instance that creates a database in a temporary dir.
	pub fn new(genesis: Storage, _cache_size_mb: Option<usize>) -> Result<Self, String> {
		let mut root = B::Hash::default();
		let mut mdb = MemoryDB::<HashFor<B>>::default();
		sp_state_machine::TrieDBMut::<HashFor<B>>::new(&mut mdb, &mut root);

		let mut state = BenchmarkingState {
			state: RefCell::new(None),
			db: Cell::new(None),
			root: Cell::new(root),
			genesis: Default::default(),
			genesis_root: Default::default(),
			record: Default::default(),
			shared_cache: new_shared_cache(0, (1, 10)),
			key_tracker: Default::default(),
			read_write_tracker: Default::default(),
		};

		state.reopen()?;
		let child_delta = genesis.children_default.iter().map(|(_storage_key, child_content)| (
			&child_content.child_info,
			child_content.data.iter().map(|(k, v)| (k.as_ref(), Some(v.as_ref()))),
		));
		let (root, transaction): (B::Hash, _) = state.state.borrow_mut().as_mut().unwrap().full_storage_root(
			genesis.top.iter().map(|(k, v)| (k.as_ref(), Some(v.as_ref()))),
			child_delta,
		);
		state.genesis = transaction.clone().drain();
		state.genesis_root = root.clone();
		state.commit(root, transaction, Vec::new())?;
		state.record.take();
		Ok(state)
	}

	fn reopen(&self) -> Result<(), String> {
		*self.state.borrow_mut() = None;
		let db = match self.db.take() {
			Some(db) => db,
			None => Arc::new(::kvdb_memorydb::create(1)),
		};
		self.db.set(Some(db.clone()));
		let storage_db = Arc::new(StorageDb::<B> { db, _block: Default::default() });
		*self.state.borrow_mut() = Some(State::new(
			DbState::<B>::new(storage_db, self.root.get()),
			self.shared_cache.clone(),
			None
		));
		Ok(())
	}

	fn wipe_tracker(&self) {
		*self.key_tracker.borrow_mut() = HashMap::new();
		*self.read_write_tracker.borrow_mut() = Default::default();
	}

	fn add_read_key(&self, key: &[u8]) {
		log::trace!(target: "benchmark", "Read: {}", HexDisplay::from(&key));

		let mut key_tracker = self.key_tracker.borrow_mut();
		let mut read_write_tracker = self.read_write_tracker.borrow_mut();

		let maybe_tracker = key_tracker.get(&key.to_vec());

		let has_been_read = KeyTracker {
			has_been_read: true,
			has_been_written: false,
		};

		match maybe_tracker {
			None => {
				key_tracker.insert(key.to_vec(), has_been_read);
				read_write_tracker.add_read();
			},
			Some(tracker) => {
				if !tracker.has_been_read {
					key_tracker.insert(key.to_vec(), has_been_read);
					read_write_tracker.add_read();
				} else {
					read_write_tracker.add_repeat_read();
				}
			}
		}
	}

	fn add_write_key(&self, key: &[u8]) {
		log::trace!(target: "benchmark", "Write: {}", HexDisplay::from(&key));

		let mut key_tracker = self.key_tracker.borrow_mut();
		let mut read_write_tracker = self.read_write_tracker.borrow_mut();

		let maybe_tracker = key_tracker.get(&key.to_vec());

		// If we have written to the key, we also consider that we have read from it.
		let has_been_written = KeyTracker {
			has_been_read: true,
			has_been_written: true,
		};

		match maybe_tracker {
			None => {
				key_tracker.insert(key.to_vec(), has_been_written);
				read_write_tracker.add_write();
			},
			Some(tracker) => {
				if !tracker.has_been_written {
					key_tracker.insert(key.to_vec(), has_been_written);
					read_write_tracker.add_write();
				} else {
					read_write_tracker.add_repeat_write();
				}
			}
		}
	}
}

fn state_err() -> String {
	"State is not open".into()
}

impl<B: BlockT> StateBackend<HashFor<B>> for BenchmarkingState<B> {
	type Error =  <DbState<B> as StateBackend<HashFor<B>>>::Error;
	type Transaction = <DbState<B> as StateBackend<HashFor<B>>>::Transaction;
	type TrieBackendStorage = <DbState<B> as StateBackend<HashFor<B>>>::TrieBackendStorage;

	fn storage(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		self.add_read_key(key);
		self.state.borrow().as_ref().ok_or_else(state_err)?.storage(key)
	}

	fn storage_hash(&self, key: &[u8]) -> Result<Option<B::Hash>, Self::Error> {
		self.add_read_key(key);
		self.state.borrow().as_ref().ok_or_else(state_err)?.storage_hash(key)
	}

	fn child_storage(
		&self,
		child_info: &ChildInfo,
		key: &[u8],
	) -> Result<Option<Vec<u8>>, Self::Error> {
		self.add_read_key(key);
		self.state.borrow().as_ref().ok_or_else(state_err)?.child_storage(child_info, key)
	}

	fn exists_storage(&self, key: &[u8]) -> Result<bool, Self::Error> {
		self.add_read_key(key);
		self.state.borrow().as_ref().ok_or_else(state_err)?.exists_storage(key)
	}

	fn exists_child_storage(
		&self,
		child_info: &ChildInfo,
		key: &[u8],
	) -> Result<bool, Self::Error> {
		self.add_read_key(key);
		self.state.borrow().as_ref().ok_or_else(state_err)?.exists_child_storage(child_info, key)
	}

	fn next_storage_key(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		self.add_read_key(key);
		self.state.borrow().as_ref().ok_or_else(state_err)?.next_storage_key(key)
	}

	fn next_child_storage_key(
		&self,
		child_info: &ChildInfo,
		key: &[u8],
	) -> Result<Option<Vec<u8>>, Self::Error> {
		self.add_read_key(key);
		self.state.borrow().as_ref().ok_or_else(state_err)?.next_child_storage_key(child_info, key)
	}

	fn for_keys_with_prefix<F: FnMut(&[u8])>(&self, prefix: &[u8], f: F) {
		if let Some(ref state) = *self.state.borrow() {
			state.for_keys_with_prefix(prefix, f)
		}
	}

	fn for_key_values_with_prefix<F: FnMut(&[u8], &[u8])>(&self, prefix: &[u8], f: F) {
		if let Some(ref state) = *self.state.borrow() {
			state.for_key_values_with_prefix(prefix, f)
		}
	}

	fn for_keys_in_child_storage<F: FnMut(&[u8])>(
		&self,
		child_info: &ChildInfo,
		f: F,
	) {
		if let Some(ref state) = *self.state.borrow() {
			state.for_keys_in_child_storage(child_info, f)
		}
	}

	fn for_child_keys_with_prefix<F: FnMut(&[u8])>(
		&self,
		child_info: &ChildInfo,
		prefix: &[u8],
		f: F,
	) {
		if let Some(ref state) = *self.state.borrow() {
			state.for_child_keys_with_prefix(child_info, prefix, f)
		}
	}

	fn storage_root<'a>(
		&self,
		delta: impl Iterator<Item=(&'a [u8], Option<&'a [u8]>)>,
	) -> (B::Hash, Self::Transaction) where B::Hash: Ord {
		self.state.borrow().as_ref().map_or(Default::default(), |s| s.storage_root(delta))
	}

	fn child_storage_root<'a>(
		&self,
		child_info: &ChildInfo,
		delta: impl Iterator<Item=(&'a [u8], Option<&'a [u8]>)>,
	) -> (B::Hash, bool, Self::Transaction) where B::Hash: Ord {
		self.state.borrow().as_ref().map_or(Default::default(), |s| s.child_storage_root(child_info, delta))
	}

	fn pairs(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
		self.state.borrow().as_ref().map_or(Default::default(), |s| s.pairs())
	}

	fn keys(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
		self.state.borrow().as_ref().map_or(Default::default(), |s| s.keys(prefix))
	}

	fn child_keys(
		&self,
		child_info: &ChildInfo,
		prefix: &[u8],
	) -> Vec<Vec<u8>> {
		self.state.borrow().as_ref().map_or(Default::default(), |s| s.child_keys(child_info, prefix))
	}

	fn as_trie_backend(&mut self)
		-> Option<&sp_state_machine::TrieBackend<Self::TrieBackendStorage, HashFor<B>>>
	{
		None
	}

	fn commit(&self, storage_root: <HashFor<B> as Hasher>::Out, mut transaction: Self::Transaction, storage_changes: StorageCollection)
		-> Result<(), Self::Error>
	{
		if let Some(db) = self.db.take() {
			let mut db_transaction = DBTransaction::new();
			let changes = transaction.drain();
			let mut keys = Vec::with_capacity(changes.len());
			for (key, (val, rc)) in changes {
				if rc > 0 {
					db_transaction.put(0, &key, &val);
				} else if rc < 0 {
					db_transaction.delete(0, &key);
				}
				keys.push(key);
			}
			let mut record = self.record.take();
			record.extend(keys);
			self.record.set(record);
			db.write(db_transaction).map_err(|_| String::from("Error committing transaction"))?;
			self.root.set(storage_root);
			self.db.set(Some(db));

			// Track DB Writes
			storage_changes.iter().for_each(|(key, _)| {
				self.add_write_key(key);
			});
		} else {
			return Err("Trying to commit to a closed db".into())
		}
		self.reopen()
	}

	fn wipe(&self) -> Result<(), Self::Error> {
		// Restore to genesis
		let record = self.record.take();
		if let Some(db) = self.db.take() {
			let mut db_transaction = DBTransaction::new();
			for key in record {
				match self.genesis.get(&key) {
					Some((v, _)) => db_transaction.put(0, &key, v),
					None => db_transaction.delete(0, &key),
				}
			}
			db.write(db_transaction).map_err(|_| String::from("Error committing transaction"))?;
			self.db.set(Some(db));
		}

		self.root.set(self.genesis_root.clone());
		self.reopen()?;
		self.wipe_tracker();
		Ok(())
	}

	/// Get the key tracking information for the state db.
	fn read_write_count(&self) -> (u32, u32, u32, u32) {
		let count = *self.read_write_tracker.borrow_mut();
		(count.reads, count.repeat_reads, count.writes, count.repeat_writes)
	}

	/// Reset the key tracking information for the state db.
	fn reset_read_write_count(&mut self) {
		self.wipe_tracker()
	}

	fn register_overlay_stats(&mut self, stats: &sp_state_machine::StateMachineStats) {
		self.state.borrow_mut().as_mut().map(|s| s.register_overlay_stats(stats));
	}

	fn usage_info(&self) -> sp_state_machine::UsageInfo {
		self.state.borrow().as_ref().map_or(sp_state_machine::UsageInfo::empty(), |s| s.usage_info())
	}
}

impl<Block: BlockT> std::fmt::Debug for BenchmarkingState<Block> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "Bench DB")
	}
}
