/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::borrow::Cow;
use std::fmt;
use std::fmt::Debug;
use std::ops::Range;
use std::sync::Arc;

use tracing::debug_span;

use super::fold::Fold;
use super::fold::FoldDef;
use super::fold::FoldState;
use crate::errors::ResultExt;
use crate::index::Index;
use crate::lock::READER_LOCK_OPTS;
use crate::lock::ScopedDirLock;
use crate::log::GenericPath;
use crate::log::Log;
use crate::log::LogMetadata;
use crate::log::PRIMARY_START_OFFSET;

const INDEX_FILE_PREFIX: &str = "index2-";
const META_PREFIX: &str = "2-";

/// Definition of an index. It includes: name, function to extract index keys,
/// and how much the index can lag on disk.
#[derive(Clone)]
pub struct IndexDef {
    /// Function to extract index keys from an entry.
    ///
    /// The input is bytes of an entry (ex. the data passed to [`Log::append`]).
    /// The output is an array of index keys. An entry can have zero or more
    /// than one index keys for a same index.
    ///
    /// The output can be an allocated slice of bytes, or a reference to offsets
    /// in the input. See [`IndexOutput`] for details.
    ///
    /// The function should be pure and fast. i.e. It should not use inputs
    /// from other things, like the network, filesystem, or an external random
    /// generator.
    ///
    /// For example, if the [`Log`] is to store git commits, and the index is to
    /// help finding child commits given parent commit hashes as index keys.
    /// This function gets the commit metadata as input. It then parses the
    /// input, and extract parent commit hashes as the output. A git commit can
    /// have 0 or 1 or 2 or even more parents. Therefore the output is a [`Vec`].
    pub(crate) func: Arc<dyn Fn(&[u8]) -> Vec<IndexOutput> + Send + Sync + 'static>,

    /// Name of the index.
    ///
    /// The name will be used as part of the index file name. Therefore do not
    /// use user-generated content here. And do not abuse this by using `..` or `/`.
    ///
    /// When adding new or changing index functions, make sure a different
    /// `name` is used so the existing index won't be reused incorrectly.
    pub(crate) name: Arc<String>,

    /// How many bytes (as counted in the file backing [`Log`]) could be left not
    /// indexed on-disk.
    ///
    /// This is related to [`Index`] implementation detail. Since it's append-only
    /// and needs to write `O(log N)` data for updating a single entry. Allowing
    /// lagged indexes reduces writes and saves disk space.
    ///
    /// The lagged part of the index will be built on-demand in-memory by
    /// [`Log::open`].
    ///
    /// Practically, this correlates to how fast `func` is.
    pub(crate) lag_threshold: u64,
}

/// Output of an index function. Bytes that can be used for lookups.
pub enum IndexOutput {
    /// The index key is a slice, relative to the data entry (ex. input of the
    /// index function).
    ///
    /// Use this if possible. It generates smaller indexes.
    Reference(Range<u64>),

    /// The index key is a separate sequence of bytes unrelated to the input
    /// bytes.
    ///
    /// Use this if the index key is not in the entry. For example, if the entry
    /// is compressed.
    Owned(Box<[u8]>),

    /// Remove all values associated with the key in the index.
    ///
    /// This only affects the index. The entry is not removed in the log.
    Remove(Box<[u8]>),

    /// Remove all values associated with all keys with the given prefix in the index.
    ///
    /// This only affects the index. The entry is not removed in the log.
    RemovePrefix(Box<[u8]>),
}

/// What checksum function to use for an entry.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ChecksumType {
    /// Choose xxhash64 or xxhash32 automatically based on data size.
    Auto,

    /// Use xxhash64 checksum algorithm. Efficient on 64bit platforms.
    Xxhash64,

    /// Use xxhash32 checksum algorithm. It is slower than xxhash64 for 64bit
    /// platforms, but takes less space. Perhaps a good fit when entries are
    /// short.
    Xxhash32,
}

/// Options used to configured how an [`Log`] is opened.
#[derive(Clone)]
pub struct OpenOptions {
    pub(crate) index_defs: Vec<IndexDef>,
    pub(crate) fold_defs: Vec<FoldDef>,
    pub(crate) create: bool,
    pub(crate) checksum_type: ChecksumType,
    pub(crate) flush_filter: Option<FlushFilterFunc>,
    pub(crate) fsync: bool,
    pub(crate) auto_sync_threshold: Option<u64>,
    pub(crate) btrfs_compression: bool,
}

pub type FlushFilterFunc =
    fn(
        &FlushFilterContext,
        &[u8],
    ) -> Result<FlushFilterOutput, Box<dyn std::error::Error + Send + Sync + 'static>>;

/// Potentially useful context for the flush filter function.
pub struct FlushFilterContext<'a> {
    /// The [`log`] being flushed.
    pub log: &'a Log,
}

/// Output of a flush filter.
pub enum FlushFilterOutput {
    /// Insert the entry as is.
    Keep,

    /// Remove this entry.
    Drop,

    /// Replace this entry with the specified new content.
    Replace(Vec<u8>),
}

impl IndexDef {
    /// Create an index definition.
    ///
    /// `index_func` is the function to extract index keys from an entry.
    ///
    /// The input is bytes of an entry (ex. the data passed to [`Log::append`]).
    /// The output is an array of index keys. An entry can have zero or more
    /// than one index keys for a same index.
    ///
    /// The output can be an allocated slice of bytes, or a reference to offsets
    /// in the input. See [`IndexOutput`] for details.
    ///
    /// The function should be pure and fast. i.e. It should not use inputs
    /// from other things, like the network, filesystem, or an external random
    /// generator.
    ///
    /// For example, if the [`Log`] is to store git commits, and the index is to
    /// help finding child commits given parent commit hashes as index keys.
    /// This function gets the commit metadata as input. It then parses the
    /// input, and extract parent commit hashes as the output. A git commit can
    /// have 0 or 1 or 2 or even more parents. Therefore the output is a [`Vec`].
    ///
    /// `name` is the name of the index.
    ///
    /// The name will be used as part of the index file name. Therefore do not
    /// use user-generated content here. And do not abuse this by using `..` or `/`.
    ///
    /// When adding new or changing index functions, make sure a different
    /// `name` is used so the existing index won't be reused incorrectly.
    pub fn new(
        name: impl ToString,
        index_func: impl Fn(&[u8]) -> Vec<IndexOutput> + Send + Sync + 'static,
    ) -> Self {
        Self {
            func: Arc::new(index_func),
            name: Arc::new(name.to_string()),
            // For a typical commit hash index (20-byte). IndexedLog insertion
            // overhead is about 1500 entries per millisecond. For other things
            // the xxhash check might take some time. 500 entries takes <1ms
            // for commit hash index, and might be okay for non-commit-hash
            // indexes. Users should customize the value if the default is not
            // good enough.
            lag_threshold: 25 * 500,
        }
    }

    /// Set how many bytes (as counted in the file backing [`Log`]) could be left
    /// not indexed on-disk.
    ///
    /// This is related to [`Index`] implementation detail. Since it's append-only
    /// and needs to write `O(log N)` data for updating a single entry. Allowing
    /// lagged indexes reduces writes and saves disk space.
    ///
    /// The lagged part of the index will be built on-demand in-memory by
    /// [`Log::open`].
    ///
    /// Practically, this correlates to how fast `func` is.
    pub fn lag_threshold(self, lag_threshold: u64) -> Self {
        Self {
            func: self.func,
            name: self.name,
            lag_threshold,
        }
    }

    /// Name used in log metadata.
    pub(crate) fn metaname(&self) -> String {
        format!("{}{}", META_PREFIX, self.name)
    }

    /// Name used in filesystem.
    pub(crate) fn filename(&self) -> String {
        format!("{}{}", INDEX_FILE_PREFIX, self.name)
    }
}

impl OpenOptions {
    #[allow(clippy::new_without_default)]
    /// Creates a blank new set of options ready for configuration.
    ///
    /// `create` is initially `false`.
    /// `fsync` is initially `false`.
    /// `index_defs` is initially empty.
    /// `auto_sync_threshold` is initially `None`.
    pub fn new() -> Self {
        Self {
            create: false,
            index_defs: Vec::new(),
            fold_defs: Vec::new(),
            checksum_type: ChecksumType::Auto,
            flush_filter: None,
            fsync: false,
            auto_sync_threshold: None,
            btrfs_compression: false,
        }
    }

    /// Set fsync behavior.
    ///
    /// If true, then [`Log::sync`] will use `fsync` to flush log and index
    /// data to the physical device before returning.
    pub fn fsync(mut self, fsync: bool) -> Self {
        self.fsync = fsync;
        self
    }

    /// Add an index function.
    ///
    /// This is a convenient way to define indexes without using [`IndexDef`]
    /// explicitly.
    pub fn index(mut self, name: &'static str, func: fn(&[u8]) -> Vec<IndexOutput>) -> Self {
        self.index_defs.push(IndexDef::new(name, func));
        self
    }

    /// Add a "fold" definition. See [`FoldDef`] and [`Fold`] for details.
    pub fn fold_def(mut self, name: &'static str, create_fold: fn() -> Box<dyn Fold>) -> Self {
        self.fold_defs.push(FoldDef::new(name, create_fold));
        self
    }

    /// Sets index definitions.
    ///
    /// See [`IndexDef::new`] for details.
    pub fn index_defs(mut self, index_defs: Vec<IndexDef>) -> Self {
        self.index_defs = index_defs;
        self
    }

    /// Sets the option for whether creating a new [`Log`] if it does not exist.
    ///
    /// If set to `true`, [`OpenOptions::open`] will create the [`Log`] on demand if
    /// it does not already exist. If set to `false`, [`OpenOptions::open`] will
    /// fail if the log does not exist.
    pub fn create(mut self, create: bool) -> Self {
        self.create = create;
        self
    }

    /// Sets whether to call [`Log::sync`] automatically when the in-memory
    /// buffer exceeds some size threshold.
    /// - `None`: Do not call `sync` automatically.
    /// - `Some(size)`: Call `sync` when the in-memory buffer exceeds `size`.
    /// - `Some(0)`: Call `sync` after every `append` automatically.
    pub fn auto_sync_threshold(mut self, threshold: impl Into<Option<u64>>) -> Self {
        self.auto_sync_threshold = threshold.into();
        self
    }

    /// Sets the checksum type.
    ///
    /// See [`ChecksumType`] for details.
    pub fn checksum_type(mut self, checksum_type: ChecksumType) -> Self {
        self.checksum_type = checksum_type;
        self
    }

    /// Sets the flush filter function.
    ///
    /// The function will be called at [`Log::sync`] time, if there are
    /// changes to the `log` since `open` (or last `sync`) time.
    ///
    /// The filter function can be used to avoid writing content that already
    /// exists in the [`Log`], or rewrite content as needed.
    pub fn flush_filter(mut self, flush_filter: Option<FlushFilterFunc>) -> Self {
        self.flush_filter = flush_filter;
        self
    }

    /// Remove index lagging.
    ///
    /// Used by `RotateLog` to make sure old logs have complete indexes.
    pub(crate) fn with_zero_index_lag(mut self) -> Self {
        for def in self.index_defs.iter_mut() {
            def.lag_threshold = 0;
        }
        self
    }

    /// Enable btrfs aware mode.
    ///
    /// Used by `RotateLog` to take into account transparent btrfs compression.
    pub fn btrfs_compression(mut self, btrfs: bool) -> Self {
        self.btrfs_compression = btrfs;
        self
    }

    /// Construct [`Log`] at given directory. Incrementally build up specified
    /// indexes.
    ///
    /// If the directory does not exist and `create` is set to `true`, it will
    /// be created with essential files populated. After that, an empty [`Log`]
    /// will be returned. Otherwise, `open` will fail.
    ///
    /// See [`IndexDef`] for index definitions. Indexes can be added, removed, or
    /// reordered, as long as a same `name` indicates a same index function.
    /// That is, when an index function is changed, the caller is responsible
    /// for changing the index name.
    ///
    /// Driven by the "immutable by default" idea, together with append-only
    /// properties, this structure is different from some traditional *mutable*
    /// databases backed by the filesystem:
    /// - Data are kind of "snapshotted and frozen" at open time. Mutating
    ///   files do not affect the view of instantiated [`Log`]s.
    /// - Writes are buffered until [`Log::sync`] is called.
    ///
    /// This maps to traditional "database transaction" concepts: a [`Log`] is
    /// always bounded to a transaction. [`Log::sync`] is like committing the
    /// transaction. Dropping the [`Log`] instance is like abandoning a
    /// transaction.
    pub fn open(&self, dir: impl Into<GenericPath>) -> crate::Result<Log> {
        let dir = dir.into();
        match dir.as_opt_path() {
            None => self.create_in_memory(dir),
            Some(fs_dir) => {
                let span = debug_span!("Log::open", dir = &fs_dir.to_string_lossy().as_ref());
                let _guard = span.enter();
                self.open_internal(&dir, None, None)
                    .context(|| format!("in log::OpenOptions::open({:?})", &dir))
            }
        }
    }

    /// Construct an empty in-memory [`Log`] without side-effects on the
    /// filesystem. The in-memory [`Log`] cannot be [`sync`]ed.
    pub(crate) fn create_in_memory(&self, dir: GenericPath) -> crate::Result<Log> {
        assert!(dir.as_opt_path().is_none());
        let result: crate::Result<_> = (|| {
            let meta = LogMetadata::new_with_primary_len(PRIMARY_START_OFFSET);
            let mem_buf = Box::pin(Vec::new());
            let (disk_buf, indexes) = Log::load_log_and_indexes(&dir, &meta, self, &mem_buf, None)?;
            let disk_folds = self.empty_folds();
            let all_folds = disk_folds.clone();
            Ok(Log {
                dir,
                disk_buf,
                mem_buf,
                meta,
                indexes,
                disk_folds,
                all_folds,
                index_out_of_sync: None,
                open_options: self.clone(),
                reader_lock: None,
                change_detector: None,
                prev_btrfs_metadata: None,
            })
        })();

        result.context("in log::OpenOptions::create_in_memory")
    }

    pub(crate) fn open_with_lock(
        &self,
        dir: &GenericPath,
        lock: &ScopedDirLock,
    ) -> crate::Result<Log> {
        self.open_internal(dir, None, Some(lock))
    }

    // "Back-door" version of "open" that allows reusing indexes.
    // Used by [`Log::sync`]. See [`Log::load_log_and_indexes`] for when indexes
    // can be reused.
    pub(crate) fn open_internal(
        &self,
        dir: &GenericPath,
        reuse_indexes: Option<&Vec<Index>>,
        lock: Option<&ScopedDirLock>,
    ) -> crate::Result<Log> {
        let (reader_lock, change_detector) = match dir.as_opt_path() {
            Some(d) => {
                let lock = ScopedDirLock::new_with_options(d, &READER_LOCK_OPTS)?;
                let detector = lock.shared_change_detector()?;
                (Some(lock), Some(detector))
            }
            None => (None, None),
        };
        let create = self.create;

        // Do a lock-less load_or_create_meta to avoid the flock overhead.
        let meta = Log::load_or_create_meta(dir, false).or_else(|err| {
            if create {
                dir.mkdir()
                    .context("cannot mkdir after failing to read metadata")
                    .source(err)?;
                // Make sure check and write happens atomically.
                if lock.is_some() {
                    Log::load_or_create_meta(dir, true)
                } else {
                    let _lock = dir.lock()?;
                    Log::load_or_create_meta(dir, true)
                }
            } else {
                Err(err).context(|| format!("cannot open Log at {:?}", &dir))
            }
        })?;

        let mem_buf = Box::pin(Vec::new());
        let (disk_buf, indexes) =
            Log::load_log_and_indexes(dir, &meta, self, &mem_buf, reuse_indexes)?;
        let disk_folds = self.empty_folds();
        let all_folds = disk_folds.clone();
        let mut log = Log {
            dir: dir.clone(),
            disk_buf,
            mem_buf,
            meta,
            indexes,
            disk_folds,
            all_folds,
            index_out_of_sync: None,
            open_options: self.clone(),
            reader_lock,
            change_detector,
            prev_btrfs_metadata: None,
        };
        log.update_indexes_for_on_disk_entries()?;
        log.update_and_flush_disk_folds()?;
        log.all_folds = log.disk_folds.clone();
        let lagging_index_ids = log.lagging_index_ids();
        if !lagging_index_ids.is_empty() {
            // Update indexes.
            // NOTE: Consider ignoring failures if they are caused by permission
            // issues.
            if let Some(lock) = lock {
                log.flush_lagging_indexes(&lagging_index_ids, lock)?;
                log.dir.write_meta(&log.meta, self.fsync)?;
            } else {
                let lock = dir.lock()?;
                // At this time the Log might be changed on-disk. Reload them.
                return self.open_internal(dir, reuse_indexes, Some(&lock));
            }
        }
        log.update_change_detector_to_match_meta();
        Ok(log)
    }

    pub(crate) fn empty_folds(&self) -> Vec<FoldState> {
        self.fold_defs.iter().map(|def| def.empty_state()).collect()
    }
}

impl IndexOutput {
    pub(crate) fn into_cow(self, data: &[u8]) -> crate::Result<Cow<'_, [u8]>> {
        Ok(match self {
            IndexOutput::Reference(range) => Cow::Borrowed(
                data.get(range.start as usize..range.end as usize)
                    .ok_or_else(|| {
                        let msg = format!(
                            "IndexFunc returned range {:?} but the data only has {} bytes",
                            range,
                            data.len()
                        );
                        let mut err = crate::Error::programming(msg);
                        // If the data is short, add its content to error message.
                        if data.len() < 128 {
                            err = err.message(format!("Data = {:?}", data))
                        }
                        err
                    })?,
            ),
            IndexOutput::Owned(key) => Cow::Owned(key.into_vec()),
            IndexOutput::Remove(_) | IndexOutput::RemovePrefix(_) => {
                return Err(crate::Error::programming(
                    "into_cow does not support Remove or RemovePrefix",
                ));
            }
        })
    }
}

impl fmt::Debug for OpenOptions {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "OpenOptions {{ ")?;
        write!(
            f,
            "index_defs: {:?}, ",
            self.index_defs
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
        )?;
        write!(
            f,
            "fold_defs: {:?}, ",
            self.fold_defs.iter().map(|d| d.name).collect::<Vec<_>>()
        )?;
        write!(f, "fsync: {}, ", self.fsync)?;
        write!(f, "create: {}, ", self.create)?;
        write!(f, "checksum_type: {:?}, ", self.checksum_type)?;
        write!(f, "auto_sync_threshold: {:?}, ", self.auto_sync_threshold)?;
        let flush_filter_desc = match self.flush_filter {
            Some(ref _buf) => "Some(_)",
            None => "None",
        };
        write!(f, "flush_filter: {} }}", flush_filter_desc)?;
        Ok(())
    }
}
