# Phase 1 - Core Struct Design (`super_block.rs`)

Target implementation file: `kernel/src/fs/ext2/super_block.rs`

## 1) Structure Definition

```rust
/// Decoded Ext2 superblock used by runtime logic.
///
/// # Linux Reference
/// - Source: `fs/ext2/ext2.h:411-481`
/// - Corresponds to: `struct ext2_super_block`
/// - Validation path: `fs/ext2/super.c:877-980` (`ext2_fill_super`)
///
/// # Concurrency
/// - Lock: protected by filesystem-level `Ext2.super_block: RwMutex<Dirty<SuperBlockMem>>`
/// - Ordering: `SuperBlock -> BlockGroup -> inode_cache -> inode(asc ino) -> bitmap`
///
/// # Caching
/// - Cached fields: free counters, mount state, feature flags
/// - Authoritative source: on-disk superblock at offset 1024
#[derive(Clone, Copy, Debug)]
pub struct SuperBlockMem {
    /// Raw semantic superblock decoded from disk bytes.
    /// Linux: `ext2_sb_info.s_es` -> `struct ext2_super_block`
    /// Linux: `fs/ext2/ext2.h:83`, `fs/ext2/ext2.h:411-481`
    pub on_disk: SuperBlock,

    /// Cached mount state mirrored to `s_state` on sync/freeze.
    /// Linux: `ext2_sb_info.s_mount_state`
    /// Linux: `fs/ext2/ext2.h:89`, `fs/ext2/super.c:1343-1345`
    pub mount_state: u16,

    /// Effective mount options affecting state transitions and write policy.
    /// Linux: `ext2_sb_info.s_mount_opt`
    /// Linux: `fs/ext2/ext2.h:85`
    pub mount_opt: u32,

    /// Reserved UID/GID policy cache.
    /// Linux: `ext2_sb_info.s_resuid`, `ext2_sb_info.s_resgid`
    /// Linux: `fs/ext2/ext2.h:87-88`
    pub reserved_uid: u32,
    pub reserved_gid: u32,

    /// Aggregate counters used by statfs/allocation checks.
    /// Linux: `s_freeblocks_counter`, `s_freeinodes_counter`, `s_dirs_counter`
    /// Linux: `fs/ext2/ext2.h:99-101`
    pub free_blocks_counter: u64,
    pub free_inodes_counter: u64,
    pub dirs_counter: u64,

    /// Last overhead cache used for recomputation consistency.
    /// Linux: `s_overhead_last`, `s_blocks_last`
    /// Linux: `fs/ext2/ext2.h:80-81`
    pub overhead_last: u64,
    pub blocks_last: u64,
}

/// Exact on-disk layout mapping.
///
/// # Linux Reference
/// - Source: `fs/ext2/ext2.h:411-481`
/// - Corresponds to: `struct ext2_super_block`
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct RawSuperBlock {
    /// `s_inodes_count`
    /// Linux: `fs/ext2/ext2.h:412`
    pub inodes_count: u32,
    /// `s_blocks_count`
    /// Linux: `fs/ext2/ext2.h:413`
    pub blocks_count: u32,
    /// `s_r_blocks_count`
    /// Linux: `fs/ext2/ext2.h:414`
    pub reserved_blocks_count: u32,
    /// `s_free_blocks_count`
    /// Linux: `fs/ext2/ext2.h:415`
    pub free_blocks_count: u32,
    /// `s_free_inodes_count`
    /// Linux: `fs/ext2/ext2.h:416`
    pub free_inodes_count: u32,
    // ... all remaining fields must keep exact Linux order/size
}

/// Filesystem health state tied to `s_state` and fatal-error policy.
/// Linux: `fs/ext2/ext2.h:358-360`, `fs/ext2/super.c:49-82`
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsHealth {
    Clean,
    Dirty,
    ErrorDetected,
}
```

## 2) Method Signatures (No Implementation Yet)

```rust
impl SuperBlock {
    /// Decode from `RawSuperBlock` and validate mount-time invariants.
    /// Linux equivalent: `ext2_fill_super()` checks.
    /// Linux: `fs/ext2/super.c:950-980`
    pub fn from_raw(raw: RawSuperBlock, readonly_mount: bool) -> Result<Self>;

    /// Validate feature flags for mount mode.
    /// Linux equivalent: incompat/ro-compat checks.
    /// Linux: `fs/ext2/super.c:971-980`
    pub fn validate_features(&self, readonly_mount: bool) -> Result<()>;

    /// Prepare superblock state update before sync.
    /// Linux equivalent: clear `EXT2_VALID_FS` on writable sync.
    /// Linux: `fs/ext2/super.c:1319-1325`
    pub fn prepare_sync_state(&mut self);

    /// Apply error policy updates (`EXT2_ERROR_FS`, optional readonly fallback).
    /// Linux equivalent: `ext2_error()`.
    /// Linux: `fs/ext2/super.c:49-82`
    pub fn apply_error_policy(&mut self) -> FsHealth;

    /// Encode semantic superblock back to raw disk format.
    pub fn to_raw(&self) -> RawSuperBlock;
}

impl SuperBlockMem {
    /// Mark superblock dirty and classify reason for writeback scheduling.
    pub fn mark_dirty(&mut self, reason: SuperDirtyReason);

    /// Validate loaded metadata against ext2 corruption boundaries.
    pub fn validate_on_load(&self) -> Result<()>;
}
```

## 3) Corruption and Readonly Degradation Strategy

- Reject mount when magic or unsupported incompat feature is invalid (`EINVAL`/`ENOTSUP`) before serving requests.
- On runtime metadata corruption, set error state and transition filesystem into readonly-forced mode.
- Keep on-disk compatibility: do not drop unknown fields from raw layout; only gate behavior by supported feature policy.
