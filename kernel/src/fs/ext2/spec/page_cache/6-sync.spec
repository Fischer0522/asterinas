[PROMPT]
Provide modifications to `kernel/src/fs/ext2/inode.rs`,
`kernel/src/fs/ext2/impl_for_vfs/inode.rs`,
`kernel/src/fs/ext2/fs.rs`, and
`kernel/src/fs/ext2/impl_for_vfs/fs.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_fsync             → fs/ext2/file.c:155
generic_buffers_fsync  → fs/buffer.c:646
generic_buffers_fsync_noflush → fs/buffer.c:602
file_write_and_wait_range     → mm/filemap.c:777
ext2_sync_fs           → fs/ext2/super.c:1308
ext2_write_inode       → fs/ext2/inode.c:1616
__ext2_write_inode     → fs/ext2/inode.c:1512
ExfatInode::sync_all   → kernel/src/fs/exfat/inode.rs:1707
ExfatInode::sync_data  → kernel/src/fs/exfat/inode.rs:1718
ExfatInodeInner::sync_data → kernel/src/fs/exfat/inode.rs:583
ExfatFs::sync          → kernel/src/fs/exfat/fs.rs:411
Ext2Old::sync_all_inodes  → kernel/src/fs/ext2_old/fs.rs:412
Ext2OldBlockGroup::sync_all_inodes → kernel/src/fs/ext2_old/block_group.rs:266
Ext2OldInodeInner::sync_data      → kernel/src/fs/ext2_old/inode.rs:1172

[RELY]
```rust
use super::prelude::*;
use super::fs::Ext2;
use super::utils::Dirty;
```

```rust
/// PageCache infrastructure (from spec 2-inode-data-page-cache).
pub struct PageCache { /* ... */ }
impl PageCache {
    pub fn pages(&self) -> &Arc<Vmo>;
    pub fn resize(&self, new_size: usize) -> Result<()>;
    pub fn discard_range(&self, range: Range<usize>);
    pub fn evict_range(&self, range: Range<usize>) -> Result<()>;
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
    pub(super) fn persist_inode_and_sync(&self, fs: &Ext2) -> Result<()>;
}
```

```rust
/// Current Ext2 struct (fs.rs).
pub struct Ext2 {
    block_device: Arc<dyn BlockDevice>,
    super_block: RwMutex<Dirty<SuperBlock>>,
    block_groups: Vec<BlockGroup>,
    inodes_per_group: u32,
    blocks_per_group: u32,
    inode_size: usize,
    block_size: usize,
    group_descriptors_segment: USegment,
    root_inode: Arc<Inode>,
    fs_event_subscriber_stats: FsEventSubscriberStats,
    self_ref: Weak<Ext2>,
}
```

```rust
impl Ext2 {
    pub(super) fn read_inode(&self, ino: u32) -> Result<Arc<Inode>>;
    pub fn sync_metadata(&self) -> Result<()>;
    pub fn block_device(&self) -> &Arc<dyn BlockDevice>;
}
```

```rust
/// VFS traits.
pub trait Inode: Any + Send + Sync {
    fn sync_all(&self) -> Result<()> { Ok(()) }
    fn sync_data(&self) -> Result<()> { Ok(()) }
    // ...
}

pub trait FileSystem: Any + Sync + Send {
    fn sync(&self) -> Result<()>;
    // ...
}
```

[GUARANTEE]
```rust
impl InodeInner {
    /// Writes back all dirty pages in the page cache to the block device.
    ///
    /// Linux: generic_buffers_fsync_noflush (fs/buffer.c:602) calls
    /// file_write_and_wait_range which flushes dirty pages via writeback.
    /// Asterinas equivalent: evict_range writes back dirty CachePages
    /// and marks them UpToDate.
    ///
    /// # Lock
    /// Caller must hold at least read lock on InodeInner.
    fn sync_data(&self) -> Result<()>;
}
```

```rust
impl Inode {
    /// Syncs file data (dirty pages) to disk. If the inode descriptor
    /// is dirty (e.g., file was extended, changing i_size or block mappings),
    /// also persists inode metadata.
    ///
    /// Linux: fdatasync path in generic_buffers_fsync_noflush (fs/buffer.c:614-617):
    ///   if (datasync && !(inode_state & I_DIRTY_DATASYNC)) goto out;
    /// Metadata IS written when I_DIRTY_DATASYNC is set.
    /// Asterinas: uses Dirty<InodeDesc>::is_dirty() as conservative approximation
    /// of I_DIRTY_DATASYNC — always persists metadata if desc was modified.
    ///
    /// # Lock
    /// Acquires inner read lock. evict_range and persist_inode_and_sync
    /// are both safe under read lock.
    pub(super) fn sync_data(&self) -> Result<()>;

    /// Syncs file data and inode metadata to disk.
    ///
    /// Linux: ext2_fsync (fs/ext2/file.c:155) calls generic_buffers_fsync
    /// which does: file_write_and_wait_range → sync_inode_metadata →
    /// blkdev_issue_flush.
    /// Asterinas: sync_data (evict dirty pages) + persist_inode_and_sync
    /// (write inode descriptor) + block_device.sync (flush).
    ///
    /// # Lock
    /// Acquires inner read lock for sync_data, then read lock again
    /// for persist_inode_and_sync. No lock held across both — safe.
    pub(super) fn sync_all(&self) -> Result<()>;
}
```

```rust
impl Ext2 {
    /// Syncs all cached inodes: writes back dirty pages and inode metadata.
    ///
    /// Linux: writeback framework iterates dirty inodes via sb->s_inodes
    /// and calls __ext2_write_inode + writeback dirty pages.
    /// Asterinas: iterates the inode cache, calls sync_all on each.
    ///
    /// Follows ext2_old pattern (block_group.rs:266) and exFAT pattern
    /// (fs.rs:411) which iterate cached inodes and call sync_all.
    pub fn sync_all_inodes(&self) -> Result<()>;
}
```

```rust
/// VFS Inode: sync operations.
impl VfsInode for Inode {
    fn sync_all(&self) -> Result<()>;
    fn sync_data(&self) -> Result<()>;
}
```

```rust
/// VFS FileSystem: sync operation.
impl FileSystem for Ext2 {
    fn sync(&self) -> Result<()>;
}
```

[SPECIFICATION]

## InodeInner::sync_data

Writes back all dirty pages from the page cache to the block device.

Linux equivalent: `file_write_and_wait_range` (mm/filemap.c:777) calls
`filemap_fdatawrite_range` (WB_SYNC_ALL) to initiate writeback of all dirty
pages, then `__filemap_fdatawait_range` to wait for completion.

Pre:
- Caller holds at least read lock on InodeInner.

Post (success):
- Obtains `file_size = desc.size as usize`.
- If `file_size == 0`, returns `Ok(())` immediately.
- Calls `page_cache.evict_range(0..file_size)`.
  - evict_range iterates all pages in range, writes back Dirty pages
    via `PageCacheBackend::write_page_async`, waits for completion,
    marks them UpToDate.
- Returns `Ok(())`.

Post (failure):
- `Err(EIO)` if any page writeback fails.

Note: evict_range does NOT remove pages from LruCache — it only writes
back dirty pages and changes their state to UpToDate. Pages remain cached
for future reads.

## Inode::sync_data

Per-file data sync (fdatasync path).

Linux equivalent: `generic_buffers_fsync_noflush` (fs/buffer.c:602) calls
`file_write_and_wait_range` then conditionally writes metadata:
```c
// fs/buffer.c:614-617
if (datasync && !(inode_state_read_once(inode) & I_DIRTY_DATASYNC))
    goto out;  // skip metadata ONLY if no datasync-relevant dirt
err = sync_inode_metadata(inode, 1);  // otherwise, write metadata
```
POSIX requires fdatasync to persist metadata needed to retrieve the data
(i.e., i_size and block mappings). Linux uses `I_DIRTY_DATASYNC` to track
this. Asterinas uses `Dirty<InodeDesc>::is_dirty()` as a conservative
approximation.

Pre:
- None (public API).

Post (success):
- Obtains `fs` via `self.fs_arc()`.
- Acquires inner read lock.
- Calls `inner.sync_data()` to write back dirty pages.
- If `inner.desc.is_dirty()`: calls `inner.persist_inode_and_sync(&fs)`
  to persist inode metadata (i_size, block pointers, timestamps).
  - This ensures that after a file-extending write + fdatasync, the new
    i_size and block mappings are persisted. Without this, a crash would
    lose the size change and the data would appear truncated.
- Releases read lock.
- Calls `fs.block_device().sync()` to flush device write cache.
  - Linux: `blkdev_issue_flush` at end of `generic_buffers_fsync`.
- Returns `Ok(())`.

Post (failure):
- `Err(EIO)` if page writeback or device flush fails.

## Inode::sync_all

Per-file full sync (fsync path).

Linux equivalent: `ext2_fsync` (fs/ext2/file.c:155) calls
`generic_buffers_fsync` which does:
1. `file_write_and_wait_range` — write back dirty pages
2. `sync_mapping_buffers` — sync associated metadata buffers
3. `sync_inode_metadata` → `__ext2_write_inode` — write inode to disk
4. `blkdev_issue_flush` — flush device cache

Pre:
- None (public API).

Post (success):
- Obtains `fs` via `self.fs_arc()`.
- Step 1: Acquires inner read lock, calls `inner.sync_data()`.
  Releases read lock.
- Step 2: Acquires inner read lock, calls `inner.persist_inode_and_sync(&fs)`.
  This writes the raw inode descriptor to the inode table page cache
  and syncs filesystem metadata (superblock, group descriptors).
  Releases read lock.
- Step 3: Calls `fs.block_device().sync()` to flush device cache.
- Returns `Ok(())`.

Post (failure):
- `Err(EIO)` if any step fails. Partial sync is possible (data written
  but metadata not, or vice versa). This matches Linux behavior where
  `generic_buffers_fsync` continues after partial failures.

Lock protocol:
- Read lock acquired and released twice (once for data, once for metadata).
- No lock held across both operations — avoids holding lock during
  potentially long I/O.
- persist_inode_and_sync under read lock is safe: it only reads desc
  fields and calls fs.write_inode_desc (which writes to the block group's
  inode table page cache, a separate lock domain).

## Ext2::sync_all_inodes

Filesystem-level inode sync for `sync(2)` / `syncfs(2)`.

Linux equivalent: The writeback framework iterates `sb->s_inodes` (all
inodes on the superblock's inode list) and calls `__ext2_write_inode`
plus page writeback for each dirty inode.

Asterinas: Since there is no global writeback framework, the filesystem
must iterate its own cached inodes. This follows the pattern established
by ext2_old (`sync_all_inodes` in block_group.rs:266) and exFAT
(`sync` in fs.rs:411).

Pre:
- Called from `FileSystem::sync()`.

Post (success):
- Iterates all `BlockGroup`s in `self.block_groups`.
- For each group, calls `group.sync_all_inodes()` which:
  1. Evicts unreferenced inodes (`Arc::strong_count == 1`) from the
     per-group `BTreeMap<u32, Arc<Inode>>` cache.
  2. For evicted inodes with `nlink == 0`: runs `truncate_blocks(0)` +
     `free_inode` (bitmap clear) — equivalent to Linux `ext2_evict_inode`.
  3. For evicted inodes with `nlink > 0`: calls `sync_all()` before drop.
  4. Calls `sync_all()` on all remaining cached inodes.
- Aggregates `EvictResult` (freed inode/dir counts) across groups.
- If any inodes were freed, updates superblock `free_inodes_count`.
- Returns `Ok(())`.

Post (failure):
- `Err(EIO)` if any group sync or eviction fails.

## FileSystem::sync for Ext2

Filesystem-level sync.

Linux equivalent: `ext2_sync_fs` (fs/ext2/super.c:1308):
1. `dquot_writeback_dquots` — write quota (not applicable)
2. Clear `EXT2_VALID_FS` flag
3. `ext2_sync_super` — write superblock with updated free counts and wtime

Pre:
- Called from `sys_sync` / `sys_syncfs`.

Post (success):
- Calls `self.sync_all_inodes()` to write back dirty inode data.
  - Linux: writeback framework handles this before ext2_sync_fs is called.
  - Asterinas: must do it explicitly since there is no writeback framework.
- Calls `self.sync_metadata()` to write superblock, group descriptors,
  and bitmaps.
  - Linux: `ext2_sync_super` writes superblock.
  - Asterinas: `sync_metadata` writes superblock + group descriptors +
    bitmaps (more comprehensive than Linux's ext2_sync_fs alone, but
    Linux's writeback framework handles the rest separately).
- Calls `self.block_device().sync()` to flush device cache.
  - Linux: `sync_blockdev` called by VFS after ext2_sync_fs.
- Returns `Ok(())`.

Post (failure):
- `Err(EIO)` if any step fails.

## Deadlock Analysis

| Operation              | Lock held     | I/O path              | Callback needs    | Safe? |
|------------------------|---------------|-----------------------|-------------------|-------|
| InodeInner::sync_data  | read          | evict_range           | no Pager callback | YES   |
| Inode::sync_data       | read          | evict + persist(cond) | writes to BG cache| YES   |
| Inode::sync_data       | NONE          | device.sync           | none              | YES   |
| Inode::sync_all step1  | read          | evict_range           | no Pager callback | YES   |
| Inode::sync_all step2  | read          | persist_inode_and_sync| writes to BG cache| YES   |
| Inode::sync_all step3  | NONE          | device.sync           | none              | YES   |
| Ext2::sync             | NONE          | delegates to above    | none              | YES   |

evict_range safety: evict_range locks PageCacheManager::pages (Mutex) and
calls backend.write_page_async. The backend is `Weak<dyn PageCacheBackend>`
pointing to the Inode itself. `write_page_async` on Inode acquires
`inner.read()` — but the caller already holds inner read lock. Since
RwMutex read locks are reentrant in Asterinas (multiple readers allowed),
this is safe.

[DIFF]
Linux: `ext2_fsync` (file.c:155) delegates to `generic_buffers_fsync` which
  calls `file_write_and_wait_range` to write back dirty pages via the
  writeback framework (`address_space_operations::writepages`).
  → Asterinas: `Inode::sync_all` calls `evict_range(0..file_size)` which
  directly iterates the PageCacheManager's LruCache and writes back dirty
  pages via `PageCacheBackend::write_page_async`.
  Reason: Asterinas has no writeback framework. `evict_range` is the
  equivalent mechanism, already used by ext2_old (inode.rs:1175) and
  exFAT (inode.rs:584) for the same purpose.

Linux: `generic_buffers_fsync` calls `sync_mapping_buffers` to sync
  metadata buffers associated with the file's address_space.
  → Asterinas: `persist_inode_and_sync` writes the inode descriptor to
  the block group's inode table page cache and calls `sync_metadata`
  to flush superblock/group descriptors.
  Reason: Asterinas does not use buffer_heads. Inode metadata is written
  via the inode table page cache (spec 1-inode-table-page-cache).

Linux: `ext2_sync_fs` (super.c:1308) only syncs the superblock. Dirty
  inode writeback is handled by the VFS writeback framework before
  ext2_sync_fs is called.
  → Asterinas: `FileSystem::sync` must explicitly call `sync_all_inodes`
  before `sync_metadata` because there is no writeback framework.
  Reason: Without a global writeback thread, the filesystem must take
  responsibility for flushing dirty data during sync.

Linux: The writeback framework tracks dirty inodes via `sb->s_inodes`
  and `inode->i_io_list`. Eviction happens immediately when `i_count`
  drops to 0 via `iput_final` → `evict`.
  → Asterinas: Per-BlockGroup `BTreeMap<u32, Arc<Inode>>` inode cache.
  Eviction of unreferenced inodes (`Arc::strong_count == 1`) is deferred
  to `sync_all_inodes` time rather than happening immediately on last
  reference drop.
  Reason: Asterinas has no shrinker/LRU infrastructure yet. Deferred
  eviction at sync time is acceptable for current usage patterns.
  TODO: Integrate with memory pressure callbacks when Asterinas supports
  periodic writeback/shrinker (see phase-08-inode-cache.spec).

Linux: `generic_buffers_fsync_noflush` (buffer.c:614-617) conditionally
  writes metadata during fdatasync: skips only if `I_DIRTY_DATASYNC` is
  NOT set. When a file is extended (i_size or block mappings change),
  `I_DIRTY_DATASYNC` is set and metadata IS written.
  → Asterinas: `Inode::sync_data` checks `desc.is_dirty()` and
  conditionally calls `persist_inode_and_sync`. This is a conservative
  approximation — it may write metadata for non-datasync changes (e.g.,
  atime), but never misses datasync-relevant changes.
  Reason: POSIX requires fdatasync to persist metadata needed to retrieve
  the written data. Without this, a crash after write()+fdatasync() on an
  extended file would lose the new i_size, making data appear truncated.
  Note: ext2_old has the same gap (sync_data only does evict_range).

[TEST]
## InodeInner::sync_data
- File with dirty pages → pages written back, state becomes UpToDate
- File with no dirty pages → no-op, Ok(())
- Empty file (size 0) → no-op, Ok(())
- evict_range I/O failure → Err(EIO)

## Inode::sync_data
- Buffered write then sync_data → data persisted to disk
- Read back after sync_data → correct data
- sync_data on clean file → Ok(())
- Extending write + sync_data → i_size persisted (desc.is_dirty() triggers metadata write)
- Non-extending write + sync_data → metadata NOT written (desc clean)

## Inode::sync_all
- Buffered write then sync_all → data + metadata persisted
- Inode timestamps updated before sync → persisted to disk
- sync_all on clean file → Ok(()) (no-op evict + metadata write)
- I/O error during evict → Err(EIO)
- I/O error during persist → Err(EIO)

## FileSystem::sync
- sync writes back root inode dirty pages
- sync writes superblock and group descriptors
- sync flushes block device cache
- sync on clean filesystem → Ok(())

## Integration: fsync correctness
- Buffered write → fsync → direct read → sees written data
- Buffered write → fsync → remount → read → sees written data
- Multiple files: fsync on file A does not affect file B's dirty pages
