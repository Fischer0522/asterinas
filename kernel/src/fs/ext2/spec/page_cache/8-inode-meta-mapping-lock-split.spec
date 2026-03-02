[PROMPT]
Refactor Ext2 inode state to split metadata and block-mapping lock domains so
that PageCacheBackend callbacks never self-deadlock with inode operations.

Provide modifications to `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_write_begin        → fs/ext2/inode.c:928
ext2_write_end          → fs/ext2/inode.c:939
ext2_write_failed       → fs/ext2/inode.c:59
ext2_setsize            → fs/ext2/inode.c:1275
ext2_get_block          → fs/ext2/inode.c:783
ext2_get_blocks         → fs/ext2/inode.c:624
__ext2_truncate_blocks  → fs/ext2/inode.c:1172
block_truncate_page     → fs/buffer.c:2654
truncate_setsize        → mm/truncate.c:812
generic_buffers_fsync   → fs/buffer.c:646

Locking rationale (conceptual):
- Linux uses separate lock domains for inode metadata and page-cache/mapping
  invalidation (i_rwsem vs truncate/mapping locks). The split avoids re-entrancy
  deadlocks when page cache code calls into get_block paths.

Asterinas references:
- Current upread-based workaround → kernel/src/fs/ext2/spec/page_cache/7-upread-lock-refactor.spec
- PageCache contract and callback behavior → kernel/src/fs/utils/page_cache.rs
- ext2_old separate backend precedent (dual-write mapping) → kernel/src/fs/ext2_old/inode.rs

[RELY]
```rust
use super::prelude::*;
use super::fs::Ext2;
use super::utils::Dirty;
```

```rust
/// PageCache infrastructure.
pub struct PageCache { /* ... */ }

/// Backend trait used by PageCache for on-demand fill and writeback.
///
/// IMPORTANT: Implementations may be called while PageCache holds internal
/// mutexes. Therefore, backend methods must not participate in lock cycles with
/// the caller's locks.
pub trait PageCacheBackend: Sync + Send {
    /// Reads page `idx` (logical block index) into `frame`.
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;
    /// Writes page `idx` (logical block index) from `frame` back to disk.
    fn write_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;
    /// Returns the number of pages (logical blocks) addressable by this backend.
    fn npages(&self) -> usize;
}
```

```rust
/// The Ext2 inode public handle.
#[derive(Debug)]
pub struct Inode {
    /// 1-based inode number.
    ino: u32,
    /// Inode type (file, directory, symlink, etc.). Immutable after load.
    type_: InodeType,
    /// Split-lock mutable state and per-inode PageCache.
    inner: InodeInner,
    /// Index of the block group this inode belongs to.
    block_group_idx: usize,
    /// Weak reference to the owning filesystem instance.
    fs: Weak<Ext2>,
    /// Extended attributes (only for some inode types).
    xattr: Option<RwMutex<Xattr>>,
    /// VFS extension data.
    extension: Extension,
}
```

```rust
/// Mutable inode state container.
///
/// This replaces the old `RwMutex<InodeInner>` monolithic lock with two
/// independent lock domains:
/// - `meta`: inode metadata (i_size/times/perm/uid/gid/flags/...)
/// - `mapping`: block mapping (i_blocks + i_block[15])
///
/// Rationale: PageCacheBackend callbacks must be able to resolve mapping while a
/// thread holds the inode metadata write lock. Splitting locks removes the need
/// for multi-phase upread/upgrade choreography.
#[derive(Debug)]
pub struct InodeInner {
    /// Metadata lock domain (Linux i_rwsem analogue).
    meta: RwMutex<InodeMeta>,
    /// Mapping lock domain (Linux truncate/mapping lock analogue).
    mapping: RwMutex<InodeMapping>,
    /// Per-inode PageCache for file/directory content.
    page_cache: PageCache,
}
```

```rust
/// Inode metadata domain.
#[derive(Debug)]
pub struct InodeMeta {
    /// In-memory meta descriptor with dirty tracking.
    desc: Dirty<InodeMetaDesc>,
    /// Whether this inode has been freed (unlinked + nlink=0).
    is_freed: bool,
}

/// In-memory inode metadata (raw on-disk view excluding i_blocks/i_block[]).
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeMetaDesc {
    /// Permission bits (includes file type bits as stored on disk; see mode()).
    perm: FilePerm,
    /// Owner uid.
    uid: u32,
    /// Owner gid.
    gid: u32,
    /// File size in bytes.
    size: u64,
    /// Last access time.
    atime: Duration,
    /// Last status change time.
    ctime: Duration,
    /// Last modification time.
    mtime: Duration,
    /// Deletion time.
    dtime: Duration,
    /// Hard link count.
    links_count: u16,
    /// Inode flags (immutable/dirsync/etc).
    flags: FileFlags,
    /// File ACL / xattr block pointer (ext2 field i_file_acl).
    file_acl: u32,
    /// Inode generation number.
    generation: u32,
}
```

```rust
/// Inode block mapping domain.
///
/// This domain owns the on-disk `i_blocks` and `i_block[15]` fields.
/// For regular files/directories, `i_block[]` is the block pointer tree.
/// For fast symlinks and device inodes, `i_block[]` stores payload/encoding but
/// must still be persisted exactly.
#[derive(Debug)]
pub struct InodeMapping {
    /// Mapping descriptor with dirty tracking.
    desc: Dirty<InodeMappingDesc>,
}

/// In-memory mapping descriptor (raw on-disk view of i_blocks + i_block[]).
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeMappingDesc {
    /// Allocated block count in 512-byte sectors (ext2 i_blocks).
    blocks: u32,
    /// The ext2 i_block[15] array.
    ///
    /// Regular file/dir: 12 direct + single/double/triple indirect pointers.
    /// Fast symlink: inline payload bytes.
    /// Device inode: encoded rdev.
    block_ptrs: [u32; 15],
}
```

```rust
/// On-disk inode structure.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct RawInode {
    pub mode: u16,        // i_mode
    pub uid: u16,         // i_uid (low 16 bits)
    pub size_lo: u32,     // i_size
    pub atime: u32,       // i_atime
    pub ctime: u32,       // i_ctime
    pub mtime: u32,       // i_mtime
    pub dtime: u32,       // i_dtime
    pub gid: u16,         // i_gid (low 16 bits)
    pub links_count: u16, // i_links_count
    pub blocks: u32,      // i_blocks (512-byte sectors)
    pub flags: u32,       // i_flags
    pub osd1: u32,        // osd1.linux1.l_i_reserved1
    pub block: [u32; 15], // i_block
    pub generation: u32,  // i_generation
    pub file_acl: u32,    // i_file_acl
    pub size_high: u32,   // i_dir_acl (size high)
    pub faddr: u32,       // i_faddr
    pub frag: u8,         // osd2.linux2.l_i_frag
    pub fsize: u8,        // osd2.linux2.l_i_fsize
    pub pad1: u16,        // osd2.linux2.i_pad1
    pub uid_high: u16,    // osd2.linux2.l_i_uid_high
    pub gid_high: u16,    // osd2.linux2.l_i_gid_high
    pub reserved2: u32,   // osd2.linux2.l_i_reserved2
}
```

[GUARANTEE]

```rust
impl InodeInner {
    /// Creates a new split-lock inode inner state.
    ///
    /// # Arguments
    /// * `meta` - Meta descriptor (clean or dirty).
    /// * `mapping` - Mapping descriptor (clean or dirty).
    /// * `backend` - Weak backend pointer for PageCache to call into.
    pub fn new(meta: Dirty<InodeMetaDesc>, mapping: Dirty<InodeMappingDesc>, backend: Weak<dyn PageCacheBackend>) -> Self;

    /// Acquires the meta read lock.
    pub(super) fn meta_read(&self) -> RwMutexReadGuard<'_, InodeMeta>;
    /// Acquires the meta write lock.
    pub(super) fn meta_write(&self) -> RwMutexWriteGuard<'_, InodeMeta>;
    /// Acquires the mapping read lock.
    pub(super) fn mapping_read(&self) -> RwMutexReadGuard<'_, InodeMapping>;
    /// Acquires the mapping write lock.
    pub(super) fn mapping_write(&self) -> RwMutexWriteGuard<'_, InodeMapping>;
    /// Returns the per-inode PageCache.
    pub(super) fn page_cache(&self) -> &PageCache;
}
```

```rust
impl InodeMeta {
    /// Returns the current file size in bytes.
    pub(super) fn file_size(&self) -> usize;
    /// Sets file size in bytes.
    pub(super) fn set_file_size(&mut self, new_size: usize);

    /// Returns the inode mode (permission bits only; file type is stored in Inode.type_).
    pub(super) fn mode(&self) -> InodeMode;
    /// Sets inode mode (permission bits).
    pub(super) fn set_mode(&mut self, mode: InodeMode);

    pub(super) fn uid(&self) -> u32;
    pub(super) fn set_uid(&mut self, uid: u32);
    pub(super) fn gid(&self) -> u32;
    pub(super) fn set_gid(&mut self, gid: u32);

    pub(super) fn links_count(&self) -> u16;
    pub(super) fn set_links_count(&mut self, nlinks: u16);

    pub(super) fn atime(&self) -> Duration;
    pub(super) fn set_atime(&mut self, t: Duration);
    pub(super) fn mtime(&self) -> Duration;
    pub(super) fn set_mtime(&mut self, t: Duration);
    pub(super) fn ctime(&self) -> Duration;
    pub(super) fn set_ctime(&mut self, t: Duration);

    /// Returns whether meta is dirty.
    pub(super) fn is_dirty(&self) -> bool;
    /// Clears the meta dirty flag.
    pub(super) fn clear_dirty(&mut self);
}
```

```rust
impl InodeMapping {
    /// Returns the number of allocated 512-byte sectors.
    pub(super) fn blocks_512(&self) -> u32;
    /// Sets the number of allocated 512-byte sectors.
    pub(super) fn set_blocks_512(&mut self, blocks: u32);

    /// Read-only mapping: translate logical block to physical block.
    pub(super) fn get_block(&self, fs: &Ext2, iblock: u32) -> Result<Option<Bid>>;
    /// Mapping with optional allocation.
    pub(super) fn get_or_alloc_block(&mut self, fs: &Ext2, iblock: u32, create: bool) -> Result<Option<Bid>>;
    /// Truncates blocks beyond `new_size`.
    pub(super) fn truncate_blocks(&mut self, fs: &Ext2, new_size: usize) -> Result<()>;

    /// Returns whether mapping is dirty.
    pub(super) fn is_dirty(&self) -> bool;
    /// Clears the mapping dirty flag.
    pub(super) fn clear_dirty(&mut self);
}
```

```rust
impl InodeMetaDesc {
    /// Parses meta fields from an on-disk inode and returns the inode type.
    ///
    /// # Returns
    /// * `Ok((type_, meta))` - Parsed type and meta descriptor.
    /// * `Err(ESTALE)` - Inode is deleted.
    /// * `Err(EIO)` - Invalid flags or other irrecoverable on-disk format errors.
    /// * `Err(EUCLEAN)` - Corrupted values (e.g. size overflow).
    pub(super) fn try_from_raw(raw: &RawInode) -> Result<(InodeType, InodeMetaDesc)>;
}

impl InodeMappingDesc {
    /// Parses mapping fields (i_blocks + i_block[]) from an on-disk inode.
    pub(super) fn from_raw(raw: &RawInode) -> InodeMappingDesc;
}

impl RawInode {
    /// Assembles an on-disk inode from split meta/mapping descriptors.
    pub(super) fn from_parts(type_: InodeType, meta: &InodeMetaDesc, mapping: &InodeMappingDesc) -> RawInode;
}
```

```rust
impl InodeInner {
    /// Persists inode meta+mapping to inode table page cache.
    ///
    /// # Lock
    /// The caller must hold both `meta.write()` and `mapping.write()` guards and
    /// pass them in. This avoids nested lock acquisition and keeps the lock
    /// protocol explicit.
    pub(super) fn persist_inode_locked(
        meta: &mut InodeMeta,
        mapping: &mut InodeMapping,
        ino: u32,
        type_: InodeType,
        fs: &Ext2,
    ) -> Result<()>;
}
```

```rust
impl Inode {
    /// Returns a reference to this inode's PageCache.
    pub(super) fn page_cache(&self) -> &PageCache;

    /// Returns the VMO for VFS page-cache export.
    pub(super) fn page_cache_vmo(&self) -> Arc<Vmo>;

    /// Reads file data through PageCache.
    ///
    /// # Lock
    /// Takes `meta.read()` for size/times and calls into VMO read.
    /// PageCache may call backend mapping under `mapping.read()`.
    pub(super) fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize>;

    /// Writes file data through PageCache.
    ///
    /// # Lock
    /// Takes `meta.write()` for size/times updates.
    /// Uses `mapping.write()` only for short allocation/truncate steps and MUST
    /// NOT hold `mapping.write()` across VMO/PageCache operations.
    pub(super) fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize>;

    /// Resizes this inode to `new_size` bytes.
    ///
    /// # Lock
    /// - Grow: `meta.write()` + PageCache resize (no mapping allocation needed).
    /// - Shrink: ensure no PageCache writeback occurs while holding `mapping.write()`.
    pub(super) fn resize(&self, new_size: usize) -> Result<()>;

    /// Syncs all dirty data and metadata to disk.
    pub(super) fn sync_all(&self) -> Result<()>;

    /// Syncs file data pages to disk; persists metadata if required.
    pub(super) fn sync_data(&self) -> Result<()>;

    /// Adds a directory entry to a directory inode.
    ///
    /// # Lock
    /// MUST serialize directory mutations by holding `meta.write()` for the
    /// duration. Concurrent `lookup`/`readdir` may be blocked.
    pub(super) fn add_entry(
        &self,
        name: &str,
        ino: u32,
        file_type: DirEntryFileType,
    ) -> Result<()>;

    /// Deletes a directory entry by name.
    ///
    /// # Lock
    /// MUST serialize directory mutations by holding `meta.write()` for the
    /// duration. Concurrent `lookup`/`readdir` may be blocked.
    pub(super) fn delete_entry(&self, name: &str) -> Result<()>;
}

impl PageCacheBackend for Inode {
    /// Called by PageCache on cache miss.
    ///
    /// # Lock
    /// Takes `mapping.read()` ONLY. Must not take `meta`.
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;

    /// Called by PageCache writeback.
    ///
    /// # Lock
    /// Takes `mapping.read()` ONLY. Must not take `meta`.
    fn write_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;

    /// Returns number of pages addressable by this inode's data PageCache.
    ///
    /// # Lock
    /// Must not take `meta`. Prefer using PageCache VMO size.
    fn npages(&self) -> usize;
}
```

[SPECIFICATION]

## Invariants

- Split descriptors:
  - `InodeMeta.desc` contains all on-disk fields except `i_blocks` and `i_block[]`.
  - `InodeMapping.desc` contains exactly `i_blocks` and `i_block[]`.
  - There MUST be no live second copy of `i_block[]` elsewhere.

- PageCache sizing:
  - `inner.page_cache` capacity tracks file size, aligned to `BLOCK_SIZE`.
  - Resizing the file updates PageCache size consistently.

## Locking protocol

- Inode lock order: `meta` then `mapping`.

- Allocation policy (explicit, not in PageCache):
  - Block allocation for buffered writes MUST be performed by ext2 inode methods
    (e.g., `Inode::write_at`) under `mapping.write()` BEFORE calling into
    PageCache/VMO operations.
  - `PageCacheBackend::write_page_async` MUST NOT allocate blocks (writeback must
    only operate on already-mapped pages).
  - Reason: preserve Linux ext2 behavior (allocation failures must surface on
    the write path) and avoid complex lock coupling inside PageCache callbacks.

- Directory mutation serialization (accepted):
  - Directory entry mutations (`Inode::add_entry`, `Inode::delete_entry`, and
    rename-related helpers) MUST take the directory inode `meta.write()` lock for
    the duration of the operation.
  - Concurrent directory reads (`lookup`/`readdir`) may be blocked while the
    mutation is in progress.
  - This refactor explicitly prefers a simpler lock protocol over maximizing
    directory read concurrency.
- Backend callback constraint:
  - `PageCacheBackend::{read_page_async, write_page_async, npages}` MUST NOT
    acquire `meta`.
  - Backend callbacks may acquire `mapping.read()`.

- PageCache interaction constraint:
  - No code path may hold `mapping.write()` while calling into PageCache/VMO
    operations that can trigger pager callbacks (e.g., Vmo reads/writes, eviction,
    decommit). This prevents deadlocks:
    `PageCacheManager.pages_mutex` -> backend -> `mapping.read()`
    vs
    `mapping.write()` -> PageCacheManager.pages_mutex.

## Conversions and persistence

- `InodeMetaDesc::try_from_raw` must preserve existing error behavior:
  - Deleted inode: `Err(ESTALE)` (same condition as current `InodeDesc::try_from`).
  - Invalid flags: `Err(EIO)`.
  - Size overflow/corruption: `Err(EUCLEAN)`.

- `persist_inode_locked` assembles a `RawInode` from `(type_, meta, mapping)` and
  calls `Ext2::write_inode_desc(ino, &raw)`.
  On success it clears both dirty flags.

## Encapsulation / call flow

- `Inode` is the public handle and implements `PageCacheBackend`.
- `InodeInner` owns locks and PageCache.
- `InodeMeta` owns metadata-only methods; `InodeMapping` owns mapping-only methods.
- High-level inode operations (read/write/resize/sync/direntry) should:
  - take meta/mapping locks explicitly via `InodeInner::{meta_*, mapping_*}`
  - call `InodeMeta`/`InodeMapping` methods for domain logic
  - call `InodeInner::persist_inode_locked` for persistence

Concrete call patterns (illustrative):

- `Inode::read_at`:
  - Acquire `let meta = self.inner.meta_read()` to read `size`.
  - Call `self.inner.page_cache().pages().read(...)`.
  - PageCache commit path may call backend, which acquires `mapping.read()`.

- `Inode::write_at`:
  - Acquire `let mut meta = self.inner.meta_write()` for size/time updates.
  - Acquire `let mut mapping = self.inner.mapping_write()` and allocate ALL
    logical blocks touched by the write range via
    `mapping.get_or_alloc_block(..., create=true)`.
    - Allocation failures (e.g., ENOSPC) MUST be returned from `write_at`.
    - Drop `mapping` BEFORE touching PageCache/VMO.
  - Extend PageCache (grow) if needed, then write user data via VMO.
  - Update `mtime/ctime` in meta.
  - Acquire `mapping.write()` again and call `persist_inode_locked(...)`.

- `Inode::add_entry` / `Inode::delete_entry`:
  - Serialize directory mutations by taking `meta.write()` for the directory inode.
  - If a directory growth block is required:
    - Take `mapping.write()` only for the allocation step, then drop.
  - Perform directory block read/modify/write via PageCache/VMO.
  - Persist via `persist_inode_locked(meta, mapping, ...)`.

[DIFF]

Linux: ext2 relies on separate lock domains (i_rwsem + truncate/mapping locks)
  to allow page cache paths to call get_block without deadlocking.
  → Asterinas: split `InodeInner` into `meta` and `mapping` locks.
  Reason: PageCacheBackend callbacks in Asterinas can re-enter ext2 mapping.

Asterinas (current): uses a single `RwMutex<InodeInner>` and an upread/upgrade
  workaround to avoid self-deadlock.
  → Asterinas (new): remove the need for that workaround by lock splitting.

[TEST]

## Conversion / persistence
- Parse valid RawInode -> (type_, meta, mapping) and re-assemble RawInode; fields match.
- Deleted inode encoding -> InodeMetaDesc::try_from_raw returns ESTALE.
- Invalid flags -> returns EIO.
- File size overflow -> returns EUCLEAN.

## Deadlock regression
- Hold `meta.write()` in one thread while triggering PageCache commit/writeback in another
  (or via VMO read/write that commits pages); must not deadlock.

## Mapping correctness
- Read sparse hole via PageCache -> returns zero page (no BIO).
- Writeback of a dirty page with missing mapping -> returns EIO.
