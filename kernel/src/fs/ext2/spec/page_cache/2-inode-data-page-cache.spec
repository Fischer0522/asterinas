[PROMPT]
Provide modifications to `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_read_folio      → fs/ext2/inode.c:917
ext2_readahead       → fs/ext2/inode.c:922
ext2_write_begin     → fs/ext2/inode.c:928
ext2_write_end       → fs/ext2/inode.c:939
ext2_write_failed    → fs/ext2/inode.c:59
ext2_get_block       → fs/ext2/inode.c:783
ext2_get_blocks      → fs/ext2/inode.c:624
ext2_setsize         → fs/ext2/inode.c:1275
block_truncate_page  → fs/buffer.c:2654
truncate_setsize     → mm/truncate.c:812
generic_write_end    → fs/buffer.c:2300
ExfatInode (style)   → kernel/src/fs/exfat/inode.rs:136

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::fs::Ext2;
```

```rust
use super::utils::Dirty;
```

```rust
/// PageCache infrastructure.
pub struct PageCache { /* ... */ }
pub trait PageCacheBackend: Sync + Send {
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;
    fn write_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;
    fn npages(&self) -> usize;
}

impl PageCache {
    /// Creates an empty page cache.
    pub fn new(backend: Weak<dyn PageCacheBackend>) -> Result<Self>;
    /// Creates a page cache with initial capacity.
    pub fn with_capacity(capacity: usize, backend: Weak<dyn PageCacheBackend>) -> Result<Self>;
    /// Returns the Vmo object for read/write operations.
    pub fn pages(&self) -> &Arc<Vmo>;
    /// Resizes the page cache.
    /// On shrink: fill_zeros tail gap (may trigger read_page_async),
    ///            then decommit_pages (may trigger write_page_async for dirty pages).
    pub fn resize(&self, new_size: usize) -> Result<()>;
    /// Discards pages in range WITHOUT writeback. No backend callbacks.
    pub fn discard_range(&self, range: Range<usize>);
    /// Evicts dirty pages in range WITH writeback. Triggers write_page_async.
    pub fn evict_range(&self, range: Range<usize>) -> Result<()>;
    /// Fills range with zeros via Vmo::write (may trigger read_page_async on commit).
    pub fn fill_zeros(&self, range: Range<usize>) -> Result<()>;
}
```

```rust
/// The Ext2 inode public handle.
/// PageCacheBackend is implemented directly on Inode (exFAT pattern).
#[derive(Debug)]
pub struct Inode {
    /// 1-based inode number.
    ino: u32,
    /// Inode type (file, directory, symlink, etc.).
    type_: InodeType,
    /// Mutable inode state, protected by RwMutex.
    inner: RwMutex<InodeInner>,
    /// Index of the block group this inode belongs to.
    block_group_idx: usize,
    /// Weak reference to the owning Ext2 filesystem.
    fs: Weak<Ext2>,
    /// VFS extension data.
    extension: Extension,
}
```

```rust
/// Mutable inode state.
#[derive(Debug)]
pub struct InodeInner {
    /// In-memory inode descriptor wrapped in Dirty tracker.
    desc: Dirty<InodeDesc>,
    /// Whether this inode has been freed (unlinked + nlink=0).
    is_freed: bool,
    /// Weak back-reference to the owning Inode Arc.
    weak_self: Weak<Inode>,
    /// Weak reference to the filesystem.
    fs: Weak<Ext2>,
    /// Per-inode data PageCache for file/directory content.
    /// Backend is Weak<Inode> as Weak<dyn PageCacheBackend>.
    page_cache: PageCache,
}
```

```rust
/// In-memory inode descriptor.
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeDesc {
    type_: InodeType,
    perm: FilePerm,
    uid: u32,
    gid: u32,
    size: u64,
    atime: Duration,
    ctime: Duration,
    mtime: Duration,
    dtime: Duration,
    links_count: u16,
    blocks: u32,
    flags: FileFlags,
    file_acl: u32,
    block_ptrs: [u32; 15],
}
```

```rust
impl InodeInner {
    /// Resolves logical block number to block path through indirect tree.
    pub(super) fn block_to_path(&self, iblock: u32) -> Result<BlockPath>;
    /// Returns the physical Bid for a logical block, or None for sparse holes.
    pub(super) fn get_block(&self, iblock: u32) -> Result<Option<Bid>>;
    /// Returns the physical Bid for a logical block, allocating if `create` is true.
    pub(super) fn get_or_alloc_block(&mut self, iblock: u32, create: bool) -> Result<Option<Bid>>;
    /// Truncates blocks beyond `new_size` bytes.
    pub(super) fn truncate_blocks(&mut self, new_size: usize) -> Result<()>;
    /// Persists inode descriptor to disk.
    pub(super) fn persist_inode_and_sync(&self, fs: &Ext2) -> Result<()>;
}
```

```rust
impl Ext2 {
    /// Returns a reference to the underlying block device.
    pub fn block_device(&self) -> &dyn BlockDevice;
    /// Returns the filesystem block size in bytes.
    pub fn block_size(&self) -> usize;
    /// Returns the SuperBlock (for total_inodes, etc.).
    pub fn super_block(&self) -> &SuperBlock;
}
```

[GUARANTEE]
```rust
impl Inode {
    /// Creates a new Inode with an associated data PageCache.
    ///
    /// Uses `Arc::new_cyclic` to resolve the self-referential dependency:
    /// Inode is the PageCacheBackend, PageCache holds Weak<Inode>.
    /// Follows the ExfatInode construction pattern (exfat/inode.rs:840).
    ///
    /// # Arguments
    /// * `ino` - 1-based inode number.
    /// * `type_` - Inode type (file, directory, symlink, etc.).
    /// * `desc` - Parsed inode descriptor wrapped in Dirty tracker.
    /// * `block_group_idx` - Index of the block group this inode belongs to.
    /// * `fs` - Weak reference to the owning Ext2 filesystem.
    ///
    /// # Returns
    /// * `Arc<Self>` - The constructed inode with initialized PageCache.
    pub fn new(
        ino: u32,
        type_: InodeType,
        desc: Dirty<InodeDesc>,
        block_group_idx: usize,
        fs: Weak<Ext2>,
    ) -> Arc<Self>;

    /// Returns a reference to this inode's data PageCache.
    pub(super) fn page_cache(&self) -> &PageCache;
}
```

```rust
impl PageCacheBackend for Inode {
    /// Reads one data block from disk into the cache page.
    ///
    /// Called by PageCache on cache miss during read operations.
    /// Acquires inner read lock to resolve logical→physical block mapping.
    ///
    /// # Arguments
    /// * `idx` - Logical block number (0-based) within the inode's data.
    /// * `frame` - Target CachePage to fill.
    ///
    /// # Lock
    /// Acquires inner read lock (compatible with caller's read lock — RwMutex
    /// read locks are reentrant).
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;

    /// Writes one data block from cache page to disk.
    ///
    /// Called by PageCache during writeback (eviction or explicit sync).
    /// Physical block MUST already be allocated before writeback occurs.
    ///
    /// # Arguments
    /// * `idx` - Logical block number (0-based) within the inode's data.
    /// * `frame` - Source CachePage containing dirty data.
    ///
    /// # Lock
    /// Acquires inner read lock.
    fn write_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;

    /// Returns the number of data blocks for this inode.
    ///
    /// # Lock
    /// Acquires inner read lock.
    fn npages(&self) -> usize;
}
```

```rust
impl Inode {
    /// Reads file data from the PageCache into the provided writer.
    ///
    /// Replaces direct block device I/O with cached page access.
    /// Linux equivalent: generic_file_read_iter → ext2_read_folio.
    ///
    /// # Arguments
    /// * `offset` - Byte offset within the file to start reading.
    /// * `writer` - Destination VmWriter to fill with file data.
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes read (may be < writer.avail() near EOF).
    /// * `Err(EISDIR)` - Inode is a directory.
    /// * `Err(EIO)` - I/O failure.
    ///
    /// # Lock
    /// Acquires inner read lock for metadata access.
    /// PageCache read may trigger read_page_async callback which re-acquires
    /// inner read lock — safe because RwMutex read locks are reentrant.
    pub(super) fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize>;

    /// Writes file data through the PageCache from the provided reader.
    ///
    /// Linux equivalent: generic_file_write_iter → ext2_write_begin/end.
    ///
    /// Uses a three-phase lock protocol to avoid deadlock:
    /// - Phase 1 (write lock): pre-allocate blocks, resize PageCache if extending.
    /// - Phase 2 (no inner lock): write data through PageCache.
    /// - Phase 3 (write lock): update timestamps, persist inode.
    ///
    /// # Arguments
    /// * `offset` - Byte offset within the file to start writing.
    /// * `reader` - Source VmReader containing data to write.
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes written.
    /// * `Err(EISDIR)` - Inode is a directory.
    /// * `Err(EIO)` - I/O failure.
    /// * `Err(ENOSPC)` - Block allocation failed.
    pub(super) fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize>;

    /// Resizes this inode to `new_size` bytes.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1275 (ext2_setsize)
    ///
    /// Uses a two-phase approach to avoid deadlock on shrink:
    /// - Phase 1 (no inner lock): tail block zeroing via PageCache fill_zeros.
    /// - Phase 2 (write lock): discard out-of-range pages, resize PageCache,
    ///   truncate blocks, update metadata, persist.
    ///
    /// VFS layer guarantees no concurrent write_at during resize (serialized
    /// via setattr), matching Linux's i_rwsem guarantee for ext2_setsize.
    ///
    /// # Arguments
    /// * `new_size` - Target size in bytes.
    pub(super) fn resize(&self, new_size: usize) -> Result<()>;
}
```

[SPECIFICATION]

## Inode::new

Pre:
- `ino > 0`.
- `desc` contains a valid parsed inode descriptor.
- `fs` is a valid weak reference to the Ext2 instance.

Post (success):
- Uses `Arc::new_cyclic` to construct the Inode:
  1. Inside the closure, receives `weak_self: Weak<Inode>`.
  2. Computes `num_page_bytes` from `desc.size`:
     - `num_page_bytes = (desc.size as usize).align_up(BLOCK_SIZE)`
  3. Creates PageCache:
     - If `num_page_bytes == 0`: `PageCache::new(weak_self.clone() as _)`.
     - Else: `PageCache::with_capacity(num_page_bytes, weak_self.clone() as _)`.
  4. Stores `page_cache` in InodeInner alongside existing fields.
- Returns `Arc<Inode>` with fully initialized PageCache.

Post (failure):
- Panics only if PageCache allocation fails (system OOM).

## PageCacheBackend for Inode

Pre (read_page_async):
- `frame` is a valid allocated CachePage.

Post (read_page_async: success):
- Acquires `self.inner` read lock.
- Upgrades `self.fs` weak reference → `Err(EIO)` if dead.
- Calls `inner.get_block(idx as u32)`:
  - `Ok(Some(bid))`: creates `BioSegment::new_from_segment` from frame
    with `BioDirection::FromDevice`, submits async read via
    `fs.block_device().read_blocks_async(bid, bio_segment)`.
    Returns the `BioWaiter`.
  - `Ok(None)`: sparse hole — zero-fills the frame via
    `frame.writer().fill_zeros(BLOCK_SIZE)`, returns empty `BioWaiter::new()`.

Post (read_page_async: failure):
- `Err(EIO)` if `self.fs` weak reference cannot be upgraded.
- Propagates errors from `get_block` or block device I/O.

Pre (write_page_async):
- `frame` contains dirty data to write back.
- Physical block for `idx` MUST already be allocated (by write_at Phase 1).

Post (write_page_async: success):
- Acquires `self.inner` read lock.
- Upgrades `self.fs` weak reference → `Err(EIO)` if dead.
- Calls `inner.get_block(idx as u32)`:
  - `Ok(Some(bid))`: creates `BioSegment::new_from_segment` from frame
    with `BioDirection::ToDevice`, submits async write via
    `fs.block_device().write_blocks_async(bid, bio_segment)`.
    Returns the `BioWaiter`.
  - `Ok(None)`: bug — `error!("write_page_async: no block mapping for idx {}", idx)`,
    returns `Err(EIO)`.

Post (write_page_async: failure):
- `Err(EIO)` if fs reference dead, block not mapped, or device write fails.

Pre (npages):
- (none, always callable)

Post (npages):
- Acquires `self.inner` read lock.
- Returns `(inner.desc.size as usize).align_up(BLOCK_SIZE) / BLOCK_SIZE`.

## Inode::read_at

Pre:
- `self` is a valid, non-freed Inode.

Post (success):
- Rejects directories: if `self.type_ == InodeType::Dir`, returns `Err(EISDIR)`.
- If `writer.avail() == 0`, returns `Ok(0)`.
- Acquires inner read lock, obtains `file_size = desc.size as usize`.
- If `offset >= file_size`, returns `Ok(0)`.
- Computes `read_len = min(writer.avail(), file_size - offset)`.
- Reads data via `inner.page_cache.pages().read(offset, writer)`.
  - PageCache automatically triggers `read_page_async` on cache miss.
  - `read_page_async` acquires inner read lock again (reentrant, no deadlock).
- Releases read lock.
- Updates `atime` via `self.set_atime(now())`.
- Returns `Ok(read_len)`.

Post (failure):
- `Err(EISDIR)` if inode is a directory.
- `Err(EIO)` if PageCache I/O fails.

## Inode::write_at

Pre:
- `self` is a valid, non-freed Inode.

Post (success):
- Rejects directories: if `self.type_ == InodeType::Dir`, returns `Err(EISDIR)`.
- If `reader.remain() == 0`, returns `Ok(0)`.
- Computes `write_len = reader.remain()`, `end = offset + write_len`.

- Phase 1 — pre-allocate blocks and extend (write lock):
  - Acquires inner write lock.
  - Obtains `old_size = desc.size as usize`, `block_size = fs.block_size()`.
  - For each logical block in `offset / block_size .. end.div_ceil(block_size)`:
    calls `inner.get_or_alloc_block(iblock, true)` to ensure physical block exists.
  - If `end > old_size`:
    - `new_size = end`.
    - Calls `inner.page_cache.resize(new_size.align_up(block_size))` to extend.
      (Grow path: Vmo::resize only updates size atomically, no callbacks — safe
      under write lock.)
    - Updates `inner.desc.size = new_size as u64`.
  - Releases write lock.
  - On alloc failure: calls `write_failed_cleanup` (see below), returns error.

- Phase 2 — write data through PageCache (no inner lock):
  - Accesses `self.page_cache().pages().write(offset, reader)`.
  - PageCache may trigger:
    - `read_page_async` on cache miss → acquires inner read lock → safe (no lock held).
    - `update_page` to mark dirty → no backend callback.
  - No inner lock held during this phase — deadlock impossible.

- Phase 3 — update metadata (write lock):
  - Acquires inner write lock.
  - Updates `desc.mtime` and `desc.ctime` to `now()`.
  - Calls `inner.persist_inode_and_sync(&fs)`.
  - Releases write lock.

- Returns `Ok(write_len)`.

write_failed_cleanup (on Phase 1 allocation failure):
  - Linux: ext2_write_failed (fs/ext2/inode.c:59) — if `to > i_size`,
    truncate_pagecache + ext2_truncate_blocks back to i_size.
  - Asterinas: if `end > old_size` and blocks were partially allocated:
    - `inner.page_cache.discard_range(old_size_aligned..end_aligned)` — discard
      any pages for the partially-allocated region (no callback, safe under write lock).
    - `inner.page_cache.resize(old_size_aligned)` — shrink back (pages already
      discarded, no callback).
    - `inner.truncate_blocks(old_size)` — free excess blocks.
    - `inner.desc.size = old_size as u64` — restore original size.
    - Log error on cleanup failure (best-effort, matches Linux void return).

Post (failure):
- `Err(EISDIR)` if inode is a directory.
- `Err(EIO)` if fs dropped or PageCache I/O fails.
- `Err(ENOSPC)` if block allocation fails.
- On allocation failure: `write_failed_cleanup` rolls back to original state.

## Inode::resize

Pre:
- `self` is a valid, non-freed Inode.
- VFS layer guarantees no concurrent `write_at` during `resize`
  (Linux: i_rwsem held exclusively by setattr path).

Post (success):
- Acquires inner read lock for pre-checks:
  - Rejects non-regular/dir/symlink types: returns `Err(EINVAL)`.
  - Rejects fast symlinks (blocks==0 && size<=60): returns `Err(EINVAL)`.
  - Rejects APPEND_ONLY/IMMUTABLE flags: returns `Err(EPERM)`.
  - Obtains `old_size = desc.size as usize`, `block_size`.
  - If `new_size == old_size`, returns `Ok(())`.
  - Releases read lock.

- If shrinking (`new_size < old_size`):

  - Step 1 — tail block zeroing (no inner lock):
    - Linux: block_truncate_page (fs/buffer.c:2654) — called BEFORE
      filemap_invalidate_lock, may trigger page cache I/O.
    - If `new_size % block_size != 0`:
      - `zero_from = new_size`, `zero_to = new_size.align_up(block_size)`.
      - Calls `self.page_cache().fill_zeros(zero_from..zero_to)`.
        - May trigger `read_page_async` → acquires inner read lock → safe
          (no inner lock held).
      - If tail block is a sparse hole (get_block returns None), fill_zeros
        writes to a zero-committed page — harmless, page will be discarded next.

  - Step 2 — truncate (write lock):
    - Acquires inner write lock.
    - Re-reads `old_size = desc.size` (guard against TOCTOU — though VFS
      serialization makes this defensive only).
    - `old_size_aligned = old_size.align_up(block_size)`.
    - `new_size_aligned = new_size.align_up(block_size)`.
    - If `new_size_aligned < old_size_aligned`:
      - `inner.page_cache.discard_range(new_size_aligned..old_size_aligned)`.
        - Discards pages beyond new boundary WITHOUT writeback.
        - No backend callbacks — safe under write lock.
        - Linux equivalent: truncate_pagecache (mm/truncate.c:812).
    - `inner.page_cache.resize(new_size_aligned)`.
      - Pages already discarded → decommit_pages finds nothing → no callbacks.
      - fill_zeros gap: new_size_aligned is block-aligned → no gap → no callback.
      - Safe under write lock.
    - Linux: truncate_setsize updates i_size BEFORE block release.
    - `inner.desc.size = new_size as u64`.
    - `inner.truncate_blocks(new_size)` — frees disk blocks.
    - Updates `desc.mtime` and `desc.ctime` to `now()`.
    - Calls `inner.persist_inode_and_sync(&fs)`.
    - Releases write lock.

- If growing (`new_size > old_size`):
  - Acquires inner write lock.
  - `inner.page_cache.resize(new_size.align_up(block_size))`.
    - Grow: Vmo::resize only updates size, no callbacks — safe under write lock.
  - `inner.desc.size = new_size as u64`.
  - No block allocation needed (ext2 sparse files — blocks allocated on write).
  - Updates `desc.mtime` and `desc.ctime` to `now()`.
  - Calls `inner.persist_inode_and_sync(&fs)`.
  - Releases write lock.

Post (failure):
- `Err(EINVAL)` for invalid inode type or fast symlink.
- `Err(EPERM)` for immutable/append-only.
- `Err(EIO)` for I/O failures during tail zeroing, truncate, or persist.

## Deadlock Analysis

Lock hierarchy (acquire in this order only):
```
Level 0: Inode (Arc, no lock)
Level 1: inner: RwMutex<InodeInner>  (read or write)
Level 2: PageCacheManager::pages: Mutex<LruCache>
Level 3: PageCacheManager::ra_state: Mutex<ReadaheadState>
```

PageCache callback lock requirements:
- `read_page_async`: acquires inner READ lock (Level 1).
- `write_page_async`: acquires inner READ lock (Level 1).

Safe paths (no deadlock):
| Operation              | Held lock     | PageCache op        | Callback needs    | Safe? |
|------------------------|---------------|---------------------|-------------------|-------|
| read_at                | inner READ    | pages().read        | read_page → READ  | YES (reentrant) |
| write_at Phase 2       | NONE          | pages().write       | read_page → READ  | YES |
| write_at Phase 1 grow  | inner WRITE   | resize (grow)       | no callbacks      | YES |
| resize shrink Step 1   | NONE          | fill_zeros          | read_page → READ  | YES |
| resize shrink Step 2   | inner WRITE   | discard_range       | no callbacks      | YES |
| resize shrink Step 2   | inner WRITE   | resize (shrink)     | no callbacks (*)  | YES |
| resize grow            | inner WRITE   | resize (grow)       | no callbacks      | YES |
| evict/sync (external)  | NONE          | evict_range         | write_page → READ | YES |

(*) Safe because discard_range removes all pages in the truncated range BEFORE
resize is called. resize's decommit_pages finds no pages to evict → no
write_page_async callback.

Dangerous paths (prevented by design):
- inner WRITE + PageCache fill_zeros → read_page → inner READ → DEADLOCK.
  Prevention: resize Step 1 (tail zeroing) runs WITHOUT inner lock.
- inner WRITE + PageCache resize (shrink with dirty pages) → write_page → inner READ → DEADLOCK.
  Prevention: discard_range clears pages first, then resize finds nothing to evict.

Invariant:
- All file data I/O goes through PageCache, never direct block device access.
- PageCacheBackend is implemented on Inode (outer struct), not InodeInner.
- read_page_async/write_page_async acquire inner read lock only.
- write_at MUST release write lock before accessing PageCache data path.
- resize shrink MUST do tail zeroing before acquiring write lock.
- resize shrink MUST discard truncated pages before calling page_cache.resize().
- write_page_async expects blocks to be pre-allocated; None mapping is a bug (EIO).
- Sparse holes (unallocated blocks) are zero-filled on read via read_page_async.
- PageCache capacity tracks file size (block-aligned) on resize/truncate/write-extend.

[DIFF]
Linux: File data I/O uses VFS page cache with `address_space_operations` (ext2_aops).
  `ext2_read_folio` calls `mpage_read_folio(folio, ext2_get_block)` which fills
  page cache folios via the block mapping callback.
  → Asterinas: `impl PageCacheBackend for Inode` directly. `read_page_async`
  acquires inner read lock and calls `get_block` for the same mapping.
  Reason: Follows ExfatInode pattern (exfat/inode.rs:136). No separate backend struct.

Linux: `ext2_write_begin` calls `block_write_begin(mapping, pos, len, foliop, ext2_get_block)`
  which allocates blocks via `ext2_get_block(create=1)` inside the page cache callback,
  protected by `truncate_mutex` (independent from `i_rwsem`).
  → Asterinas: `write_at` pre-allocates ALL blocks in Phase 1 (write lock), then writes
  through PageCache in Phase 2 (no lock). Block allocation is separated from
  PageCache data path.
  Reason: Linux uses two independent locks (i_rwsem + truncate_mutex) so the page cache
  callback never contends with the inode metadata lock. Our design uses a single
  RwMutex for both, so we must release the write lock before touching PageCache.
  Three-phase protocol achieves the same deadlock freedom via temporal separation
  instead of spatial separation (two locks).

Linux: `generic_write_end` (fs/buffer.c:2300) updates `i_size` while holding folio lock
  + `i_rwsem`, ensuring writeback cannot write beyond i_size.
  → Asterinas: Phase 1 updates `desc.size` before Phase 2 writes data. Phase 3
  updates timestamps. Size is visible to concurrent readers between phases, which
  matches Linux's per-chunk write_begin/write_end loop where i_size advances
  incrementally.

Linux: `ext2_write_failed` (fs/ext2/inode.c:59) calls `truncate_pagecache(inode, i_size)`
  + `ext2_truncate_blocks(inode, i_size)` on write failure.
  → Asterinas: `write_failed_cleanup` uses `discard_range` + `resize` + `truncate_blocks`
  to roll back. discard_range is used instead of truncate_pagecache because we are
  under write lock and cannot trigger writeback callbacks.

Linux: `ext2_setsize` (fs/ext2/inode.c:1275) calls `block_truncate_page` BEFORE
  `filemap_invalidate_lock`, then `truncate_setsize` + `__ext2_truncate_blocks`
  under `filemap_invalidate_lock`.
  → Asterinas: resize Step 1 (fill_zeros for tail) runs without inner lock (matches
  block_truncate_page before invalidate_lock). Step 2 acquires write lock, does
  discard_range + resize + truncate_blocks (matches truncate_setsize +
  __ext2_truncate_blocks under invalidate_lock).
  Reason: Exact structural correspondence. fill_zeros may trigger read_page_async
  which needs read lock — must not hold write lock. discard_range has no callbacks —
  safe under write lock.

Linux: ext2_old `InodeBlockManager` implements `PageCacheBackend` as a separate struct
  with its own `RwMutex<BlockPtrs>` copy and `RwMutex<IndirectBlockCache>`, requiring
  dual-write synchronization on every block pointer modification.
  → Asterinas (new): No separate backend struct. Inode itself implements
  PageCacheBackend, delegating to `InodeInner::get_block()` via read lock.
  Single source of truth for block_ptrs (in InodeDesc). No dual-write needed.
  Tradeoff: Requires three-phase lock protocol instead of independent lock domains.

[TEST]
## Inode::new
- Construct inode with size > 0 → PageCache created with correct capacity (size-aligned)
- Construct inode with size == 0 (new empty file) → PageCache created empty
- Verify page_cache() returns valid reference after construction

## PageCacheBackend::read_page_async
- Read mapped block → BioWaiter returned, frame filled with block data
- Read sparse hole (get_block returns None) → frame zero-filled, empty BioWaiter
- fs Weak reference dead → Err(EIO)
- get_block returns error → error propagated

## PageCacheBackend::write_page_async
- Write mapped block → BioWaiter returned, data written to device
- Block not mapped (get_block returns None) → error!() logged, Err(EIO)
- fs Weak reference dead → Err(EIO)

## PageCacheBackend::npages
- Inode with size=8192 (block_size=4096) → returns 2
- Inode with size=0 → returns 0
- Inode with size=1 → returns 1 (aligned up to one block)

## Inode::read_at
- Read from file with data → correct bytes returned via PageCache
- Read at offset >= file_size → Ok(0)
- Read with empty writer → Ok(0)
- Read near EOF (writer extends past EOF) → clamped to file_size - offset
- Read from directory → Err(EISDIR)
- Read triggers cache miss → read_page_async called, data loaded from disk
- Concurrent reads → both succeed (read locks are compatible)

## Inode::write_at
- Write within existing file size → data written via PageCache, timestamps updated
- Write extending file → blocks allocated in Phase 1, PageCache resized, size updated
- Write to directory → Err(EISDIR)
- Write empty reader → Ok(0)
- Block allocation fails mid-write → write_failed_cleanup rolls back, Err(ENOSPC)
- Verify 3-phase lock protocol: no deadlock on cache miss during Phase 2
- Partial block write → read-modify-write handled by PageCache (read_page_async
  loads existing data, then write overwrites partial range)

## Inode::resize
- Truncate file to smaller size → tail zeroed, pages discarded, blocks freed
- Truncate to block-aligned size → no tail zeroing needed, pages discarded
- Truncate file with dirty cached pages → pages discarded (not written back)
- Extend file to larger size → PageCache extended, size updated, no block alloc
- Resize to same size → Ok(()), no-op
- Resize non-regular/dir/symlink → Err(EINVAL)
- Resize fast symlink → Err(EINVAL)
- Resize immutable file → Err(EPERM)
- Resize append-only file → Err(EPERM)
- Verify shrink Step 1 (fill_zeros) runs without inner lock → no deadlock
- Verify shrink Step 2 discard_range before resize → no writeback callback
