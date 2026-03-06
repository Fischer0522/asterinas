[PROMPT]
Provide modifications to `kernel/src/fs/ext2/block_group.rs`,
`kernel/src/fs/ext2/fs.rs`, and `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_iget              → fs/ext2/inode.c:1387
ext2_evict_inode       → fs/ext2/inode.c:72
ext2_free_inode        → fs/ext2/ialloc.c:79
iput / iput_final      → fs/inode.c:1910
ext2_old lookup_inode  → kernel/src/fs/ext2_old/block_group.rs:104
ext2_old insert_cache  → kernel/src/fs/ext2_old/block_group.rs:153
ext2_old sync_all_inodes → kernel/src/fs/ext2_old/block_group.rs:266

[RELY]
```rust
use super::prelude::*;
use super::fs::Ext2;
use super::utils::Dirty;
use super::inode::{Inode, InodeDesc, InodeType, RawInode, FilePerm};
```

```rust
pub struct BlockGroup {
    idx: usize,
    desc: RwMutex<Dirty<GroupDesc>>,
    block_bitmap: RwMutex<Dirty<IdBitmap>>,
    inode_bitmap: RwMutex<Dirty<IdBitmap>>,
    block_device: Arc<dyn BlockDevice>,
    first_block: u32,
    last_block: u32,
    itb_per_group: u32,
    inodes_per_group: u32,
    inode_size: usize,
    inode_table_backend: Arc<InodeTableBackend>,
    inode_table_cache: PageCache,
}
```

```rust
pub struct Inode {
    ino: u32,
    type_: InodeType,
    inner: RwMutex<InodeInner>,
    block_group_idx: usize,
    fs: Weak<Ext2>,
    extension: Extension,
}
```

```rust
pub struct InodeInner {
    desc: Dirty<InodeDesc>,
    is_freed: bool,
    weak_self: Weak<Inode>,
    fs: Weak<Ext2>,
    page_cache: PageCache,
}
```

```rust
impl InodeInner {
    fn truncate_blocks(&mut self, new_size: usize) -> Result<()>;
    pub(super) fn persist_inode_and_sync(&self, fs: &Ext2) -> Result<()>;
}
```

```rust
impl Inode {
    pub(super) fn new(
        ino: u32, type_: InodeType, desc: Dirty<InodeDesc>,
        block_group_idx: usize, fs: Weak<Ext2>,
    ) -> Arc<Self>;
    pub(super) fn sync_all(&self) -> Result<()>;
    pub(super) fn inode_type(&self) -> InodeType;
}
```

```rust
impl Ext2 {
    pub(super) fn read_inode_desc(&self, ino: u32) -> Result<InodeDesc>;
    pub(super) fn write_inode_desc(&self, ino: u32, raw: &RawInode) -> Result<()>;
    pub fn sync_metadata(&self) -> Result<()>;
    pub fn block_device(&self) -> &Arc<dyn BlockDevice>;
    pub fn super_block(&self) -> RwMutexReadGuard<'_, Dirty<SuperBlock>>;
}
```

```rust
impl BlockGroup {
    pub(super) fn read_inode_desc(&self, index_in_group: u32) -> Result<InodeDesc>;
    pub(super) fn alloc_inode(&self) -> Result<Option<u16>>;
    pub(super) fn free_inode(&self, bit: u16) -> Result<bool>;
    pub(super) fn free_inodes_count(&self) -> u16;
    pub(super) fn inc_free_inodes(&self, count: u16);
    pub(super) fn dec_free_inodes(&self, count: u16);
    pub(super) fn inc_used_dirs(&self);
    pub(super) fn dec_used_dirs(&self);
}
```

[GUARANTEE]

## BlockGroup: inode cache field

```rust
pub struct BlockGroup {
    // ... existing fields ...
    /// Per-group inode cache: maps group-local inode index to Arc<Inode>.
    ///
    /// Linux equivalent: VFS global inode hash table (fs/inode.c:63).
    /// Asterinas: per-BlockGroup BTreeMap since VFS does not provide inode caching.
    /// Follows ext2_old pattern (ext2_old/block_group.rs:364).
    inode_cache: RwMutex<BTreeMap<u32, Arc<Inode>>>,
}
```

## BlockGroup: lookup_inode

```rust
impl BlockGroup {
    /// Looks up an inode by group-local index, returning cached or loading from disk.
    ///
    /// Linux: iget_locked (fs/inode.c:1371) searches hash table, allocates on miss.
    /// Asterinas: per-group BTreeMap with double-checked locking.
    /// Follows ext2_old pattern (ext2_old/block_group.rs:104).
    ///
    /// # Arguments
    /// * `inode_idx` - 0-based group-local inode index.
    /// * `ino` - 1-based filesystem-wide inode number.
    /// * `fs` - Weak reference to the owning Ext2 filesystem.
    pub(super) fn lookup_inode(
        &self,
        inode_idx: u32,
        ino: u32,
        fs: Weak<Ext2>,
    ) -> Result<Arc<Inode>>;
}
```

## BlockGroup: insert_cache

```rust
impl BlockGroup {
    /// Inserts an inode into the cache after successful creation.
    ///
    /// Called only after the full create path succeeds (alloc + init + add_entry).
    /// Follows ext2_old pattern (ext2_old/block_group.rs:153).
    ///
    /// # Arguments
    /// * `inode_idx` - 0-based group-local inode index.
    /// * `inode` - The fully initialized inode.
    pub(super) fn insert_cache(&self, inode_idx: u32, inode: Arc<Inode>);
}
```

## BlockGroup: evict_inode

```rust
impl BlockGroup {
    /// Evicts a single inode from cache, performing cleanup if nlink == 0.
    ///
    /// Linux: ext2_evict_inode (fs/ext2/inode.c:72):
    ///   if (!i_nlink && !is_bad_inode) → set dtime, truncate_blocks(0), free_inode.
    ///
    /// # Returns
    /// `EvictResult` indicating what counters the caller must update.
    fn evict_inode(&self, inode: &Arc<Inode>) -> Result<EvictResult>;
}
```

## BlockGroup: sync_all

```rust
impl BlockGroup {
    /// Syncs and evicts unreferenced inodes from this group's cache, then
    /// flushes group-local metadata.
    ///
    /// Linux: iput_final (fs/inode.c:1910) evicts when i_count reaches 0.
    /// Asterinas: deferred to sync time — evicts inodes with Arc::strong_count == 1.
    /// Follows ext2_old pattern (ext2_old/block_group.rs:266).
    ///
    /// # Returns
    /// Aggregated `EvictResult` for superblock counter updates.
    pub(super) fn sync_all(&self, group_descs: &USegment) -> Result<EvictResult>;
}
```

## EvictResult

```rust
/// Counters that the caller (Ext2) must apply to the superblock
/// after a BlockGroup eviction pass.
pub(super) struct EvictResult {
    /// Number of inodes freed (bitmap cleared).
    pub freed_inodes: u32,
    /// Number of freed inodes that were directories.
    pub freed_dirs: u32,
}
```

## Ext2: read_inode (refactored)

```rust
impl Ext2 {
    /// Reads an inode, using the BlockGroup inode cache.
    ///
    /// Thin orchestrator: computes group index, delegates to
    /// BlockGroup::lookup_inode.
    ///
    /// Linux: ext2_iget (fs/ext2/inode.c:1387)
    pub(super) fn read_inode(&self, ino: u32) -> Result<Arc<Inode>>;
}
```

## Ext2: sync_all (updated)

```rust
impl Ext2 {
    /// Syncs all cached inodes and group-local metadata across all block groups.
    ///
    /// Iterates each BlockGroup, calls sync_all, aggregates freed inode counts,
    /// updates superblock, then syncs filesystem-global metadata.
    pub fn sync_all(&self) -> Result<()>;
}
```

[SPECIFICATION]

## BlockGroup field addition

Add `inode_cache: RwMutex<BTreeMap<u32, Arc<Inode>>>` to `BlockGroup`.
Initialize as empty `BTreeMap::new()` in `BlockGroup::load()`.

## BlockGroup::lookup_inode

Looks up or loads an inode by group-local index.

Pre:
- `inode_idx` is a valid 0-based index within this group.
- `ino` is the corresponding 1-based filesystem-wide inode number.
- The inode bitmap bit for `inode_idx` is allocated.

Post (cache hit):
- Acquires `inode_cache` read lock.
- Finds `inode_idx` in the BTreeMap.
- Returns cloned `Arc<Inode>`.

Post (cache miss):
- Drops read lock, acquires write lock.
- Double-checks the map (another thread may have inserted).
- If still missing: calls `self.read_inode_desc(inode_idx)` to load
  `InodeDesc` from the inode table PageCache.
- Wraps in `Dirty::new(desc)`.
- Constructs `Inode::new(ino, desc.type_(), desc, self.idx, fs)`.
- Inserts into BTreeMap.
- Returns cloned `Arc<Inode>`.

Post (failure):
- `Err(EIO|EINVAL)` from `read_inode_desc`.

Lock protocol:
- Read lock on `inode_cache` for fast path.
- Write lock on `inode_cache` for slow path (load + insert).
- No other locks held while loading from inode table PageCache.

## BlockGroup::insert_cache

Inserts a newly created inode into the cache.

Pre:
- `inode_idx` is a valid allocated index.
- `inode` is fully initialized.

Post:
- Acquires `inode_cache` write lock.
- Inserts `(inode_idx, inode)` into BTreeMap.

## BlockGroup::evict_inode

Evicts a single inode, performing cleanup if deleted.

Pre:
- `inode` is being evicted (strong_count == 1 in cache, about to be removed).

Post (nlink > 0, file still exists):
- Calls `inode.sync_all()` to write back dirty pages and metadata.
- Returns `EvictResult { freed_inodes: 0, freed_dirs: 0 }`.

Post (nlink == 0, file deleted — mirrors ext2_evict_inode):
- Linux: fs/ext2/inode.c:86-96:
  ```c
  EXT2_I(inode)->i_dtime = ktime_get_real_seconds();
  mark_inode_dirty(inode);
  __ext2_write_inode(inode, inode_needs_sync(inode));
  inode->i_size = 0;
  if (inode->i_blocks) ext2_truncate_blocks(inode, 0);
  ```
- Acquires `inode.inner` write lock.
- Calls `inner.truncate_blocks(0)` to free all data + indirect blocks.
- Drops write lock.
- Linux: fs/ext2/inode.c:108-109:
  ```c
  ext2_free_inode(inode);
  ```
- Calls `self.free_inode(bit)` to clear inode bitmap.
- Updates group counters: `self.inc_free_inodes(1)`.
- If inode was directory: `self.dec_used_dirs()`.
- Returns `EvictResult { freed_inodes: 1, freed_dirs: is_dir as u32 }`.

Post (failure):
- `Err(EIO)` from sync or truncate. Inode remains in cache for retry.

## BlockGroup::sync_all

Syncs cached inodes and evicts unreferenced ones.

Pre:
- Called from `Ext2::sync_all()`.

Post:
- Phase 1: Identify evictable inodes.
  - Acquires `inode_cache` write lock.
  - Uses `extract_if` to remove entries where `Arc::strong_count == 1`.
  - Collects removed inodes into a local Vec.
  - Releases write lock.

- Phase 2: Evict removed inodes.
  - For each removed inode, calls `self.evict_inode(&inode)`.
  - Aggregates `EvictResult` counters.

- Phase 3: Sync remaining cached inodes.
  - Acquires `inode_cache` read lock.
  - Clones all remaining `Arc<Inode>` values into a local Vec.
  - Releases read lock.
  - For each remaining inode, calls `inode.sync_all()`.

- Returns aggregated `EvictResult`.

- Phase 4: Sync group-local metadata.
  - Flushes `inode_bitmap` and `block_bitmap` if dirty.
  - Writes dirty `GroupDesc` into Ext2's `group_descriptors_segment`.

Lock protocol:
- Write lock on `inode_cache` only during extract_if (Phase 1).
- No lock held during I/O (evict/sync in Phase 2 and 3).
- This matches ext2_old (block_group.rs:266-295).

## Ext2::read_inode (refactored)

Thin orchestrator delegating to BlockGroup.

Pre:
- `ino` is a 1-based inode number.

Post:
- Computes `group_idx = (ino - 1) / inodes_per_group`.
- Computes `inode_idx = (ino - 1) % inodes_per_group`.
- Calls `self.block_groups[group_idx].lookup_inode(inode_idx, ino, self.self_ref.clone())`.
- Returns the result.

## Ext2::sync_all (updated)

Iterates all groups, syncs and evicts.

Pre:
- Called from `FileSystem::sync()`.

Post:
- For each `BlockGroup` in `self.block_groups`:
  - Calls `group.sync_all(group_descs)`.
  - Accumulates `EvictResult`.
- If total `freed_inodes > 0`:
  - Acquires superblock write lock.
  - Increments `free_inodes_count` by `freed_inodes`.
- Calls `self.sync_metadata()` to persist the descriptor-table segment and
  superblock to disk.
- Returns `Ok(())`.

## Inode::unlink (modification)

Current (fs/ext2/inode.rs:3204-3211):
```rust
if child_inner.desc.links_count == 0 {
    child_inner.desc.dtime = now();
    child_inner.persist_inode_and_sync(&fs)?;
    drop(child_inner);
    let _ = fs.free_inode(child_ino);
}
```

New:
```rust
if child_inner.desc.links_count == 0 {
    child_inner.desc.dtime = now();
    child_inner.is_freed = true;
}
child_inner.persist_inode_and_sync(&fs)?;
```

Rationale: Defer resource reclamation to eviction time (BlockGroup::evict_inode).
This fixes the data block leak: current code calls free_inode (bitmap only)
without truncate_blocks. Eviction path does both.

## Inode::rmdir (modification)

Similar change: remove immediate `fs.free_inode()` call, set `is_freed = true`.
Eviction handles truncate + bitmap free.

## Inode::rename (modification for overwrite case)

When rename overwrites an existing inode and its nlink reaches 0:
same pattern — set `is_freed = true`, defer reclamation to eviction.

## Inode::create (cache insertion point)

Current flow:
```
child = fs.create_inode(parent_ino, type, perm)
add_entry(name, child_ino, dir_ft)?  // may fail
```

New flow:
```
child = fs.create_inode(parent_ino, type, perm)
if let Err(err) = self.add_entry(name, child_ino, dir_ft) {
    let _ = fs.free_inode(child_ino);  // rollback bitmap (no cache entry)
    return Err(err);
}
// Success: insert into cache.
fs.insert_inode_cache(child_ino, child.clone());
```

`Ext2::insert_inode_cache` is a thin helper that computes group_idx/inode_idx
and delegates to `BlockGroup::insert_cache`.

Invariant:
- Cache only contains fully committed inodes.
- Rollback path never touches cache.

[DIFF]
Linux: VFS provides global inode cache via iget_locked / iput / evict_inode.
  → Asterinas: VFS does not cache inodes. Ext2 implements per-BlockGroup
  BTreeMap<u32, Arc<Inode>> cache internally.
  Reason: Asterinas VFS delegates inode lifecycle to individual filesystems.

Linux: iput_final (fs/inode.c:1910) triggers eviction immediately when
  i_count reaches 0 (either LRU or direct evict depending on drop_inode).
  → Asterinas: Eviction is deferred to sync time. Unreferenced inodes
  (Arc::strong_count == 1) are evicted during sync_all_inodes.
  Reason: No shrinker/LRU infrastructure in Asterinas yet. Acceptable for
  current usage patterns. TODO: integrate with memory pressure callbacks
  when Asterinas supports periodic writeback/shrinker.

Linux: ext2_evict_inode (fs/ext2/inode.c:72) is called by VFS evict()
  when inode refcount drops to 0.
  → Asterinas: BlockGroup::evict_inode performs equivalent logic
  (truncate_blocks + free_inode) during sync-time eviction pass.
  Reason: Same logical intent, different trigger point.

Linux: ext2_new_inode inserts into VFS inode cache immediately via
  iget_locked / unlock_new_inode.
  → Asterinas: Inode is inserted into BlockGroup cache only after the
  full create path succeeds (alloc + init + add_entry).
  Reason: Simplifies rollback — failed creates never pollute the cache.

[TEST]
## BlockGroup::lookup_inode
- lookup existing inode → cache miss → loads from disk → returns Arc<Inode>
- second lookup same inode → cache hit → returns same Arc (ptr equality)
- lookup unallocated inode → Err(ENOENT) or load failure

## BlockGroup::insert_cache
- insert after create → subsequent lookup returns cached inode

## BlockGroup::evict_inode (nlink > 0)
- evict inode with nlink > 0 → sync_all called, no bitmap change

## BlockGroup::evict_inode (nlink == 0)
- evict inode with nlink == 0 → truncate_blocks(0) + free_inode bitmap
- data blocks returned to free pool
- inode bitmap bit cleared

## BlockGroup::sync_all_inodes
- inodes with strong_count == 1 → evicted from cache
- inodes with strong_count > 1 → remain in cache, sync_all called
- freed inode counts returned correctly

## Ext2::read_inode (refactored)
- read_inode delegates to BlockGroup::lookup_inode
- repeated read_inode for same ino returns cached instance

## Unlink deferred reclamation
- unlink file → nlink becomes 0 → is_freed = true, no immediate free_inode
- sync_all_inodes → evicts inode → truncate_blocks + free_inode
- data blocks and inode bitmap freed after sync

## Create + rollback
- create_inode + add_entry fails → free_inode called, cache untouched
- create_inode + add_entry succeeds → inode in cache
