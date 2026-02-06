# Phase 1 - Supplementary Struct Design (orphan lifecycle)

Target implementation files:
- `kernel/src/fs/ext2/super_block.rs`
- `kernel/src/fs/ext2/inode.rs`

## 1) Structure Definition

```rust
/// Orphan inode lifecycle manager for unlinked-but-open files.
///
/// # Linux Reference
/// - On-disk head field: `s_last_orphan`
///   (`fs/ext2/ext2.h:473`)
/// - In-memory list anchor field: `i_orphan`
///   (`fs/ext2/ext2.h:676`)
/// - Eviction/delete path: `ext2_evict_inode`
///   (`fs/ext2/inode.c:72-110`)
#[derive(Debug)]
pub struct OrphanState {
    /// On-disk orphan list head inode number.
    /// Linux peer: `s_last_orphan`.
    pub last_orphan: u32,

    /// In-memory orphan inode set (Asterinas adaptation of list_head).
    /// Maintained in ascending inode order for deterministic recovery.
    pub inodes: BTreeSet<u32>,
}

/// Orphan transition state for a single inode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrphanPhase {
    /// Not orphaned.
    None,
    /// Unlinked but still referenced/open.
    PendingDelete,
    /// Final eviction in progress.
    Evicting,
    /// Removed from orphan tracking after successful cleanup.
    Reaped,
}

/// Recovery action when mounting with orphan head present.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrphanRecoveryAction {
    /// Truncate inode data and free inode.
    Reap,
    /// Skip and keep readonly due to corruption/IO error.
    SkipWithReadonly,
}
```

## 2) Method Signatures (No Implementation Yet)

```rust
impl SuperBlockMem {
    /// Return current on-disk orphan list head.
    /// Linux field: `s_last_orphan`.
    /// Linux: `fs/ext2/ext2.h:473`
    pub fn orphan_head(&self) -> u32;

    /// Update orphan head and mark superblock dirty.
    pub fn set_orphan_head(&mut self, ino: u32);
}

impl OrphanState {
    /// Add inode to orphan tracking.
    pub fn add_orphan(&mut self, ino: u32);

    /// Remove inode from orphan tracking.
    pub fn remove_orphan(&mut self, ino: u32);

    /// Determine next recovery action for inode.
    pub fn recovery_action(&self, ino: u32) -> OrphanRecoveryAction;
}

impl Inode {
    /// Enter orphan state when link count drops to zero but inode still active.
    pub fn enter_orphan(&self) -> Result<()>;

    /// Finalize orphan deletion during eviction.
    /// Linux equivalent intent: `ext2_evict_inode` sequence.
    /// Linux: `fs/ext2/inode.c:72-110`
    pub fn evict_orphan(&self) -> Result<()>;

    /// Recover orphaned inode during mount recovery.
    pub fn recover_orphan(&self) -> Result<()>;
}

impl Ext2 {
    /// Walk and recover orphan list at mount time.
    pub fn recover_orphans(&self) -> Result<()>;
}
```

## 3) Concurrency & Ordering

- Orphan set update requires filesystem-level orphan lock (or superblock write lock if reused).
- When orphaning due to unlink: acquire inode lock first, then orphan-state lock.
- During recovery: process in ascending inode order; each inode recovery serializes truncate/writeback with inode-local truncate mutex.

## 4) Corruption / Degradation

- Invalid orphan head inode number or cycle detection triggers corruption handling.
- If recovery I/O fails, transition filesystem to readonly-forced mode to prevent further damage.

## 5) Design Rationale

### Why `BTreeSet` for orphan list?
- Replaces Linux intrusive list while preserving ordered deterministic processing and memory safety.

### Why explicit orphan phase enum?
- Makes lifecycle transitions (`pending -> evicting -> reaped`) auditable and easier to verify against eviction logic.
