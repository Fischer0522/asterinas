# Phase 1 - Utility Struct Design (`utils.rs`)

Target implementation file: `kernel/src/fs/ext2/utils.rs`

## 1) Structure Definition

```rust
/// Trait for testing whether a value is an exact positive power of `x`.
///
/// # Linux Logic Reference
/// - Source: `fs/ext2/balloc.c:1489-1504`
/// - Corresponds to: `test_root(a, b)` + `ext2_group_sparse(group)` helpers
///
/// # Usage in Ext2
/// - Determines sparse-super backup group membership
///   (`group == 0|1|3^n|5^n|7^n` semantics).
pub trait IsPowerOf: Copy + Sized + MulAssign + PartialOrd {
    fn is_power_of(&self, x: Self) -> bool;
}

/// Dirty-tracked wrapper for mutable cached metadata.
///
/// # Linux Logic Reference
/// - Inode metadata dirtying: `mark_inode_dirty()` call sites
///   (`fs/ext2/inode.c`, `fs/ext2/dir.c`, `fs/ext2/namei.c`)
/// - Superblock sync path: `ext2_sync_super`/`ext2_sync_fs`
///   (`fs/ext2/super.c:1288-1326`)
///
/// # Concurrency
/// - Must be wrapped by outer lock (`RwMutex`/`Mutex`) in shared state structs.
/// - `DerefMut` transition marks state dirty atomically with mutation intent.
///
/// # Caching
/// - `value` is cached in-memory copy.
/// - `dirty=true` means writeback required before dropping persistent consistency guarantees.
pub struct Dirty<T: Debug> {
    /// Cached semantic value.
    value: T,
    /// Dirty flag indicating pending persistence.
    dirty: bool,
}

/// Dirty transition reason (supplementary state for auditability).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirtyReason {
    /// Allocation/free counter updates.
    Allocation,
    /// Metadata field edits (mode/size/timestamps/links/etc.).
    Metadata,
    /// Mount state / feature flag update.
    MountState,
}
```

## 2) Method Signatures (No Implementation Yet)

```rust
impl<T: Debug> Dirty<T> {
    /// Create clean cached value.
    pub fn new(value: T) -> Self;

    /// Create dirty cached value (writeback required).
    pub fn new_dirty(value: T) -> Self;

    /// Return whether value has unflushed modifications.
    pub fn is_dirty(&self) -> bool;

    /// Clear dirty marker after successful writeback.
    pub fn clear_dirty(&mut self);

    /// Explicitly mark dirty with reason.
    pub fn mark_dirty(&mut self, reason: DirtyReason);

    /// Extract inner value if clean, otherwise return error.
    /// (Safety helper to avoid silent dirty-drop of persistent metadata.)
    pub fn into_clean(self) -> Result<T>;
}

impl IsPowerOf for u32 {
    /// Return true if `self == x^k` for some `k > 0`.
    /// Linux intent peer: `test_root` loop.
    /// Linux: `fs/ext2/balloc.c:1489-1496`
    fn is_power_of(&self, x: Self) -> bool;
}

impl SuperBlock {
    /// Whether block group has sparse-super backup.
    /// Linux equivalent: `ext2_group_sparse` + `ext2_bg_has_super`.
    /// Linux: `fs/ext2/balloc.c:1498-1520`
    pub fn bg_has_super(&self, group: u32) -> bool;

    /// Number of group-descriptor blocks present in group.
    /// Linux equivalent: `ext2_bg_num_gdb`.
    /// Linux: `fs/ext2/balloc.c:1531-1534`
    pub fn bg_num_gdb(&self, group: u32) -> u64;
}
```

## 3) State-Safety Notes

- `Dirty<T>` centralizes clean/dirty transitions to avoid ad-hoc bool flags scattered across structures.
- Dirty wrapper is a **tracking primitive**, not a synchronization primitive; outer locks remain mandatory.
- Dropping dirty persistent metadata should be treated as bug-level signal (current warning behavior kept; future harden path may escalate in debug builds).

## 4) Design Rationale

### Why keep `Dirty<T>` generic instead of ext2-specific wrappers?
- Shared logic for superblock/group/inode metadata avoids duplicated state machines and keeps writeback policy consistent.

### Why include `IsPowerOf` helper?
- Sparse-super group membership is part of ext2’s core on-disk placement logic; explicit helper makes the 3^n/5^n/7^n rule auditable and testable.

### Why add explicit `DirtyReason` in design?
- Enables future observability (tracing/metrics) for writeback cause analysis without changing core mutation APIs.
