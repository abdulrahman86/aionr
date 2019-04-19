mod generic;
mod traits;

use lru_cache::LruCache;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, BTreeMap};
use std::sync::Arc;
use std::fmt;

use aion_types::{H128, U128, H256, U256, Address};
use bytes::{Bytes, ToPretty};
use self::generic::{Filth, BasicAccount};
use blake2b::{BLAKE2B_EMPTY, BLAKE2B_NULL_RLP, blake2b};
use rlp::*;
use pod_account::*;
use trie;
use trie::{Trie, SecTrieDB, TrieFactory, TrieError};

use kvdb::{DBValue, HashStore, DBTransaction};
use avm_abi::{ToBytes, FromBytes};
use db::{self};

pub use self::generic::Account;
pub use self::traits::{VMAccount, AccType};
use state::Backend;

const STORAGE_CACHE_ITEMS: usize = 8192;

pub type FVMCache = (RefCell<LruCache<H128, H128>>, RefCell<LruCache<H128, H256>>);
pub type FVMStorageChange = (HashMap<H128, H128>, HashMap<H128, H256>);
pub type FVMAccount = Account<FVMCache, FVMStorageChange>;

#[derive(Copy, Clone)]
pub enum RequireCache {
    None,
    CodeSize,
    Code,
}

impl FVMAccount {
    fn empty_storage_cache() -> FVMCache {
        (RefCell::new(LruCache::new(STORAGE_CACHE_ITEMS)), RefCell::new(LruCache::new(STORAGE_CACHE_ITEMS)))
    }

    fn empty_storage_change() -> FVMStorageChange {
        (HashMap::new(), HashMap::new())
    }
}

impl From<BasicAccount> for FVMAccount {
    fn from(basic: BasicAccount) -> Self {
        Account {
            balance: basic.balance,
            nonce: basic.nonce,
            storage_root: basic.storage_root,
            delta_root: basic.storage_root,
            storage_cache: Self::empty_storage_cache(),
            storage_changes: (HashMap::new(), HashMap::new()),
            code_hash: basic.code_hash,
            code_size: None,
            code_cache: Arc::new(vec![]),
            transformed_code_hash: BLAKE2B_EMPTY,
            transformed_code_size: None,
            transformed_code_cache: Arc::new(vec![]),
            objectgraph_hash: BLAKE2B_EMPTY,
            object_graph_size: None,
            object_graph_cache: Arc::new(vec![]),
            code_filth: Filth::Clean,
            address_hash: Cell::new(None),
            empty_but_commit: false,
            account_type: AccType::FVM,
            vm_create: false,
        }
    }
}

impl FVMAccount {
    pub fn new_contract(balance: U256, nonce: U256) -> Self {
        Self {
            balance: balance,
            nonce: nonce,
            storage_root: BLAKE2B_NULL_RLP,
            delta_root: BLAKE2B_NULL_RLP,
            storage_cache: Self::empty_storage_cache(),
            storage_changes: Self::empty_storage_change(),
            code_hash: BLAKE2B_EMPTY,
            code_cache: Arc::new(vec![]),
            transformed_code_hash: BLAKE2B_EMPTY,
            transformed_code_size: None,
            transformed_code_cache: Arc::new(vec![]),
            objectgraph_hash: BLAKE2B_EMPTY,
            object_graph_size: None,
            object_graph_cache: Arc::new(vec![]),
            code_size: None,
            code_filth: Filth::Clean,
            address_hash: Cell::new(None),
            empty_but_commit: false,
            account_type: AccType::FVM,
            vm_create: false,
        }
    }

    pub fn new_basic(balance: U256, nonce: U256) -> Self {
        Self {
            balance: balance,
            nonce: nonce,
            storage_root: BLAKE2B_NULL_RLP,
            delta_root: BLAKE2B_NULL_RLP,
            storage_cache: Self::empty_storage_cache(),
            storage_changes: Self::empty_storage_change(),
            code_hash: BLAKE2B_EMPTY,
            code_cache: Arc::new(vec![]),
            transformed_code_hash: BLAKE2B_EMPTY,
            transformed_code_size: None,
            transformed_code_cache: Arc::new(vec![]),
            objectgraph_hash: BLAKE2B_EMPTY,
            object_graph_size: None,
            object_graph_cache: Arc::new(vec![]),
            code_size: Some(0),
            code_filth: Filth::Clean,
            address_hash: Cell::new(None),
            empty_but_commit: false,
            account_type: AccType::FVM,
            vm_create: false,
        }
    }

    pub fn from_pod(pod: PodAccount) -> Self {
        FVMAccount {
            balance: pod.balance,
            nonce: pod.nonce,
            storage_root: BLAKE2B_NULL_RLP,
            delta_root: BLAKE2B_NULL_RLP,
            storage_cache: Self::empty_storage_cache(),
            storage_changes: (pod.storage.into_iter().collect(), HashMap::new()),
            code_hash: pod.code.as_ref().map_or(BLAKE2B_EMPTY, |c| blake2b(c)),
            code_filth: Filth::Dirty,
            code_size: Some(pod.code.as_ref().map_or(0, |c| c.len())),
            code_cache: Arc::new(pod.code.map_or_else(
                || {
                    warn!(target:"account","POD account with unknown code is being created! Assuming no code.");
                    vec![]
                },
                |c| c,
            )),
            transformed_code_hash: BLAKE2B_EMPTY,
            transformed_code_size: None,
            transformed_code_cache: Arc::new(vec![]),
            objectgraph_hash: BLAKE2B_EMPTY,
            object_graph_size: None,
            object_graph_cache: Arc::new(vec![]),
            address_hash: Cell::new(None),
            empty_but_commit: false,
            account_type: AccType::FVM,
            vm_create: false,
        }
    }

    fn storage_is_clean(&self) -> bool {
        self.storage_changes.0.is_empty() && self.storage_changes.1.is_empty()
    }

    /// Commit the `storage_changes` to the backing DB and update `storage_root`.
    pub fn commit_storage(
        &mut self,
        trie_factory: &TrieFactory,
        db: &mut HashStore,
    ) -> trie::Result<()>
    {
        let mut t = trie_factory.from_existing(db, &mut self.storage_root)?;
        for (k, v) in self.storage_changes.0.drain() {
            // cast key and value to trait type,
            // so we can call overloaded `to_bytes` method
            match v.is_zero() {
                true => t.remove(&k)?,
                false => t.insert(&k, &encode(&U128::from(&*v)))?,
            };

            self.storage_cache.0.borrow_mut().insert(k, v);
        }

        for (k, v) in self.storage_changes.1.drain() {
            // cast key and value to trait type,
            // so we can call overloaded `to_bytes` method
            match v.is_zero() {
                true => t.remove(&k)?,
                false => t.insert(&k, &encode(&v))?,
            };

            self.storage_cache.1.borrow_mut().insert(k, v);
        }

        Ok(())
    }

    pub fn discard_storage_changes(&mut self) {
        self.storage_changes.0.clear();
        self.storage_changes.1.clear();
    }

    /// Return the storage overlay.
    pub fn storage_changes(&self) -> &FVMStorageChange {
        &self.storage_changes
    }

    pub fn get_empty_but_commit(&mut self) -> bool { return self.empty_but_commit; }

    /// Clone basic account data
    pub fn clone_basic(&self) -> Self {
        Self {
            balance: self.balance.clone(),
            nonce: self.nonce.clone(),
            storage_root: self.storage_root.clone(),
            delta_root: self.delta_root.clone(),
            storage_cache: Self::empty_storage_cache(),
            storage_changes: Self::empty_storage_change(),
            code_hash: self.code_hash.clone(),
            code_size: self.code_size.clone(),
            code_cache: self.code_cache.clone(),
            transformed_code_hash: self.transformed_code_hash.clone(),
            transformed_code_size: self.transformed_code_size.clone(),
            transformed_code_cache: self.transformed_code_cache.clone(),
            objectgraph_hash: self.objectgraph_hash(),
            object_graph_size: self.object_graph_size.clone(),
            object_graph_cache: self.object_graph_cache.clone(),
            code_filth: self.code_filth,
            address_hash: self.address_hash.clone(),
            empty_but_commit: self.empty_but_commit.clone(),
            account_type: self.account_type.clone(),
            vm_create: self.vm_create.clone(),
        }
    }

    /// Clone account data, dirty storage keys and cached storage keys.
    // fn clone_all(&self) -> Self {
    //     let mut account = self.clone_dirty();
    //     account.storage_cache = self.storage_cache.clone();
    //     account
    // }

    pub fn set_empty_but_commit(&mut self) { self.empty_but_commit = true; }
}

impl AVMAccount {
    pub fn new_contract(balance: U256, nonce: U256) -> Self {
        Self {
            balance: balance,
            nonce: nonce,
            storage_root: BLAKE2B_NULL_RLP,
            delta_root: BLAKE2B_NULL_RLP,
            storage_cache: Self::empty_storage_cache(),
            storage_changes: Self::empty_storage_change(),
            code_hash: BLAKE2B_EMPTY,
            code_cache: Arc::new(vec![]),
            transformed_code_hash: BLAKE2B_EMPTY,
            transformed_code_size: None,
            transformed_code_cache: Arc::new(vec![]),
            objectgraph_hash: BLAKE2B_EMPTY,
            object_graph_size: None,
            object_graph_cache: Arc::new(vec![]),
            code_size: None,
            code_filth: Filth::Clean,
            address_hash: Cell::new(None),
            empty_but_commit: false,
            account_type: AccType::AVM,
            vm_create: false,
        }
    }
     /// Replace self with the data from other account merging storage cache.
    /// Basic account data and all modifications are overwritten
    /// with new values.
    pub fn overwrite_with(&mut self, other: Self) {
        self.balance = other.balance;
        self.nonce = other.nonce;
        self.storage_root = other.storage_root;
        self.code_hash = other.code_hash;
        self.code_filth = other.code_filth;
        self.code_cache = other.code_cache;
        self.code_size = other.code_size;
        self.address_hash = other.address_hash;

        let mut cache = self.storage_cache.borrow_mut();
        for (k, v) in other.storage_cache.into_inner() {
            cache.insert(k.clone(), v.clone()); //TODO: cloning should not be required here
        }
        self.storage_changes = other.storage_changes;
    }
}

impl From<BasicAccount> for AVMAccount {
    fn from(basic: BasicAccount) -> Self {
        Account {
            balance: basic.balance,
            nonce: basic.nonce,
            storage_root: basic.storage_root,
            delta_root: basic.storage_root,
            storage_cache: Self::empty_storage_cache(),
            storage_changes: Self::empty_storage_change(),
            code_hash: basic.code_hash,
            code_size: None,
            code_cache: Arc::new(vec![]),
            transformed_code_hash: BLAKE2B_EMPTY,
            transformed_code_size: None,
            transformed_code_cache: Arc::new(vec![]),
            objectgraph_hash: BLAKE2B_EMPTY,
            object_graph_size: None,
            object_graph_cache: Arc::new(vec![]),
            code_filth: Filth::Clean,
            address_hash: Cell::new(None),
            empty_but_commit: false,
            account_type: AccType::AVM,
            vm_create: true,
        }
    }
}

type AVMCache = RefCell<LruCache<Bytes, Bytes>>;
type AVMStorageChange = HashMap<Bytes, Bytes>;
pub type AVMAccount = Account<AVMCache, AVMStorageChange>;

impl AVMAccount {
    fn empty_storage_cache() -> AVMCache {
        RefCell::new(LruCache::new(STORAGE_CACHE_ITEMS))
    }

    fn empty_storage_change() -> AVMStorageChange {
        HashMap::new()
    }

    pub fn new_basic(balance: U256, nonce: U256) -> Self {
        Self {
            balance: balance,
            nonce: nonce,
            storage_root: BLAKE2B_NULL_RLP,
            delta_root: BLAKE2B_NULL_RLP,
            storage_cache: Self::empty_storage_cache(),
            storage_changes: HashMap::new(),
            code_hash: BLAKE2B_EMPTY,
            code_cache: Arc::new(vec![]),
            transformed_code_hash: BLAKE2B_EMPTY,
            transformed_code_size: None,
            transformed_code_cache: Arc::new(vec![]),
            objectgraph_hash: BLAKE2B_EMPTY,
            object_graph_size: None,
            object_graph_cache: Arc::new(vec![]),
            code_size: Some(0),
            code_filth: Filth::Clean,
            address_hash: Cell::new(None),
            empty_but_commit: false,
            account_type: AccType::AVM,
            vm_create: false,
        }
    }

    /// Determine whether there are any un-`commit()`-ed storage-setting operations.
    fn storage_is_clean(&self) -> bool {
        self.storage_changes.is_empty() 
    }

    /// Clone basic account data
    pub fn clone_basic(&self) -> Self {
        Self {
            balance: self.balance.clone(),
            nonce: self.nonce.clone(),
            storage_root: self.storage_root.clone(),
            delta_root: self.delta_root.clone(),
            storage_cache: Self::empty_storage_cache(),
            storage_changes: Self::empty_storage_change(),
            code_hash: self.code_hash.clone(),
            code_size: self.code_size.clone(),
            code_cache: self.code_cache.clone(),
            transformed_code_hash: self.transformed_code_hash.clone(),
            transformed_code_size: self.transformed_code_size.clone(),
            transformed_code_cache: self.transformed_code_cache.clone(),
            objectgraph_hash: self.objectgraph_hash.clone(),
            object_graph_size: self.object_graph_size.clone(),
            object_graph_cache: self.object_graph_cache.clone(),
            code_filth: self.code_filth,
            address_hash: self.address_hash.clone(),
            empty_but_commit: self.empty_but_commit.clone(),
            account_type: self.account_type.clone(),
            vm_create: self.vm_create.clone(),
        }
    }

    // commit avm storage changes to the Backing DB
    pub fn commit_storage(
        &mut self,
        trie_factory: &TrieFactory,
        db: &mut HashStore,
    ) -> trie::Result<()>
    {
        let mut t = trie_factory.from_existing(db, &mut self.storage_root)?;
        for (k, v) in self.storage_changes.drain() {
            // cast key and value to trait type,
            // so we can call overloaded `to_bytes` method
            let mut is_zero = true;
            for item in &v {
                if *item != 0x00_u8 {
                    is_zero = false;
                    break;
                }
            }
            match is_zero {
                true => t.remove(&k)?,
                false => t.insert(&k, &encode(&v))?,
            };

            self.storage_cache.borrow_mut().insert(k, v);
        }

        // store object graph for avm
        if self.objectgraph_hash != BLAKE2B_EMPTY {
            let mut db_tx = DBTransaction::new();
            db_tx.put(db::COL_EXTRA, &self.objectgraph_hash[..], self.object_graph_cache.clone().as_slice());
        }

        Ok(())
    }

    pub fn storage_changes(&self) -> &AVMStorageChange {
        &self.storage_changes
    }

    pub fn discard_storage_changes(&mut self) {
        self.storage_changes.clear()
    }
}

macro_rules! impl_account {
    ($T: ty, $fixed_strg: expr) => {
        impl VMAccount for $T {
            fn from_rlp(rlp: &[u8]) -> $T {
                let basic: BasicAccount = ::rlp::decode(rlp);
                basic.into()
            }

            fn init_code(&mut self, code: Bytes) {
                if self.code_hash == BLAKE2B_EMPTY {
                    self.vm_create = true;
                }
                self.code_hash = blake2b(&code);
                self.code_cache = Arc::new(code);
                self.code_size = Some(self.code_cache.len());
                self.code_filth = Filth::Dirty;
            }

            fn init_transformed_code(&mut self, code: Bytes) {
                self.transformed_code_hash = blake2b(&code);
                self.transformed_code_cache = Arc::new(code);
                self.transformed_code_size = Some(self.transformed_code_cache.len());
                self.code_filth = Filth::Dirty;
            }

            fn init_objectgraph(&mut self, data: Bytes) {
                self.objectgraph_hash = blake2b(&data);
                self.object_graph_cache = Arc::new(data);
            }

            fn objectgraph(&self) -> Option<Arc<Bytes>> {
                if self.object_graph_cache.is_empty() {
                    return None;
                }

                Some(self.object_graph_cache.clone())
            }

            fn reset_code(&mut self, code: Bytes) {
                self.init_code(code);
            }

            fn balance(&self) -> &U256 {&self.balance}

            fn nonce(&self) -> &U256 {&self.nonce}

            fn code_hash(&self) -> H256 {self.code_hash.clone()}

            fn transformed_code_hash(&self) -> H256 {self.transformed_code_hash.clone()}

            fn objectgraph_hash(&self) -> H256 {self.objectgraph_hash.clone()}

            fn address_hash(&self, address: &Address) -> H256 {
                let hash = self.address_hash.get();
                hash.unwrap_or_else(|| {
                    let hash = blake2b(address);
                    self.address_hash.set(Some(hash.clone()));
                    hash
                })
            }

            fn code(&self) -> Option<Arc<Bytes>> {
                if self.code_cache.is_empty() {
                    return None;
                }

                Some(self.code_cache.clone())
            }

            fn transformed_code(&self) -> Option<Arc<Bytes>> {
                if self.transformed_code_cache.is_empty() {
                    return None;
                }

                Some(self.transformed_code_cache.clone())
            }

            fn code_size(&self) -> Option<usize>{self.code_size.clone()}

            fn transformed_code_size(&self) -> Option<usize> {self.transformed_code_size.clone()}
            
            fn is_cached(&self) -> bool {
                !self.code_cache.is_empty()
                    || (self.code_cache.is_empty() && self.code_hash == BLAKE2B_EMPTY)
            }

            fn is_transformed_cached(&self) -> bool {
                !self.transformed_code_cache.is_empty()
                    || (self.transformed_code_cache.is_empty() && self.transformed_code_hash == BLAKE2B_EMPTY)
            }

            fn is_objectgraph_cached(&self) -> bool {
                !self.object_graph_cache.is_empty()
                    || (self.object_graph_cache.is_empty() && self.objectgraph_hash == BLAKE2B_EMPTY)
            }

            fn cache_code(&mut self, db: &HashStore) -> Option<Arc<Bytes>> {
                // TODO: fill out self.code_cache;
                trace!(
                    target: "account",
                    "Account::cache_code: ic={}; self.code_hash={:?}, self.code_cache={}",
                    self.is_cached(),
                    self.code_hash,
                    self.code_cache.pretty()
                );

                if self.is_cached() {
                    return Some(self.code_cache.clone());
                }

                println!("update code cache");
                match db.get(&self.code_hash) {
                    Some(x) => {
                        let code_size = x[0..4].to_u32() as usize;
                        self.code_size = Some(code_size);
                        self.code_cache = Arc::new(x[4..(4+code_size)].to_vec());
                        self.transformed_code_cache = Arc::new(x[4+code_size..].to_vec());
                        // println!("transformed code = {:?}", self.transformed_code_cache);
                        self.transformed_code_size = Some(x[4+code_size..].len());
                        Some(Arc::new(x.to_vec()).clone())
                    }
                    _ => {
                        warn!(target: "account","Failed reverse get of {}", self.code_hash);
                        None
                    }
                }
            }

            fn cache_transformed_code(&mut self, db:&HashStore) -> Option<Arc<Bytes>> {
                 if self.is_transformed_cached() {
                    return Some(self.transformed_code_cache.clone());
                }

                match db.get(&self.transformed_code_hash) {
                    Some(x) => {
                        self.transformed_code_size = Some(x.len());
                        self.transformed_code_cache = Arc::new(x.into_vec());
                        Some(self.transformed_code_cache.clone())
                    }
                    _ => {
                        warn!(target: "account","Failed reverse get of {}", self.transformed_code_hash);
                        None
                    }
                }
            }

            fn cache_objectgraph(&mut self, db: &HashStore) -> Option<Arc<Bytes>> {
                if self.is_objectgraph_cached() {
                    return Some(self.object_graph_cache.clone());
                }

                match db.get(&self.objectgraph_hash) {
                    Some(x) => {
                        self.object_graph_size = Some(x.len());
                        self.object_graph_cache = Arc::new(x.into_vec());
                        Some(self.object_graph_cache.clone())
                    }
                    _ => {
                        warn!(target: "account","Failed reverse get of {}", self.objectgraph_hash);
                        None
                    }
                }
            }

            fn cache_given_code(&mut self, code: Arc<Bytes>) {
                trace!(
                    target: "account",
                    "Account::cache_given_code: ic={}; self.code_hash={:?}, self.code_cache={}",
                    self.is_cached(),
                    self.code_hash,
                    self.code_cache.pretty()
                );

                let code_size = code[0..4].to_u32() as usize;
                self.code_size = Some(code_size);
                self.code_cache = Arc::new(code[4..(4+code_size)].to_vec());
                self.transformed_code_cache = Arc::new(code[4+code_size..].to_vec());
                // println!("transformed code = {:?}", self.transformed_code_cache);
                self.transformed_code_size = Some(code[4+code_size..].len());
                self.transformed_code_hash = blake2b(&code[4+code_size..].to_vec());
            }

            fn cache_given_transformed_code(&mut self, code: Arc<Bytes>) {
                trace!(
                    target: "account",
                    "Account::cache_given_code: ic={}; self.code_hash={:?}, self.code_cache={}",
                    self.is_transformed_cached(),
                    self.transformed_code_hash,
                    self.transformed_code_cache.pretty()
                );

                self.transformed_code_size = Some(code.len());
                self.transformed_code_cache = code;
            }

            fn cache_given_objectgraph(&mut self, data: Arc<Bytes>) {
                self.object_graph_size = Some(data.len());
                self.object_graph_cache = data;
            }

            fn cache_code_size(&mut self, db: &HashStore) -> bool {
                // TODO: fill out self.code_cache;
                trace!(
                    target: "account",
                    "Account::cache_code_size: ic={}; self.code_hash={:?}, self.code_cache={}",
                    self.is_cached(),
                    self.code_hash,
                    self.code_cache.pretty()
                );
                self.code_size.is_some() || if self.code_hash != BLAKE2B_EMPTY {
                    match db.get(&self.code_hash) {
                        Some(x) => {
                            self.code_size = Some(x.len());
                            true
                        }
                        _ => {
                            warn!(target: "account","Failed reverse get of {}", self.code_hash);
                            false
                        }
                    }
                } else {
                    false
                }
            }

            fn cache_transformed_code_size(&mut self, db: &HashStore) -> bool {
                self.transformed_code_size.is_some() || if self.transformed_code_hash != BLAKE2B_EMPTY {
                    match db.get(&self.transformed_code_hash) {
                        Some(x) => {
                            self.transformed_code_size = Some(x.len());
                            true
                        }
                        _ => {
                            warn!(target: "account","Failed reverse get of {}", self.transformed_code_hash);
                            false
                        }
                    }
                } else {
                    false
                }
            }

            fn cache_objectgraph_size(&mut self, db: &HashStore) -> bool {
                self.object_graph_size.is_some() || if self.objectgraph_hash != BLAKE2B_EMPTY {
                    match db.get(&self.objectgraph_hash) {
                        Some(x) => {
                            self.object_graph_size = Some(x.len());
                            true
                        }
                        _ => {
                            warn!(target: "account","Failed reverse get of {}", self.objectgraph_hash);
                            false
                        }
                    }
                } else {
                    false
                }
            }

            fn is_empty(&self) -> bool {
                assert!(
                    self.storage_is_clean(),
                    "Account::is_empty() may only legally be called when storage is clean."
                );
                self.is_null() && self.storage_root == BLAKE2B_NULL_RLP
            }

            fn is_null(&self) -> bool {
                debug!(target: "vm", "check null: balance = {:?}, nonce = {:?}, code_hash = {:?}",
                    self.balance.is_zero(), self.nonce.is_zero(), self.code_hash == BLAKE2B_EMPTY);
                self.balance.is_zero() && self.nonce.is_zero() && self.code_hash == BLAKE2B_EMPTY
            }

            fn is_basic(&self) -> bool {
                self.code_hash == BLAKE2B_EMPTY && self.transformed_code_hash == BLAKE2B_EMPTY
            }

            fn storage_root(&self) -> Option<&H256> {
                if self.storage_is_clean() {
                    Some(&self.storage_root)
                } else {
                    None
                }
            }

            fn inc_nonce(&mut self) {self.nonce = self.nonce + U256::from(1u8);}

            /// Increase account balance.
            fn add_balance(&mut self, x: &U256) {self.balance = self.balance + *x;}

            /// Decrease account balance.
            /// Panics if balance is less than `x`
            fn sub_balance(&mut self, x: &U256) {
                assert!(self.balance >= *x);
                self.balance = self.balance - *x;
            }

            /// Commit any unsaved code. `code_hash` will always return the hash of the `code_cache` after this.
            fn commit_code(&mut self, db: &mut HashStore) {
                trace!(
                    target: "account",
                    "Commiting code of {:?} - {:?}, {:?}",
                    self,
                    self.code_filth == Filth::Dirty,
                    self.code_cache.is_empty()
                );
                match (self.code_filth == Filth::Dirty, self.code_cache.is_empty(), self.transformed_code_cache.is_empty()) {
                    (true, true, true) => {
                        self.code_size = Some(0);
                        self.code_filth = Filth::Clean;
                    }
                    (true, false, true) => {
                        let mut code = Vec::new();
                        code.append(&mut (self.code_cache.len() as u32).to_vm_bytes());
                        code.extend(&*self.code_cache);
                        db.emplace(
                            self.code_hash.clone(),
                            DBValue::from_slice(code.as_slice()),
                        );
                        self.code_size = Some(self.code_cache.len());
                        self.code_filth = Filth::Clean;
                    }
                    (true, true, false) => {
                        let mut code = Vec::new();
                        code.append(&mut (0 as u32).to_vm_bytes());
                        code.extend(&*self.transformed_code_cache);
                        db.emplace(
                            self.code_hash.clone(),
                            DBValue::from_slice(code.as_slice()),
                        );
                        self.transformed_code_size = Some(self.transformed_code_cache.len());
                        self.code_filth = Filth::Clean;
                    }
                    (true, false, false) => {
                        let mut code = Vec::new();
                        code.append(&mut (self.code_cache.len() as u32).to_vm_bytes());
                        code.extend(&*self.code_cache);
                        code.extend(&*self.transformed_code_cache);
                    
                        self.code_size = Some(self.code_cache.len());

                        db.emplace(
                            self.code_hash.clone(),
                            DBValue::from_slice(code.as_slice()),
                        );
                        self.transformed_code_size = Some(self.transformed_code_cache.len());
                        self.code_filth = Filth::Clean;
                    }
                    (false, _, _) => {}
                }
            }

            /// Export to RLP.
            fn rlp(&self) -> Bytes {
                let mut stream = RlpStream::new_list(4);
                stream.append(&self.nonce);
                stream.append(&self.balance);
                let vm_type: AccType = self.acc_type().into();
                if vm_type == AccType::AVM {
                    //println!("rlp encode using delta_root = {:?}", self.delta_root);
                    stream.append(&self.delta_root);
                } else {
                    stream.append(&self.storage_root);
                }
                stream.append(&self.code_hash);
                println!("nonce = {:?}", self.nonce);
                println!("balance = {:?}", self.balance);
                if vm_type == AccType::AVM {
                    println!("storage_root = {:?}", self.delta_root);
                } else {
                    println!("storage_root = {:?}", self.storage_root);
                }
                println!("code_hash = {:?}", self.code_hash);
                
                stream.out()
            }

            /// Clone account data and dirty storage keys
            fn clone_dirty(&self) -> Self {
                let mut account = self.clone_basic();
                account.storage_changes = self.storage_changes.clone();
                account.code_cache = self.code_cache.clone();
                account.transformed_code_cache = self.transformed_code_cache.clone();
                account
            }

            fn acc_type(&self) -> U256 {
                //self.account_type.clone().into()
                return 0x00.into()
            }

            /// avm should update object graph cache
            fn update_account_cache<B: Backend>(
                &mut self,
                a: &Address,
                require: RequireCache,
                state_db: &B,
                db: &HashStore,
            )
            {
                // always cache object graph and key/value storage root
                println!("try to get object graph from: {:?}", self.delta_root);
                match db.get(&self.delta_root) {
                    Some(data) => {
                        self.object_graph_size = Some(data.len());
                        self.objectgraph_hash = blake2b(&data);
                        self.object_graph_cache = Arc::new(data[..].to_vec());
                    },
                    None => {
                        self.object_graph_size = None;
                        self.objectgraph_hash = BLAKE2B_EMPTY;
                    }
                }

                if let Some(root) = db.get(a) {
                    self.storage_root = root[..].into();
                }

                if let RequireCache::None = require {
                    return;
                }

                if self.is_cached() && self.is_transformed_cached() {
                    return;
                }

                println!("update code cache");
                // if there's already code in the global cache, always cache it localy
                let hash = self.code_hash();
                match state_db.get_cached_code(&hash) {
                    Some(code) => self.cache_given_code(code),
                    None => {
                        match require {
                            RequireCache::None => {}
                            RequireCache::Code => {
                                if let Some(code) = self.cache_code(db) {
                                    // propagate code loaded from the database to
                                    // the global code cache.
                                    state_db.cache_code(hash, code)
                                }
                            }
                            RequireCache::CodeSize => {
                                self.cache_code_size(db);
                            }
                        }
                    }
                }
            }

            /// Prove a storage key's existence or nonexistence in the account's storage
            /// trie.
            /// `storage_key` is the hash of the desired storage key, meaning
            /// this will only work correctly under a secure trie.
            fn prove_storage(
                &self,
                db: &HashStore,
                storage_key: H256,
            ) -> Result<(Vec<Bytes>, H256), Box<TrieError>>
            {
                use trie::{Trie, TrieDB};
                use trie::recorder::Recorder;

                let mut recorder = Recorder::new();

                let trie = TrieDB::new(db, &self.storage_root)?;
                let item: U256 = {
                    let query = (&mut recorder, ::rlp::decode);
                    trie.get_with(&storage_key, query)?
                        .unwrap_or_else(U256::zero)
                };

                Ok((
                    recorder.drain().into_iter().map(|r| r.data).collect(),
                    item.into(),
                ))
            }
        }
    };
}

impl_account!(FVMAccount, true);
impl_account!(AVMAccount, false);

#[derive(Debug)]
pub enum FVMKey {
    Normal(H128),
    Wide(H128),
}

#[derive(Debug, PartialEq)]
pub enum FVMValue {
    Normal(H128),
    Long(H256),
}

impl FVMAccount {
    pub fn storage_at(&self, db: &HashStore, key: &FVMKey) -> trie::Result<FVMValue> {
        if let Some(value) = self.cached_storage_at(key) {
            return Ok(value);
        }
        let db = SecTrieDB::new(db, &self.storage_root)?;

        match key {
            FVMKey::Normal(key) => {
                let item: U128 = db.get_with(key, ::rlp::decode)?.unwrap_or_else(U128::zero);
                let value: H128 = item.into();
                self.storage_cache
                    .0
                    .borrow_mut()
                    .insert(key.clone(), value.clone());
                Ok(FVMValue::Normal(value))
            },
            FVMKey::Wide(key) => {
                let item: H256 = db.get_with(key, ::rlp::decode)?.unwrap_or_else(H256::zero);
                let value: H256 = item.into();
                self.storage_cache
                    .1
                    .borrow_mut()
                    .insert(key.clone(), value.clone());
                Ok(FVMValue::Long(value))
            },
        }
    }

    pub fn cached_storage_at(&self, key: &FVMKey) -> Option<FVMValue> {
        match key {
            FVMKey::Normal(key) => {
                if let Some(value) = self.storage_changes.0.get(key) {
                    return Some(FVMValue::Normal(value.clone()));
                }
                if let Some(value) = self.storage_cache.0.borrow_mut().get_mut(key) {
                    return Some(FVMValue::Normal(value.clone()));
                }
                None
            },
            FVMKey::Wide(key) => {
                if let Some(value) = self.storage_changes.1.get(key) {
                    return Some(FVMValue::Long(value.clone()));
                }
                if let Some(value) = self.storage_cache.1.borrow_mut().get_mut(key) {
                    return Some(FVMValue::Long(value.clone()));
                }
                None
            },
        }
    }

    pub fn set_storage(&mut self, key: FVMKey, value: FVMValue) {
        match key {
            FVMKey::Normal(key) => {
                if let FVMValue::Normal(value) = value {
                    self.storage_changes.0.insert(key, value);
                } else {
                    panic!("unexpected key/value pair: maybe the value is too long");
                }
            },
            FVMKey::Wide(key) => {
                if let FVMValue::Long(value) = value {
                    self.storage_changes.1.insert(key, value);
                } else {
                    panic!("unexpected key/value pair: maybe the value is too short");
                }
            },
        }
    }

    pub fn overwrite_with(&mut self, other: Self) {
        self.balance = other.balance;
        self.nonce = other.nonce;
        self.storage_root = other.storage_root;
        self.code_hash = other.code_hash;
        self.code_filth = other.code_filth;
        self.code_cache = other.code_cache;
        self.code_size = other.code_size;
        self.address_hash = other.address_hash;
        let mut cache = self.storage_cache.0.borrow_mut();
        for (k, v) in other.storage_cache.0.into_inner() {
            cache.insert(k.clone(), v.clone()); //TODO: cloning should not be required here
        }

        let mut cache = self.storage_cache.1.borrow_mut();
        for (k, v) in other.storage_cache.1.into_inner() {
            cache.insert(k.clone(), v.clone()); //TODO: cloning should not be required here
        }
        self.storage_changes = other.storage_changes;
    }
}

impl AVMAccount {
    pub fn storage_at(&self, db: &HashStore, key: &Bytes) -> trie::Result<Bytes> {
        debug!(target: "vm", "get storage: key = {:?}", key);
        if let Some(value) = self.cached_storage_at(key) {
            return Ok(value);
        }
        let db = SecTrieDB::new(db, &self.storage_root)?;

        let value: Vec<u8> = db.get_with(key, ::rlp::decode)?.unwrap_or_else(|| vec![]);
        self.storage_cache
            .borrow_mut()
            .insert(key.clone(), value.clone());
        debug!(target: "vm", "get storage value from db: key = {:?}, value = {:?}", key, value);
        Ok(value)
    }

    pub fn cached_storage_at(&self, key: &Bytes) -> Option<Bytes> {
        debug!(target: "vm", "search storage_changes: {:?}", self.storage_changes);
        if let Some(value) = self.storage_changes.get(key) {
            return Some(value.clone());
        }

        if let Some(value) = self.storage_cache.borrow_mut().get_mut(key) {
            return Some(value.clone());
        }
        None
    }

    pub fn set_storage(&mut self, key: Bytes, value: Bytes) {
        debug!(target: "vm", "pre storage_changes = {:?}", self.storage_changes);
        self.storage_changes.insert(key, value);
        let raw_changes: *mut HashMap<Vec<u8>, Vec<u8>> = unsafe {::std::mem::transmute(&self.storage_changes)};
        debug!(target: "vm", "storage_changes ptr = {:?}", raw_changes);
        debug!(target: "vm", "post storage_changes = {:?}", self.storage_changes);
    }

    pub fn update_root(&mut self) {
        println!("vm_create: {:?}; account type: {:?}", self.vm_create, self.acc_type());
        let vm_type: AccType = self.acc_type().into();
        if self.vm_create && vm_type == AccType::AVM {
            let mut concatenated_root = Vec::new();
            concatenated_root.extend_from_slice(&self.storage_root[..]);
            concatenated_root.extend_from_slice(&self.objectgraph_hash[..]);
            debug!(target: "vm", "concatenated root = {:?}", concatenated_root);
            self.delta_root = blake2b(&concatenated_root);
            println!("updated storage root = {:?}, delta_root = {:?}, code hash = {:?}", 
                self.storage_root, self.delta_root, self.code_hash);
        }
    }

    pub fn save_object_graph(&mut self, address: &Address, db: &mut HashStore) {
        // save object graph
        println!("hash for object graph = {:?}", self.delta_root);
        db.emplace(
            self.delta_root.clone(),
            DBValue::from_slice(self.object_graph_cache.as_slice()),
        );
        // save key/valud storage root
        db.emplace(
            address.clone(), 
            DBValue::from_slice(&self.storage_root[..]));
    }

    pub fn update_object_graph(&mut self, db: &HashStore) {
        match db.get(&self.storage_root) {
            Some(x) => {
                self.object_graph_size = Some(x.len());
                self.object_graph_cache = Arc::new(x[..].to_vec());
            },
            None => {
                self.object_graph_size = None;
            }
        }
    }
}

impl fmt::Debug for FVMAccount {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("FVMAccount")
            .field("balance", &self.balance)
            .field("nonce", &self.nonce)
            .field("code", &self.code())
            .field(
                "storage",
                &self.storage_changes.0.iter().collect::<BTreeMap<_, _>>(),
            )
            .field(
                "storage dword", 
                &self.storage_changes.1.iter().collect::<BTreeMap<_, _>>(),
            )
            .field("storage_root", &self.storage_root)
            .field("empty_but_commit", &self.empty_but_commit)
            .finish()
    }
}

impl fmt::Debug for AVMAccount {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("AVMAccount")
            .field("balance", &self.balance)
            .field("nonce", &self.nonce)
            .field("code", &self.code())
            .field("storage_root", &self.storage_root())
            .field("code_hash", &self.code_hash())
            .field(
                "storage",
                &self.storage_changes.iter().collect::<BTreeMap<_, _>>(),
            )
            .field("storage_root", &self.storage_root)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvdb::MemoryDB;
    use account_db::*;

    #[test]
    fn storage_at() {
        let mut db = MemoryDB::new();
        let mut db = AccountDBMut::new(&mut db, &Address::new());
        let rlp = {
            let mut a = FVMAccount::new_contract(69.into(), 0.into());
            a.set_storage(FVMKey::Normal(H128::from(0x00u64)), FVMValue::Normal(H128::from(0x1234u64)));
            a.commit_storage(&Default::default(), &mut db).unwrap();
            a.init_code(vec![]);
            a.commit_code(&mut db);
            a.rlp()
        };

        let a = FVMAccount::from_rlp(&rlp);
        assert_eq!(
            *a.storage_root().unwrap(),
            "d2e59a50e7414e56da75917275d1542a13fd345bf88a657a4222a0d50ad58868".into()
        );
        let value = a.storage_at(&db.immutable(), &FVMKey::Normal(H128::from(0x00u64))).unwrap();
        assert_eq!(
            value,
            FVMValue::Normal(0x1234u64.into())
        );
        let value = a.storage_at(&db.immutable(), &FVMKey::Normal(0x01u64.into())).unwrap();
        assert_eq!(
            value,
            FVMValue::Normal(H128::default())
        );
    }
}

unsafe impl Sync for FVMAccount {}
unsafe impl Sync for AVMAccount {}