# Phase 1 - Core Struct Design (`inode.rs`)

Target implementation file: `kernel/src/fs/ext2/inode.rs`

## 1) Structure Definition

```rust
/// Ext2 inode shared handle.
///
/// # Linux Reference
/// - Source: `fs/ext2/ext2.h:632-680`
/// - Corresponds to: `struct ext2_inode_info` + embedded `vfs_inode`
/// - Load path: `fs/ext2/inode.c:1387-1509` (`ext2_iget`)
///
/// # Concurrency
/// - Reader-heavy: metadata queries and read mapping use shared lock
/// - Writer paths: truncate/allocation/writeback serialized
/// - Cross-inode order (cycle-free):
///   1) directory inode(s) by ascending ino
///   2) target inode by ascending ino
///   3) then block-group/bitmap locks
///   Linux intent refs: `fs/ext2/ext2.h:666-674`, `fs/ext2/inode.c:1196-1200`
///
/// # Caching
/// - Cached state: decoded inode metadata + block mapping pointers
/// - Authoritative source: on-disk `struct ext2_inode`
///   (`fs/ext2/ext2.h:290-342`)
#[derive(Debug)]
pub struct Inode {
    /// On-disk inode number, immutable after load.
    /// Linux: `inode->i_ino` used by `ext2_get_inode`.
    /// Linux: `fs/ext2/inode.c:1314-1329`
    ino: u32,

    /// Logical inode type cached from mode bits.
    /// Linux: `inode->i_mode` decode in `ext2_iget`.
    /// Linux: `fs/ext2/inode.c:1413`, `fs/ext2/inode.c:1455-1501`
    type_: InodeType,

    /// Inode's owning block group (allocation locality key).
    /// Linux: `ext2_inode_info.i_block_group`
    /// Linux: `fs/ext2/ext2.h:650`, `fs/ext2/inode.c:1466`
    block_group_idx: usize,

    /// Mutable inode state + dirty tracking.
    /// Linux intent peers: `i_meta_lock`, `truncate_mutex`, `i_state`.
    /// Linux: `fs/ext2/ext2.h:638`, `fs/ext2/ext2.h:666-674`
    inner: RwMutex<InodeInner>,

    /// Page cache for file data and directory blocks.
    /// Asterinas replacement for `buffer_head` chains.
    /// Linux intent peer: read/write via buffer cache in inode/dir paths.
    /// Linux: `fs/ext2/inode.c:783`, `fs/ext2/dir.c:342`
    page_cache: PageCache,

    /// Back-reference to owning filesystem.
    fs: Weak<Ext2>,
}

/// Mutable inode runtime state.
///
/// # Linux Reference
/// - Source: `fs/ext2/ext2.h:632-680`
/// - Corresponds to: mutable subset of `struct ext2_inode_info`
#[derive(Debug)]
pub struct InodeInner {
    /// Semantic inode descriptor mirror with dirty bit.
    /// Linux fields: `i_data`, `i_flags`, `i_file_acl`, `i_dir_acl`, `i_dtime`.
    /// Linux: `fs/ext2/ext2.h:633-641`
    desc: Dirty<InodeDesc>,

    /// Inode dynamic state flags.
    /// Linux: `ext2_inode_info.i_state`, `EXT2_STATE_NEW`.
    /// Linux: `fs/ext2/ext2.h:638`, `fs/ext2/ext2.h:685`
    state: InodeState,

    /// Free/unlinked runtime marker used during eviction.
    /// Linux intent: delete/evict path (`i_nlink==0`, `i_dtime`, `ext2_evict_inode`).
    /// Linux: `fs/ext2/inode.c:72-99`, `fs/ext2/inode.c:1433-1437`
    is_freed: bool,

    /// Last successful directory scan hint for lookup locality.
    /// Linux: `ext2_inode_info.i_dir_start_lookup`.
    /// Linux: `fs/ext2/ext2.h:655`, `fs/ext2/inode.c:1467`
    dir_start_lookup: u32,

    /// Allocation locality hint (`logical -> physical` pair).
    /// Linux: `ext2_block_alloc_info.last_alloc_*`.
    /// Linux: `fs/ext2/ext2.h:44-62`
    alloc_hint: Option<BlockAllocHint>,

    /// Weak self ref for constructing Arc-aware callbacks safely.
    weak_self: Weak<Inode>,

    /// Owning filesystem back-reference.
    fs: Weak<Ext2>,
}

/// Inode metadata state machine.
/// Linux: `EXT2_STATE_NEW`, delete/writeback transitions.
/// Linux: `fs/ext2/ext2.h:685`, `fs/ext2/inode.c:1529-1531`, `fs/ext2/inode.c:1611`
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InodeState {
    /// Loaded from disk and clean.
    Clean,
    /// Metadata dirty and pending writeback.
    Dirty,
    /// Newly allocated inode requiring zero-init semantics on first write.
    New,
    /// Unlinked/deleting path.
    Deleting,
}

/// Allocation locality hint for linear append workloads.
/// Linux counterpart: `struct ext2_block_alloc_info`.
/// Linux: `fs/ext2/ext2.h:44-62`
#[derive(Clone, Copy, Debug)]
pub struct BlockAllocHint {
    /// Last logical file block allocated.
    /// Linux: `last_alloc_logical_block` (`fs/ext2/ext2.h:53`)
    pub last_logical_block: u32,
    /// Last physical disk block allocated.
    /// Linux: `last_alloc_physical_block` (`fs/ext2/ext2.h:61`)
    pub last_physical_block: u64,
}

/// Semantic inode descriptor (decoded view of on-disk inode).
///
/// # Linux Reference
/// - On-disk source: `struct ext2_inode` (`fs/ext2/ext2.h:290-342`)
/// - In-memory peer: `struct ext2_inode_info` (`fs/ext2/ext2.h:632-680`)
#[derive(Clone, Copy, Debug)]
pub struct InodeDesc {
    /// Mode-derived type.
    /// Linux: `i_mode`
    /// Linux: `fs/ext2/ext2.h:291`
    type_: InodeType,

    /// Permission bits.
    /// Linux: `i_mode`
    /// Linux: `fs/ext2/ext2.h:291`
    perm: FilePerm,

    /// Owner uid/gid (combined low/high 16 bits).
    /// Linux: `i_uid_low/high`, `i_gid_low/high`
    /// Linux: `fs/ext2/ext2.h:292`, `fs/ext2/ext2.h:323-324`
    uid: u32,
    gid: u32,

    /// File size (with high 32 bits for regular file).
    /// Linux: `i_size`, `i_size_high`
    /// Linux: `fs/ext2/ext2.h:293`, `fs/ext2/ext2.h:344`
    size: u64,

    /// Timestamps and deletion time.
    /// Linux: `i_atime`, `i_ctime`, `i_mtime`, `i_dtime`
    /// Linux: `fs/ext2/ext2.h:294-297`
    atime: UnixTime,
    ctime: UnixTime,
    mtime: UnixTime,
    dtime: UnixTime,

    /// Hard-link count.
    /// Linux: `i_links_count`
    /// Linux: `fs/ext2/ext2.h:299`
    links_count: u16,

    /// 512-byte sector count.
    /// Linux: `i_blocks`
    /// Linux: `fs/ext2/ext2.h:300`
    blocks: u32,

    /// Inode flags.
    /// Linux: `i_flags`
    /// Linux: `fs/ext2/ext2.h:301`, `fs/ext2/ext2.h:223-244`
    flags: FileFlags,

    /// Extended attribute block.
    /// Linux: `i_file_acl`
    /// Linux: `fs/ext2/ext2.h:315`
    file_acl: u32,

    /// Direct + indirect block pointers.
    /// Linux: `i_block[15]`
    /// Linux: `fs/ext2/ext2.h:313`
    block_ptrs: [u32; 15],
}

/// Exact on-disk inode layout.
///
/// # Linux Reference
/// - Source: `fs/ext2/ext2.h:290-342`
/// - Corresponds to: `struct ext2_inode`
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct RawInode {
    /// `i_mode`
    /// Linux: `fs/ext2/ext2.h:291`
    pub mode: u16,
    /// `i_uid` low 16 bits
    /// Linux: `fs/ext2/ext2.h:292`
    pub uid: u16,
    /// `i_size` low 32 bits
    /// Linux: `fs/ext2/ext2.h:293`
    pub size_lo: u32,
    /// `i_block`
    /// Linux: `fs/ext2/ext2.h:313`
    pub block: [u32; 15],
    // ... remaining fields follow Linux order exactly.
}
```

## 2) Method Signatures (No Implementation Yet)

```rust
impl Inode {
    /// Read logical block through mapping path and page cache.
    ///
    /// # Concurrency
    /// - Acquires read lock on `inner`.
    /// - Must not hold write lock across bio wait.
    ///
    /// # Linux Equivalent
    /// - `ext2_get_block()`
    /// Linux: `fs/ext2/inode.c:783`
    pub async fn read_block(&self, iblock: u32) -> Result<Arc<CachePage>>;

    /// Translate logical -> physical block mapping (read path).
    /// Linux: `fs/ext2/inode.c:163` (`ext2_block_to_path`), `fs/ext2/inode.c:783`
    pub fn get_block(&self, iblock: u32) -> Result<Option<Bid>>;

    /// Mark inode metadata dirty and register writeback requirement.
    /// Linux equivalent: `mark_inode_dirty()` usage in ext2 paths.
    /// Linux: `fs/ext2/inode.c:90`, `fs/ext2/namei.c:118`, `fs/ext2/dir.c:469`
    pub fn mark_dirty(&self);

    /// Write inode metadata back to disk.
    /// Linux equivalent: `__ext2_write_inode()` / `ext2_write_inode()`.
    /// Linux: `fs/ext2/inode.c:1512-1619`
    pub async fn write_back(&self, sync: bool) -> Result<()>;

    /// Truncate data blocks and update pointer tree safely.
    ///
    /// # Concurrency
    /// - Serializes with allocation path via truncate mutex.
    ///
    /// # Linux Equivalent
    /// - `ext2_truncate_blocks()` / `__ext2_truncate_blocks()`.
    /// Linux: `fs/ext2/inode.c:1172-1273`
    pub fn truncate_blocks(&self, new_size: u64) -> Result<()>;
}

impl InodeInner {
    /// Find directory entry by name.
    /// Linux equivalent: `ext2_find_entry()`.
    /// Linux: `fs/ext2/dir.c:342`
    pub fn find_entry(&self, name: &str) -> Result<u32>;

    /// Read directory entries at byte offset.
    /// Linux equivalent: `ext2_readdir()`.
    /// Linux: `fs/ext2/dir.c:257`
    pub fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize>;

    /// Validate inode on load (stale/corruption checks).
    /// Linux equivalent: stale and xattr block checks in `ext2_iget()`.
    /// Linux: `fs/ext2/inode.c:1433-1453`, `fs/ext2/inode.c:1459-1462`
    pub fn validate_loaded_inode(&self) -> Result<()>;
}
```

## 3) Concurrency and Deadlock Protocol

### Read-heavy paths (`stat`, `lookup`, `readdir`)
- Use `inner` read lock for metadata snapshots (`size`, `block_ptrs`, flags).
- Release inode lock before awaiting async bio completion.

### Write serialization (`alloc`, `truncate`, `writeback`)
- Serialize pointer-tree mutation with a truncate/allocation mutex equivalent to Linux `truncate_mutex`.
- Keep meta lock short-lived; no blocking I/O while holding write lock.

### Cross-structure lock order (directory + inode)
- Directory operations (`link`, `unlink`, `rename`) acquire inode locks by ascending inode number.
- If equal/parent-child contention, parent inode lock first, then child.
- Then acquire block-group/bitmap locks for allocation/free.
- Linux intent refs: directory state transitions in `fs/ext2/namei.c:204-399` and truncate-vs-get_block serialization in `fs/ext2/inode.c:1196-1200`.

## 4) Dirty Tracking / Writeback Boundaries

- `RawInode` is authoritative on-disk format.
- `InodeDesc` is decoded cached representation for runtime logic.
- `Dirty<InodeDesc>` tracks metadata mutations (`size`, `blocks`, links, pointer updates).
- `mark_dirty()` schedules writeback; `write_back(sync)` commits and clears dirty state.
- `InodeState::New` preserves Linux first-write zero-init semantics (`EXT2_STATE_NEW`).

## 5) Error Handling and Corruption Strategy

- Reject stale/deleted inode images on load (`ESTALE`) per Linux criteria.
- Treat impossible size/xattr block inconsistencies as corruption (`EUCLEAN`/`EFSCORRUPTED` mapped to Asterinas `Errno`).
- On fatal metadata write/read error, propagate to fs-level error handler for readonly fallback.

## 6) Hard Link Semantics

- `Arc<Inode>` allows multiple directory entries to reference the same inode object safely.
- Link count transitions remain metadata (`links_count`) and are persisted by inode writeback.
- Linux intent refs: `ext2_link`/`ext2_unlink` link-count updates in `fs/ext2/namei.c:204-296`.
