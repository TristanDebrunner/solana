use crate::bank::BankError;
use crate::bank::Result;
use crate::checkpoint::Checkpoint;
use crate::counter::Counter;
use crate::status_deque::{StatusDeque, StatusDequeError};
use bincode::{self, deserialize_from, serialize, serialize_into, serialized_size};
use hashbrown::{HashMap, HashSet};
use log::Level;
use memmap::MmapMut;
use solana_sdk::account::Account;
use solana_sdk::hash::{hash, Hash};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::Transaction;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fs::{create_dir_all, remove_dir_all, rename, File, OpenOptions};
use std::io::prelude::*;
use std::io::{self, Seek, SeekFrom};
use std::mem::size_of;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::{Mutex, RwLock};

#[derive(Default)]
pub struct ErrorCounters {
    pub account_not_found: usize,
    pub account_in_use: usize,
    pub last_id_not_found: usize,
    pub reserve_last_id: usize,
    pub insufficient_funds: usize,
    pub duplicate_signature: usize,
}

//
// A persistent account is 2 files:
//  account_path/--+
//                 +-- data  <== concatenated instances of
//                                    usize length
//                                    account data

const ACCOUNT_PATHS: [&str; 2] = ["/media/nvme0/accounts", "/media/nvme1/accounts"];
const ACCOUNT_DATA_FILE: &str = "data";

// Start accounts size to be mmaped
const DATA_FILE_START_SIZE: u64 = 64 * 1024 * 1024;
const DATA_FILE_INC_SIZE: u64 = 4 * 1024 * 1024;

const SIZEOF_USIZE: usize = size_of::<usize>();

#[allow(clippy::needless_pass_by_value)]
fn err_bincode_to_io(e: Box<bincode::ErrorKind>) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

fn to_dir_index(n: u8) -> usize {
    (n >> 7) as usize
}

macro_rules! get_path_main {
    ($path: expr) => {{
        format!("{}/{}/main", $path, std::process::id())
    }};
}

macro_rules! get_path_checkpoint {
    ($path: expr, $count: expr) => {{
        format!("{}/{}/chk/{}", $path, std::process::id(), $count)
    }};
}

#[derive(Debug)]
pub struct AccountRW {
    data: File,
    map: MmapMut,
    current_offset: u64,
    file_size: u64,
}

impl AccountRW {
    pub fn new(account_path: &str, create: bool) -> Self {
        let path = get_path_main!(account_path);
        let path = Path::new(&path);

        if create {
            let _ignored = remove_dir_all(path);
            create_dir_all(path).expect("Create directory failed");
        }

        let mut data = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .open(path.join(ACCOUNT_DATA_FILE))
            .expect("Unable to open account data");

        data.seek(SeekFrom::Start(DATA_FILE_START_SIZE)).unwrap();
        data.write_all(&[0]).unwrap();
        data.seek(SeekFrom::Start(0)).unwrap();
        data.flush().unwrap();
        let map = unsafe { MmapMut::map_mut(&data).expect("failed to map the data file") };

        AccountRW {
            data,
            map,
            current_offset: 0,
            file_size: DATA_FILE_START_SIZE,
        }
    }

    fn usize_at(&self, at: usize) -> io::Result<usize> {
        deserialize_from(&self.map[at..at + SIZEOF_USIZE]).map_err(err_bincode_to_io)
    }

    fn grow_file(&mut self) -> io::Result<()> {
        let end = self.file_size + DATA_FILE_INC_SIZE;
        drop(&self.map);
        self.data.seek(SeekFrom::Start(end))?;
        self.data.write_all(&[0])?;
        self.data.seek(SeekFrom::Start(0))?;
        self.data.flush()?;
        self.map = unsafe { MmapMut::map_mut(&self.data)? };
        self.file_size = end;
        Ok(())
    }

    pub fn get_account(&self, index: usize) -> io::Result<Account> {
        let len = self.usize_at(index)?;
        let at = index + SIZEOF_USIZE;
        deserialize_from(&self.map[at..at + len]).map_err(err_bincode_to_io)
    }

    pub fn write_account(&mut self, account: &Account, offset: usize) -> io::Result<(usize)> {
        let len = serialized_size(&account).map_err(err_bincode_to_io)? as usize;
        let mut data_at: usize = offset;

        if offset == std::usize::MAX {
            data_at = self.current_offset as usize;
            self.current_offset = (data_at + len + SIZEOF_USIZE) as u64;
            if self.current_offset >= self.file_size {
                self.grow_file()?;
            }
        } else {
            let cur_len = self.usize_at(data_at)?;
            assert!(cur_len >= len);
        }
        serialize_into(&mut self.map[data_at..], &len).map_err(err_bincode_to_io)?;
        serialize_into(&mut self.map[data_at + SIZEOF_USIZE..], &account)
            .map_err(err_bincode_to_io)?;

        Ok(data_at)
    }
}

/// This structure handles the load/store of the accounts
pub struct AccountsDB {
    /// Mapping of known public keys/IDs to index into file
    index: Vec<HashMap<Pubkey, usize>>,

    /// Persistent account storage
    accounts_rw: Vec<AccountRW>,

    /// list of prior states
    checkpoints: VecDeque<(Vec<HashMap<Pubkey, usize>>, Vec<AccountRW>, u64)>,

    /// The number of transactions the bank has processed without error since the
    /// start of the ledger.
    transaction_count: u64,
}

/// This structure handles synchronization for db
pub struct Accounts {
    pub accounts_db: RwLock<AccountsDB>,

    /// set of accounts which are currently in the pipeline
    account_locks: Mutex<HashSet<Pubkey>>,
}

impl Default for Accounts {
    fn default() -> Self {
        Self {
            account_locks: Mutex::new(HashSet::new()),
            accounts_db: RwLock::new(AccountsDB::new()),
        }
    }
}

impl AccountsDB {
    pub fn new() -> AccountsDB {
        let mut index: Vec<HashMap<Pubkey, usize>> = vec![];
        let mut accounts_rw: Vec<AccountRW> = vec![];
        ACCOUNT_PATHS.into_iter().for_each(|p| {
            accounts_rw.push(AccountRW::new(p, true));
            index.push(HashMap::new());
        });
        AccountsDB {
            index,
            accounts_rw,
            checkpoints: VecDeque::new(),
            transaction_count: 0,
        }
    }

    pub fn get_filtered_accounts(&self, filter: &Fn(&Pubkey) -> bool) -> Vec<Account> {
        let mut accounts: Vec<Account> = vec![];
        for (dir, index) in self.index.iter().enumerate() {
            let reader = &self.accounts_rw[dir];
            for (_, offset) in index.iter() {
                let account = reader.get_account(*offset).unwrap();
                if filter(&account.owner) {
                    accounts.push(account.clone());
                }
            }
        }
        accounts
    }

    pub fn keys(&self) -> Vec<Pubkey> {
        let mut pubkeys: Vec<Pubkey> = vec![];
        for index in self.index.iter() {
            pubkeys.extend(index.keys().cloned());
        }
        pubkeys
    }

    pub fn hash_internal_state(&self) -> Hash {
        let mut ordered_accounts = BTreeMap::new();

        // only hash internal state of the part being voted upon, i.e. since last
        // checkpoint
        for (dir, index) in self.index.iter().enumerate() {
            let reader = &self.accounts_rw[dir];
            for (pubkey, offset) in index.iter() {
                let account = reader.get_account(*offset).unwrap();
                ordered_accounts.insert(*pubkey, account.clone());
            }
        }

        hash(&serialize(&ordered_accounts).unwrap())
    }

    pub fn load(&self, pubkey: &Pubkey) -> Option<Account> {
        let dir = to_dir_index(pubkey.as_ref()[0]);
        let reader = &self.accounts_rw[dir];
        if let Some(index) = self.index[dir].get(pubkey) {
            let account = reader.get_account(*index).unwrap();
            return Some(account);
        }

        for (index, accounts_rw, _) in &self.checkpoints {
            let reader = &accounts_rw[dir];
            if let Some(index) = index[dir].get(pubkey) {
                let account = reader.get_account(*index).unwrap();
                return Some(account);
            }
        }
        None
    }

    pub fn store(&mut self, pubkey: &Pubkey, account: &Account) {
        let dir = to_dir_index(pubkey.as_ref()[0]);
        let writer = &mut self.accounts_rw[dir];
        if account.tokens == 0 && self.checkpoints.is_empty() {
            // purge if balance is 0 and no checkpoints
            self.index[dir].remove(pubkey);
        } else {
            let mut offset: usize;
            if let Some(index) = self.index[dir].get(pubkey) {
                offset = *index;
            } else {
                offset = std::usize::MAX;
            }
            if account.tokens == 0 {
                offset = writer.write_account(&Account::default(), offset).unwrap();
            } else {
                offset = writer.write_account(&account, offset).unwrap();
            }
            self.index[dir].insert(*pubkey, offset);
        }
    }
    pub fn store_accounts(
        &mut self,
        txs: &[Transaction],
        res: &[Result<()>],
        loaded: &[Result<Vec<Account>>],
    ) {
        for (i, racc) in loaded.iter().enumerate() {
            if res[i].is_err() || racc.is_err() {
                continue;
            }

            let tx = &txs[i];
            let acc = racc.as_ref().unwrap();
            for (key, account) in tx.account_keys.iter().zip(acc.iter()) {
                self.store(key, account);
            }
        }
    }
    fn load_account(
        &self,
        tx: &Transaction,
        last_ids: &mut StatusDeque<Result<()>>,
        max_age: usize,
        error_counters: &mut ErrorCounters,
    ) -> Result<Vec<Account>> {
        let account = self.load(&tx.account_keys[0]);
        let mut called_accounts: Vec<Account> = vec![account.clone().unwrap_or_default()];
        // Copy all the accounts
        if tx.signatures.is_empty() && tx.fee != 0 {
            Err(BankError::MissingSignatureForFee)
        } else if account.is_none() {
            error_counters.account_not_found += 1;
            Err(BankError::AccountNotFound)
        } else if account.unwrap().tokens < tx.fee {
            error_counters.insufficient_funds += 1;
            Err(BankError::InsufficientFundsForFee)
        } else {
            if !last_ids.check_entry_id_age(tx.last_id, max_age) {
                error_counters.last_id_not_found += 1;
                return Err(BankError::LastIdNotFound);
            }

            // There is no way to predict what program will execute without an error
            // If a fee can pay for execution then the program will be scheduled
            last_ids
                .reserve_signature_with_last_id(&tx.last_id, &tx.signatures[0])
                .map_err(|err| match err {
                    StatusDequeError::LastIdNotFound => {
                        error_counters.reserve_last_id += 1;
                        BankError::LastIdNotFound
                    }
                    StatusDequeError::DuplicateSignature => {
                        error_counters.duplicate_signature += 1;
                        BankError::DuplicateSignature
                    }
                })?;

            called_accounts.extend(
                tx.account_keys
                    .iter()
                    .skip(1)
                    .map(|key| self.load(key).unwrap_or_default())
                    .collect::<Vec<Account>>(),
            );
            called_accounts[0].tokens -= tx.fee;
            Ok(called_accounts)
        }
    }
    fn load_accounts(
        &self,
        txs: &[Transaction],
        last_ids: &mut StatusDeque<Result<()>>,
        results: Vec<Result<()>>,
        max_age: usize,
        error_counters: &mut ErrorCounters,
    ) -> Vec<(Result<Vec<Account>>)> {
        txs.iter()
            .zip(results.into_iter())
            .map(|etx| match etx {
                (tx, Ok(())) => self.load_account(tx, last_ids, max_age, error_counters),
                (_, Err(e)) => Err(e),
            })
            .collect()
    }
    pub fn increment_transaction_count(&mut self, tx_count: usize) {
        self.transaction_count += tx_count as u64
    }
    pub fn transaction_count(&self) -> u64 {
        self.transaction_count
    }
}

impl Accounts {
    pub fn keys(&self) -> Vec<Pubkey> {
        self.accounts_db.read().unwrap().keys()
    }

    /// Slow because lock is held for 1 operation insted of many
    pub fn load_slow(&self, pubkey: &Pubkey) -> Option<Account> {
        self.accounts_db.read().unwrap().load(pubkey)
    }
    /// Slow because lock is held for 1 operation insted of many
    pub fn store_slow(&self, pubkey: &Pubkey, account: &Account) {
        self.accounts_db.write().unwrap().store(pubkey, account)
    }
    fn lock_account(
        account_locks: &mut HashSet<Pubkey>,
        keys: &[Pubkey],
        error_counters: &mut ErrorCounters,
    ) -> Result<()> {
        // Copy all the accounts
        for k in keys {
            if account_locks.contains(k) {
                error_counters.account_in_use += 1;
                return Err(BankError::AccountInUse);
            }
        }
        for k in keys {
            account_locks.insert(*k);
        }
        Ok(())
    }

    fn unlock_account(tx: &Transaction, result: &Result<()>, account_locks: &mut HashSet<Pubkey>) {
        match result {
            Err(BankError::AccountInUse) => (),
            _ => {
                for k in &tx.account_keys {
                    account_locks.remove(k);
                }
            }
        }
    }
    pub fn hash_internal_state(&self) -> Hash {
        self.accounts_db.read().unwrap().hash_internal_state()
    }

    /// This function will prevent multiple threads from modifying the same account state at the
    /// same time
    #[must_use]
    pub fn lock_accounts(&self, txs: &[Transaction]) -> Vec<Result<()>> {
        let mut account_locks = self.account_locks.lock().unwrap();
        let mut error_counters = ErrorCounters::default();
        let rv = txs
            .iter()
            .map(|tx| Self::lock_account(&mut account_locks, &tx.account_keys, &mut error_counters))
            .collect();
        if error_counters.account_in_use != 0 {
            inc_new_counter_info!(
                "bank-process_transactions-account_in_use",
                error_counters.account_in_use
            );
        }
        rv
    }

    /// Once accounts are unlocked, new transactions that modify that state can enter the pipeline
    pub fn unlock_accounts(&self, txs: &[Transaction], results: &[Result<()>]) {
        let mut account_locks = self.account_locks.lock().unwrap();
        debug!("bank unlock accounts");
        txs.iter()
            .zip(results.iter())
            .for_each(|(tx, result)| Self::unlock_account(tx, result, &mut account_locks));
    }

    pub fn load_accounts(
        &self,
        txs: &[Transaction],
        last_ids: &mut StatusDeque<Result<()>>,
        results: Vec<Result<()>>,
        max_age: usize,
        error_counters: &mut ErrorCounters,
    ) -> Vec<(Result<Vec<Account>>)> {
        self.accounts_db.read().unwrap().load_accounts(
            txs,
            last_ids,
            results,
            max_age,
            error_counters,
        )
    }

    pub fn store_accounts(
        &self,
        txs: &[Transaction],
        res: &[Result<()>],
        loaded: &[Result<Vec<Account>>],
    ) {
        self.accounts_db
            .write()
            .unwrap()
            .store_accounts(txs, res, loaded)
    }

    pub fn increment_transaction_count(&self, tx_count: usize) {
        self.accounts_db
            .write()
            .unwrap()
            .increment_transaction_count(tx_count)
    }
    pub fn transaction_count(&self) -> u64 {
        self.accounts_db.read().unwrap().transaction_count()
    }
    pub fn checkpoint(&self) {
        self.accounts_db.write().unwrap().checkpoint()
    }
    pub fn rollback(&self) {
        self.accounts_db.write().unwrap().rollback()
    }
    pub fn purge(&self, depth: usize) {
        self.accounts_db.write().unwrap().purge(depth)
    }
    pub fn depth(&self) -> usize {
        self.accounts_db.read().unwrap().depth()
    }
}

impl Checkpoint for AccountsDB {
    fn checkpoint(&mut self) {
        let count = self.checkpoints.len();
        let tx_count = self.transaction_count();
        let mut index: Vec<HashMap<Pubkey, usize>> = vec![];
        let mut accounts_rw: Vec<AccountRW> = vec![];
        ACCOUNT_PATHS.into_iter().for_each(|p| {
            let from_path = get_path_main!(p);
            let to_path = get_path_checkpoint!(p, count);
            let from_path = Path::new(&from_path);
            let to_path = Path::new(&to_path);
            let _ignored = remove_dir_all(to_path);
            create_dir_all(to_path).expect("Create directory failed");
            rename(from_path, to_path).expect("rename directory failed");

            accounts_rw.push(AccountRW::new(p, true));
            index.push(HashMap::new());
        });
        std::mem::swap(&mut self.index, &mut index);
        std::mem::swap(&mut self.accounts_rw, &mut accounts_rw);

        self.checkpoints.push_front((index, accounts_rw, tx_count));
    }

    fn rollback(&mut self) {
        let (index, accounts_rw, transaction_count) = self.checkpoints.pop_front().unwrap();
        let count = self.checkpoints.len();
        ACCOUNT_PATHS.into_iter().for_each(|p| {
            let to_path = get_path_main!(p);
            let from_path = get_path_checkpoint!(p, count);
            let to_path = Path::new(&to_path);
            let from_path = Path::new(&from_path);
            let _ignored = remove_dir_all(to_path);
            create_dir_all(to_path).expect("Create directory failed");
            rename(from_path, to_path).expect("rename directory failed");
        });
        self.index = index;
        self.accounts_rw = accounts_rw;
        self.transaction_count = transaction_count;
    }

    fn purge(&mut self, depth: usize) {
        fn merge(
            into_index: &mut Vec<HashMap<Pubkey, usize>>,
            into_accounts_rw: &mut Vec<AccountRW>,
            purge_index: &mut Vec<HashMap<Pubkey, usize>>,
            purge_accounts_rw: &mut Vec<AccountRW>,
        ) {
            for (dir, index) in purge_index.iter().enumerate() {
                let reader = &purge_accounts_rw[dir];
                let writer = &mut into_accounts_rw[dir];
                for (pubkey, offset) in index.iter() {
                    if let Some(_index) = into_index[dir].get(pubkey) {
                        continue;
                    }
                    let account = reader.get_account(*offset).unwrap();
                    if account.tokens != 0 {
                        let offset = writer.write_account(&account, std::usize::MAX).unwrap();
                        into_index[dir].insert(*pubkey, offset);
                    }
                }
            }
            let mut pubkeys: Vec<(usize, Pubkey)> = vec![];
            for (dir, index) in into_index.iter().enumerate() {
                let reader = &into_accounts_rw[dir];
                for (pubkey, offset) in index.iter() {
                    let account = reader.get_account(*offset).unwrap();
                    if account.tokens == 0 {
                        pubkeys.push((dir, pubkey.clone()));
                    }
                }
            }
            for (dir, pubkey) in pubkeys.iter() {
                into_index[*dir].remove(pubkey);
            }
        }

        while self.depth() > depth {
            let (mut purge_index, mut purge_accounts_rw, _) = self.checkpoints.pop_back().unwrap();

            if let Some((into_index, into_accounts_rw, _)) = self.checkpoints.back_mut() {
                merge(
                    into_index,
                    into_accounts_rw,
                    &mut purge_index,
                    &mut purge_accounts_rw,
                );
                continue;
            }
            merge(
                &mut self.index,
                &mut self.accounts_rw,
                &mut purge_index,
                &mut purge_accounts_rw,
            );
        }
    }
    fn depth(&self) -> usize {
        self.checkpoints.len()
    }
}

#[cfg(test)]
mod tests {
    // TODO: all the bank tests are bank specific, issue: 2194
    use super::*;
    use rand::{thread_rng, Rng};
    use solana_sdk::signature::{Keypair, KeypairUtil};

    fn get_accounts() -> &'static Accounts {
        static mut ACCOUNTS: Option<Accounts> = None;
        static INIT_ACCOUNTS: std::sync::Once = std::sync::ONCE_INIT;
        unsafe {
            INIT_ACCOUNTS.call_once(|| {
                ACCOUNTS = Some(Accounts::default());
            });
            ACCOUNTS.as_ref().unwrap()
        }
    }

    fn create_account(accounts: &Accounts, pubkeys: &mut Vec<Pubkey>, num: usize) {
        for t in 0..num {
            let pubkey = Keypair::new().pubkey();
            let mut default_account = Account::default();
            pubkeys.push(pubkey.clone());
            default_account.tokens = (t + 1) as u64;
            assert!(accounts.load_slow(&pubkey).is_none());
            accounts.store_slow(&pubkey, &default_account);
        }
    }

    fn update_accounts(accounts: &Accounts, pubkeys: Vec<Pubkey>, range: usize) {
        for _ in 1..1000 {
            let idx = thread_rng().gen_range(0, range);
            if let Some(mut account) = accounts.load_slow(&pubkeys[idx]) {
                account.tokens = account.tokens - 1;
                accounts.store_slow(&pubkeys[idx], &account);
                if account.tokens == 0 {
                    assert!(accounts.load_slow(&pubkeys[idx]).is_none());
                } else {
                    let mut default_account = Account::default();
                    default_account.tokens = account.tokens;
                    assert_eq!(compare_account(&default_account, &account), true);
                }
            }
        }
    }

    fn compare_account(account1: &Account, account2: &Account) -> bool {
        if account1.userdata != account2.userdata
            || account1.owner != account2.owner
            || account1.executable != account2.executable
            || account1.loader != account2.loader
            || account1.tokens != account2.tokens
        {
            return false;
        }
        true
    }

    #[test]
    fn test_account_one() {
        let accounts = get_accounts();
        let mut pubkeys: Vec<Pubkey> = vec![];
        create_account(accounts, &mut pubkeys, 1);
        let account = accounts.load_slow(&pubkeys[0]).unwrap();
        let mut default_account = Account::default();
        default_account.tokens = 1;
        assert_eq!(compare_account(&default_account, &account), true);
    }

    #[test]
    fn test_account_many() {
        let accounts = get_accounts();
        let mut pubkeys: Vec<Pubkey> = vec![];
        create_account(accounts, &mut pubkeys, 100);
        for _ in 1..100 {
            let idx = thread_rng().gen_range(0, 99);
            let account = accounts.load_slow(&pubkeys[idx]).unwrap();
            let mut default_account = Account::default();
            default_account.tokens = (idx + 1) as u64;
            assert_eq!(compare_account(&default_account, &account), true);
        }
    }

    #[test]
    fn test_account_update() {
        let accounts = get_accounts();
        let mut pubkeys: Vec<Pubkey> = vec![];
        create_account(accounts, &mut pubkeys, 100);
        update_accounts(accounts, pubkeys, 99);
    }

    #[test]
    #[ignore]
    fn test_grow_file() {
        let accounts = get_accounts();
        let mut pubkeys: Vec<Pubkey> = vec![];
        let account = Account::default();
        let len = serialized_size(&account).unwrap();
        let num_accounts: usize = ((DATA_FILE_START_SIZE / len) * 2) as usize;
        create_account(accounts, &mut pubkeys, num_accounts);
        update_accounts(accounts, pubkeys, num_accounts);
    }
}
