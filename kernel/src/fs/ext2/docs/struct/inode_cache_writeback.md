# Phase 1 - Supplementary Struct Design (`fs.rs` + `inode.rs`)

Target implementation files:
- `kernel/src/fs/ext2/fs.rs`
- `kernel/src/fs/ext2/inode.rs`

## 1) Structure Definition

```rust
/// Per-filesystem inode cache state.
///
/// # Linux Reference
/// - Cache lookup/insert intent: `iget_locked()`
///   (`fs/inode.c:1448-1510`)
/// - Linux data structure style: global inode hash table
///   (`fs/inode.c:1450`, `fs/inode.c:2555`)
///
/// # Asterinas Adaptation
/// - Replace global hash/list with per-fs ordered cache map.
/// - Keep shared inode identity semantics with `Arc/Weak`.
#[derive(Debug)]
pub struct InodeCacheState {
    /// Cached inode handles by inode number.
    /// Linux intent: same inode number resolves to shared inode object.
    entries: BTreeMap<u32, InodeCacheEntry>,
}

/// Cache entry containing weak inode ref + lightweight metadata.
#[derive(Debug)]
pub struct InodeCacheEntry {
    /// Weak reference to live inode object.
    handle: Weak<Inode>,

    /// Last observed dirty class for scheduling writeback urgency.
    dirty_class: DirtyClass,
}

/// Inode writeback queue state.
///
/// # Linux Reference
/// - Inode dirty state flags: `I_DIRTY_*`
///   (`include/linux/fs.h:638-751`)
/// - Writeback list links: `i_io_list`, `i_wb_list`
///   (`include/linux/fs.h:825-836`)
/// - Dirty transition entry: `__mark_inode_dirty`
///   (`fs/fs-writeback.c:2563-2701`)
#[derive(Debug)]
pub struct InodeWritebackState {
    /// FIFO/BTree priority queue of dirty inodes pending flush.
    queue: VecDeque<WritebackItem>,
}

/// Single writeback task item.
#[derive(Clone, Debug)]
pub struct WritebackItem {
    /// Target inode number.
    pub ino: u32,
    /// Dirty classification controlling sync urgency.
    pub class: DirtyClass,
}

/// Dirty class for inode metadata/data sync policy.
/// Linux intent peer: `I_DIRTY_SYNC`, `I_DIRTY_DATASYNC`, `I_DIRTY_TIME`, `I_DIRTY_PAGES`.
/// Linux: `include/linux/fs.h:732-751`
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum DirtyClass {
    Time,
    Data,
    Metadata,
    MetadataSync,
}
```

## 2) Method Signatures (No Implementation Yet)

```rust
impl InodeCacheState {
    /// Return cached inode if present and still alive.
    /// Linux equivalent intent: fast path in `iget_locked`.
    /// Linux: `fs/inode.c:1456-1468`
    pub fn get(&self, ino: u32) -> Option<Arc<Inode>>;

    /// Insert or refresh cache entry after inode load.
    /// Linux equivalent intent: hash insertion with `I_NEW` setup.
    /// Linux: `fs/inode.c:1478-1484`
    pub fn insert(&mut self, ino: u32, inode: &Arc<Inode>);

    /// Drop dead weak entries opportunistically.
    pub fn reap_dead(&mut self);
}

impl InodeWritebackState {
    /// Enqueue inode for writeback.
    /// Linux equivalent intent: transition done by `__mark_inode_dirty`.
    /// Linux: `fs/fs-writeback.c:2563-2701`
    pub fn enqueue(&mut self, ino: u32, class: DirtyClass);

    /// Dequeue next inode writeback task.
    pub fn pop_next(&mut self) -> Option<WritebackItem>;

    /// Merge duplicate tasks and keep strongest dirty class.
    pub fn dedup(&mut self);
}

impl Ext2 {
    /// Read inode via cache-or-load path.
    /// Linux equivalent intent: shared inode identity via `iget_locked`.
    /// Linux: `fs/ext2/inode.c:1387-1403`, `fs/inode.c:1448-1510`
    pub fn read_inode_cached(&self, ino: u32) -> Result<Arc<Inode>>;

    /// Mark inode dirty and stage writeback item.
    pub fn stage_inode_writeback(&self, ino: u32, class: DirtyClass);

    /// Flush queued inode writeback tasks.
    pub async fn flush_inodes(&self, sync: bool) -> Result<()>;
}
```

## 3) Locking & Ordering

- Cache lookup path: `inode_cache` read lock only.
- Cache miss insertion path: `inode_cache` write lock, no blocking I/O while holding it.
- Writeback queue lock is separate and must be acquired after `inode_cache` lock when both are needed.
- Inode object lock is acquired after queue/cache locks (global order remains acyclic).

## 4) Design Rationale

### Why explicit writeback queue struct?
- Keeps dirty scheduling logic visible and testable instead of hidden inside inode methods.

### Why `DirtyClass` abstraction?
- Preserves Linux dirty-state separation semantics while fitting Asterinas-friendly typed enums.

### Why per-fs cache + queue?
- Avoids global shared mutable state and aligns with filesystem-instance isolation.
