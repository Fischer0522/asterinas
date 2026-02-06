# Phase 1 - Ext2 Core Struct Design (`fs.rs`)

Target implementation file: `kernel/src/fs/ext2/fs.rs`  
Scope in this phase: filesystem root + superblock runtime model.

## Structure Definition

```rust
/// Ext2 filesystem runtime root.
///
/// # Linux Reference
/// - Source: `fs/ext2/ext2.h:72-119`
/// - Corresponds to: `struct ext2_sb_info`
/// - Mount/validation flow: `fs/ext2/super.c:877-980` (`ext2_fill_super`)
///
/// # Concurrency
/// - Locking hot path: `super_block` (read) for statfs/lookup
/// - Locking write path: `super_block` (write) for mount-state/counter updates
/// - Global order (cycle-free):
///   `SuperBlock -> BlockGroup -> inode_cache -> inode(ascending ino) -> bitmap`
///   // Linux: `fs/ext2/ext2.h:108-115` (`s_lock` semantics)
///
/// # Caching
/// - Cached fields: free block/inode counters, mount state, feature flags
/// - Authoritative source: on-disk superblock at byte 1024
///   (`fs/ext2/ext2.h:411-481`)
#[derive(Debug)]
pub struct Ext2 {
    /// Backing block device for all metadata/data I/O.
    /// Linux logical peer: `super_block::s_bdev` used by `sb_bread`.
    /// Linux: `fs/ext2/super.c:938-947`
    block_device: Arc<dyn BlockDevice>,

    /// In-memory superblock cache with dirty tracking.
    /// Linux peer: `ext2_sb_info.s_es` + `s_lock`.
    /// Linux: `fs/ext2/ext2.h:83`, `fs/ext2/ext2.h:115`
    super_block: RwMutex<Dirty<SuperBlockMem>>,

    /// Group descriptor runtime state.
    /// Linux peer: `ext2_sb_info.s_group_desc`.
    /// Linux: `fs/ext2/ext2.h:84`, `fs/ext2/ext2.h:102-124`
    block_groups: Vec<BlockGroup>,

    /// Cached inode instances keyed by inode number.
    /// Asterinas addition: replaces global hash/list with per-fs `BTreeMap`.
    /// Linux intent peer: `iget_locked` inode cache behavior.
    /// Linux: `fs/ext2/inode.c:1387-1403`
    inode_cache: RwLock<BTreeMap<u32, Weak<Inode>>>,

    /// Filesystem mount mode (RW/forced-RO).
    /// Linux peer: remount-RO policy in `ext2_error`.
    /// Linux: `fs/ext2/super.c:75-81`
    mount_mode: AtomicMountMode,

    /// Geometry derived from superblock for hot-path arithmetic.
    /// Linux peer: `s_inodes_per_group`, `s_blocks_per_group`, `s_inode_size`.
    /// Linux: `fs/ext2/ext2.h:74-76`, `fs/ext2/ext2.h:93-94`
    inodes_per_group: u32,
    blocks_per_group: u32,
    inode_size: usize,
    block_size: usize,

    /// Weak self for back-pointers (inode -> fs) without reference cycles.
    /// Asterinas addition for memory safety.
    self_ref: Weak<Ext2>,
}

/// In-memory mutable superblock state.
///
/// # Linux Reference
/// - On-disk layout: `struct ext2_super_block` (`fs/ext2/ext2.h:411-481`)
/// - Runtime fields: `struct ext2_sb_info` (`fs/ext2/ext2.h:72-119`)
#[derive(Clone, Copy, Debug)]
pub struct SuperBlockMem {
    /// Exact on-disk bytes decoded into typed representation.
    /// Linux: `ext2_sb_info.s_es` -> `struct ext2_super_block`
    /// Linux: `fs/ext2/ext2.h:83`, `fs/ext2/ext2.h:411-481`
    pub on_disk: SuperBlock,

    /// Mount-state cache mirrored to `s_state` during sync/freeze.
    /// Linux: `ext2_sb_info.s_mount_state`
    /// Linux: `fs/ext2/ext2.h:89`, `fs/ext2/super.c:1343-1345`
    pub mount_state: u16,

    /// Effective mount options impacting behavior and state transitions.
    /// Linux: `ext2_sb_info.s_mount_opt`
    /// Linux: `fs/ext2/ext2.h:85`
    pub mount_opt: u32,

    /// Reserved uid/gid for privileged block reservation checks.
    /// Linux: `ext2_sb_info.s_resuid`, `s_resgid`
    /// Linux: `fs/ext2/ext2.h:87-88`
    pub reserved_uid: u32,
    pub reserved_gid: u32,

    /// Last overhead/bookkeeping snapshot for incremental recomputation.
    /// Linux: `s_overhead_last`, `s_blocks_last`
    /// Linux: `fs/ext2/ext2.h:80-81`
    pub overhead_last: u64,
    pub blocks_last: u64,

    /// Aggregate counters cached in memory; persisted on sync.
    /// Linux peers: `s_freeblocks_counter`, `s_freeinodes_counter`, `s_dirs_counter`
    /// Linux: `fs/ext2/ext2.h:99-101`
    pub free_blocks_counter: u64,
    pub free_inodes_counter: u64,
    pub dirs_counter: u64,
}

/// Mount mode state machine.
///
/// - `ReadWrite` -> `ReadOnlyForced` on unrecoverable metadata error.
/// - No transition back to `ReadWrite` without remount/reopen.
/// Linux intent: `ext2_error` + `ERRORS_RO`.
/// Linux: `fs/ext2/super.c:49-82`
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountMode {
    ReadWrite,
    ReadOnlyForced,
}

#[derive(Debug)]
pub struct AtomicMountMode(AtomicU8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SuperDirtyReason {
    /// Counter updates from alloc/free paths.
    /// Linux: `fs/ext2/super.c:1288-1290`
    Counters,
    /// Feature flag transition (e.g., large file first write).
    /// Linux: `fs/ext2/inode.c:1570-1583`
    FeatureFlags,
    /// Superblock state/mount status update.
    /// Linux: `fs/ext2/super.c:1319-1323`, `fs/ext2/super.c:1343-1345`
    MountState,
}
```

## Method Signatures (No Implementation Yet)

```rust
impl Ext2 {
    /// Mount and validate ext2 metadata.
    /// Linux equivalent: `ext2_fill_super()`
    /// Linux: `fs/ext2/super.c:877-980`
    pub fn open(block_device: Arc<dyn BlockDevice>) -> Result<Arc<Self>>;

    /// Read-only access for stat/lookup fast path.
    pub fn super_block(&self) -> RwMutexReadGuard<'_, Dirty<SuperBlockMem>>;

    /// Mark superblock metadata dirty and enqueue writeback.
    /// Linux equivalent: update + `ext2_sync_super()`
    /// Linux: `fs/ext2/super.c:1288-1295`
    pub fn mark_super_dirty(&self, reason: SuperDirtyReason);

    /// Flush superblock to disk.
    /// Linux equivalent: `ext2_sync_fs()` / `ext2_write_super()`
    /// Linux: `fs/ext2/super.c:1308-1326`, `fs/ext2/super.c:1359-1363`
    pub async fn sync_super(&self, wait: bool) -> Result<()>;

    /// Handle filesystem-level corruption/error policy.
    /// Linux equivalent: `ext2_error()`
    /// Linux: `fs/ext2/super.c:49-82`
    pub fn handle_fs_error(&self, function: &'static str, err: Error) -> Result<()>;

    /// Reject writes once filesystem enters forced readonly mode.
    pub fn require_writable(&self) -> Result<()>;
}

impl SuperBlockMem {
    /// Validate invariants loaded from disk.
    /// Linux checks: magic, feature flags, revision compatibility.
    /// Linux: `fs/ext2/super.c:950-980`
    pub fn validate_on_load(&self) -> Result<()>;

    /// Persist mount-state transition before freeze/sync.
    /// Linux equivalent: `ext2_sync_fs()` and `ext2_freeze()` state writes.
    /// Linux: `fs/ext2/super.c:1319-1325`, `fs/ext2/super.c:1343-1346`
    pub fn prepare_sync_state(&mut self);
}
```

## Design Rationale

### Why `Arc<RwLock<...>>` + `Dirty<T>` around superblock?
- Linux keeps `s_es` + `s_lock` and mutates counters/state under lock; this maps directly to `RwMutex<Dirty<SuperBlockMem>>` for read-heavy stat paths and serialized writes.
- `Dirty<T>` makes clean/dirty transitions explicit and writeback-triggerable.

### Why per-filesystem inode cache (`BTreeMap`) in `Ext2`?
- Linux inode cache behavior (`iget_locked`) must be preserved logically, but global hash tables are replaced with per-instance map for isolation and unmount safety.
- `Weak<Inode>` prevents hard-reference cycles while still supporting hard-link shared inode handles.

### Why explicit `MountMode`?
- Linux `ext2_error()` may force read-only remount; modeling this as an explicit state machine avoids implicit flag coupling and makes degradation policy reviewable.

## Dirty/Clean State Transitions

```
Clean -> Dirty(Counters|FeatureFlags|MountState) -> Syncing -> Clean
  |                                      |
  +------ metadata corruption -----------+
                 v
          ReadOnlyForced (terminal until remount)
```

## Validation Checklist (Stage 1)

- [x] Every field has Linux reference or explicit Asterinas-only justification.
- [x] Lock acquisition order is documented and cycle-free.
- [x] Dirty/clean transitions are explicit.
- [x] Corruption handling includes readonly fallback.
- [x] Hard-link shared inode cache behavior is addressed.
- [x] Read-heavy vs write-heavy paths are separated by lock strategy.
- [x] Cached vs authoritative superblock ownership is explicit.
