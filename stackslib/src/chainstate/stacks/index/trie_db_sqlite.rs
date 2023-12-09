use std::{io::Write, cell::{RefCell, Ref, RefMut}, path::{PathBuf, Path}};

use rusqlite::{Connection, Transaction, blob::Blob};
use stacks_common::types::chainstate::TrieHash;
use url::Url;

use super::{
    MarfTrieId, Error,
    node::{TriePtr, TrieNodeType}, 
    db::{
        self, DbError, DbTransaction, TransactionalDb, DbTransactionGuard, 
        DbConnection
    },
    trie_db::TrieDb
};

#[derive(Debug, Clone, Copy)]
pub enum SQLitePragma {
    JournalMode(SQLiteJournalMode),
    AutomaticIndex(bool),
    BusyTimeout(u32),
    CacheSize(SQLiteCacheSize),
    CellSizeCheck(bool),
    CheckpointFullSync(bool),
    DeferForeignKeys(bool),
    Encoding(SQLiteEncoding),
    ForeignKeys(bool),
    FullFSync(bool),
    HardHeapLimit(u64),
    IgnoreCheckConstraints(bool),
    JournalSizeLimit(u64),
    LockingMode(SQLiteLockingMode),
    MaxPageCount(u32),
    MmapSize(u64),
    PageCount(u32),
    QueryOnly(bool),
    Synchronous(SQLiteSynchronizationMode),
    TempStore(SQLiteTempStore),
    Threads(u32),
    UserVersion(u32),
    WalAutoCheckpoint(u32),
}

#[derive(Debug, Clone, Copy)]
pub enum SQLiteWalCheckpointMode {
    Passive,
    Full,
    Restart,
    Truncate,
}

#[derive(Debug, Clone, Copy)]
pub enum SQLiteTempStore {
    Default,
    File,
    Memory,
}

#[derive(Debug, Clone, Copy)]
pub enum SQLiteSynchronizationMode {
    Off,
    Normal,
    Full,
    Extra
}

#[derive(Debug, Clone, Copy)]
pub enum SQLiteLockingMode {
    Normal,
    Exclusive,
}

#[derive(Debug, Clone, Copy)]
pub enum SQLiteEncoding {
    Utf8,
    Utf16,
    Utf16LE,
    Utf16BE,
}

#[derive(Debug, Clone, Copy)]
pub enum SQLiteJournalMode {
    Delete,
    Truncate,
    Persist,
    Memory,
    Wal,
    Off,
}

#[derive(Debug, Clone, Copy)]
pub enum SQLiteCacheSize {
    KibiBytes(u32),
    Pages(u32),
}

pub struct SQLiteDbConnection {
    path: PathBuf,
    conn: Connection,
    pragmas: Vec<SQLitePragma>,
}

impl DbConnection for SQLiteDbConnection {
    fn establish<P: AsRef<Path>>(uri: P)-> Result<Self, DbError> {
        let conn = Connection::open(uri.as_ref())?;
        Ok(Self {
            path: uri.as_ref().to_path_buf(),
            conn,
            pragmas: vec![],
        })
    }
}

impl SQLiteDbConnection {
    pub fn set_pragma(mut self, pragma: SQLitePragma) -> Self {
        self.pragmas.push(pragma);
        self
    }
}

pub struct SQLiteTransaction<'conn> {
    tx: Transaction<'conn>
}

impl<'conn> SQLiteTransaction<'conn> {
    pub fn new(conn: &'conn mut Connection) -> Result<Self, DbError> {
        let tx: Transaction<'conn> = conn.transaction()?;
        Ok(Self { tx })
    }
}

impl<'conn> DbTransaction<'conn> for SQLiteTransaction<'conn> {
    fn commit(self) -> Result<(), DbError> {
        self.tx.commit()?;
        Ok(())
    }

    fn rollback(self) -> Result<(), DbError> {
        self.tx.rollback()?;
        Ok(())
    }
}

impl TransactionalDb for SQLiteDbConnection {
    type TxType<'conn> = SQLiteTransaction<'conn>;

    fn transaction<'conn, 'tx, E>(
        &'conn mut self
    ) -> Result<DbTransactionGuard<Self::TxType<'conn>>, E> 
    where 
        E: From<DbError> 
    {
        let tx = SQLiteTransaction::new(&mut self.conn)?;
        let guard = DbTransactionGuard::new(tx);
        Ok(guard)
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        DbError::Database(e.to_string())
    }
}

impl TrieDb for SQLiteDbConnection {

    fn create_tables_if_needed(&self) -> Result<(), Error> {
        todo!()
    }

    fn migrate_tables_if_needed<T: MarfTrieId>(&self) -> Result<u64, Error> {
        todo!()
    }

    fn get_block_identifier<T: MarfTrieId>(&self, bhh: &T) -> Result<u32, Error> {
        todo!()
    }

    fn get_mined_block_identifier<T: MarfTrieId>(&self, bhh: &T) -> Result<u32, Error> {
        todo!()
    }

    fn get_confirmed_block_identifier<T: MarfTrieId>(
        &self,
        bhh: &T,
    ) -> Result<Option<u32>, Error> {
        todo!()
    }

    fn get_unconfirmed_block_identifier<T: MarfTrieId>(
        &self,
        bhh: &T,
    ) -> Result<Option<u32>, Error> {
        todo!()
    }

    fn get_block_hash<T: MarfTrieId>(&self, local_id: u32) -> Result<T, Error> {
        todo!()
    }

    fn write_trie_blob<T: MarfTrieId>(
        &self,
        block_hash: &T,
        data: &[u8],
    ) -> Result<u32, Error> {
        todo!()
    }

    fn update_external_trie_blob<T: MarfTrieId>(
        &self,
        block_hash: &T,
        offset: u64,
        length: u64,
        block_id: u32,
    ) -> Result<u32, Error> {
        todo!()
    }

    fn write_external_trie_blob<T: MarfTrieId>(
        &self,
        block_hash: &T,
        offset: u64,
        length: u64,
    ) -> Result<u32, Error> {
        todo!()
    }

    fn write_trie_blob_to_mined<T: MarfTrieId>(
        &self,
        block_hash: &T,
        data: &[u8],
    ) -> Result<u32, Error> {
        todo!()
    }

    fn write_trie_blob_to_unconfirmed<T: MarfTrieId>(
        &self,
        block_hash: &T,
        data: &[u8],
    ) -> Result<u32, Error> {
        todo!()
    }

    fn open_trie_blob<'a>(conn: &'a Connection, block_id: u32) -> Result<Blob<'a>, Error> {
        todo!()
    }

    fn open_trie_blob_readonly<'a>(conn: &'a Connection, block_id: u32) -> Result<Blob<'a>, Error> {
        todo!()
    }

    fn read_node_hash_bytes<W: Write>(
        &self,
        w: &mut W,
        block_id: u32,
        ptr: &TriePtr,
    ) -> Result<(), Error> {
        todo!()
    }

    fn read_node_hash_bytes_by_bhh<W: Write, T: MarfTrieId>(
        &self,
        w: &mut W,
        bhh: &T,
        ptr: &TriePtr,
    ) -> Result<(), Error> {
        todo!()
    }

    fn read_node_type(
        &self,
        block_id: u32,
        ptr: &TriePtr,
    ) -> Result<(TrieNodeType, TrieHash), Error> {
        todo!()
    }

    fn read_node_type_nohash(
        &self,
        block_id: u32,
        ptr: &TriePtr,
    ) -> Result<TrieNodeType, Error> {
        todo!()
    }

    fn get_external_trie_offset_length(
        &self,
        block_id: u32,
    ) -> Result<(u64, u64), Error> {
        todo!()
    }

    fn get_external_trie_offset_length_by_bhh<T: MarfTrieId>(
        &self,
        bhh: &T,
    ) -> Result<(u64, u64), Error> {
        todo!()
    }

    fn get_external_blobs_length(&self) -> Result<u64, Error> {
        todo!()
    }

    fn detect_partial_migration(&self) -> Result<bool, Error> {
        todo!()
    }

    fn set_migrated(&self) -> Result<(), Error> {
        todo!()
    }

    fn get_node_hash_bytes(
        &self,
        block_id: u32,
        ptr: &TriePtr,
    ) -> Result<TrieHash, Error> {
        todo!()
    }

    fn get_node_hash_bytes_by_bhh<T: MarfTrieId>(
        &self,
        bhh: &T,
        ptr: &TriePtr,
    ) -> Result<TrieHash, Error> {
        todo!()
    }

    fn tx_lock_bhh_for_extension<T: MarfTrieId>(
        tx: &Connection,
        bhh: &T,
        unconfirmed: bool,
    ) -> Result<bool, Error> {
        todo!()
    }

    fn lock_bhh_for_extension<T: MarfTrieId>(
        tx: &Transaction,
        bhh: &T,
        unconfirmed: bool,
    ) -> Result<bool, Error> {
        todo!()
    }

    fn count_blocks(&self) -> Result<u32, Error> {
        todo!()
    }

    fn is_unconfirmed_block(&self, block_id: u32) -> Result<bool, Error> {
        todo!()
    }

    fn drop_lock<T: MarfTrieId>(&self, bhh: &T) -> Result<(), Error> {
        todo!()
    }

    fn drop_unconfirmed_trie<T: MarfTrieId>(&self, bhh: &T) -> Result<(), Error> {
        todo!()
    }

    fn clear_lock_data(&self) -> Result<(), Error> {
        todo!()
    }

    fn clear_tables(tx: &Transaction) -> Result<(), Error> {
        todo!()
    }

    #[cfg(test)]
    fn read_all_block_hashes_and_roots<T: MarfTrieId>(
        &self,
    ) -> Result<Vec<(TrieHash, T)>, Error> {
        todo!()
    }
}