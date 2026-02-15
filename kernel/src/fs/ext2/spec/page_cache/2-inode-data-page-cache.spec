[PROMPT]
Provide modifications to `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_read_folio      → fs/ext2/inode.c:917
ext2_write_begin     → fs/ext2/inode.c:928
ext2_write_end       → fs/ext2/inode.c:939
ext2_write_failed    → fs/ext2/inode.c:59
ext2_get_block       → fs/ext2/inode.c:783
ext2_aops            → fs/ext2/inode.c:965
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
    /// Allocates contiguous blocks, returns the allocated range.
    pub fn alloc_blocks(&self, count: u32) -> Result<Range<u32>>;
    /// Frees `count` blocks starting at `start`.
    pub fn free_blocks(&self, start: u32, count: u32) -> Result<()>;
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
    pub fn page_cache(&self) -> &PageCache;
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
    /// Acquires inner read lock (compatible with caller's read lock).
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;

    /// Writes one data block from cache page to disk.
    ///
    /// Called by PageCache during writeback of dirty pages.
    /// Physical block MUST already be allocated by write_at before writeback.
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
    /// Reads file data from the PageCache into the provided buffer.
    ///
    /// Replaces direct block device I/O with cached page access.
    /// Linux equivalent: generic_file_read_iter → ext2_read_folio.
    ///
    /// # Arguments
    /// * `offset` - Byte offset within the file to start reading.
    /// * `buf` - Destination buffer to fill with file data.
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes read (may be < buf.len() near EOF).
    /// * `Err(EISDIR)` - Inode is a directory.
    /// * `Err(EIO)` - I/O failure or filesystem dropped.
    ///
    /// # Lock
    /// Acquires inner read lock for metadata, then accesses PageCache
    /// (cache miss callback re-acquires read lock — safe, read locks are reentrant).
    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize>;

    /// Writes file data through the PageCache from the provided buffer.
    ///
    /// Linux equivalent: generic_file_write_iter → ext2_write_begin/end.
    ///
    /// # Arguments
    /// * `offset` - Byte offset within the file to start writing.
    /// * `data` - Source data to write.
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes written.
    /// * `Err(EISDIR)` - Inode is a directory.
    /// * `Err(EIO)` - I/O failure or filesystem dropped.
    /// * `Err(ENOSPC)` - Block allocation failed.
    ///
    /// # Lock
    /// Phase 1: write lock — alloc blocks, resize PageCache, update size.
    /// Phase 2: release write lock → read lock — write data to PageCache.
    /// Phase 3: write lock — update timestamps, persist inode.
    pub fn write_at(&self, offset: usize, data: &[u8]) -> Result<usize>;

    /// Resizes this inode to `new_size` bytes.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1275 (ext2_setsize)
    ///
    /// # Arguments
    /// * `new_size` - Target size in bytes.
    ///
    /// # Lock
    /// Acquires write lock for the entire operation.
    /// PageCache resize happens inside the write lock.
    pub fn resize(&self, new_size: usize) -> Result<()>;
}
```

[SPECIFICATION]
Pre (Inode::new):
- `ino > 0`.
- `desc` contains a valid parsed inode descriptor.
- `fs` is a valid weak reference to the Ext2 instance.

Post (Inode::new: success):
- Uses `Arc::new_cyclic` to construct the Inode:
  1. Inside the closure, receives `weak_self: Weak<Inode>`.
  2. Computes `num_page_bytes` from `desc.blocks`:
     - `nblocks = desc.blocks as usize / (BLOCK_SIZE / SECTOR_SIZE)`
     - `num_page_bytes = nblocks * BLOCK_SIZE`
  3. Creates PageCache:
     - If `num_page_bytes == 0`: `PageCache::new(weak_self.clone() as _)`.
     - Else: `PageCache::with_capacity(num_page_bytes, weak_self.clone() as _)`.
  4. Stores `page_cache` in InodeInner alongside existing fields.
- Returns `Arc<Inode>` with fully initialized PageCache.

Post (Inode::new: failure):
- Panics only if PageCache allocation fails (system OOM).

Pre (Inode::read_page_async):
- `frame` is a valid allocated CachePage.

Post (read_page_async: success):
- Acquires `self.inner` read lock.
- Calls `inner.get_block(idx as u32)`:
  - If `Ok(Some(bid))`: creates `BioSegment::new_from_segment` from frame
    with `BioDirection::FromDevice`, submits async read via
    `fs.block_device().read_blocks_async(bid, bio_segment)`.
  - If `Ok(None)`: sparse hole — zero-fills the frame via
    `frame.writer().write(...)`, returns empty `BioWaiter`.
- Returns the BioWaiter.

Post (read_page_async: failure):
- `Err(EIO)` if `self.fs` weak reference cannot be upgraded.
- Propagates errors from `get_block` or block device I/O.

Pre (Inode::write_page_async):
- `frame` contains dirty data to write back.
- Physical block for `idx` MUST already be allocated.

Post (write_page_async: success):
- Acquires `self.inner` read lock.
- Calls `inner.get_block(idx as u32)`:
  - If `Ok(Some(bid))`: creates `BioSegment::new_from_segment` from frame
    with `BioDirection::ToDevice`, submits async write via
    `fs.block_device().write_blocks_async(bid, bio_segment)`.
  - If `Ok(None)`: `error!("write_page_async: no block mapping for idx {}", idx)`,
    returns `Err(EIO)`.

Post (write_page_async: failure):
- `Err(EIO)` if fs reference dead, block not mapped, or device write fails.

Pre (npages):
- (none, always callable)

Post (npages):
- Acquires `self.inner` read lock.
- Returns `inner.desc.blocks as usize / (BLOCK_SIZE / SECTOR_SIZE)`.
- Converts 512-byte sector count to block count.

Pre (read_at):
- `self` is a valid, non-freed Inode.

Post (read_at: success):
- Rejects directories: if `self.type_ == InodeType::Dir`, returns `Err(EISDIR)`.
- Acquires inner read lock, obtains `file_size = desc.size as usize`.
- If `offset >= file_size` or `buf.is_empty()`, returns `Ok(0)`.
- Computes `read_len = min(buf.len(), file_size - offset)`.
- Reads data via `inner.page_cache.pages().read_bytes(offset, &mut buf[..read_len])`.
  - PageCache automatically triggers `read_page_async` on cache miss.
  - `read_page_async` acquires inner read lock again (reentrant, no deadlock).
- Returns `Ok(read_len)`.

Post (read_at: failure):
- `Err(EISDIR)` if inode is a directory.
- `Err(EIO)` if PageCache I/O fails.

Pre (write_at):
- `self` is a valid, non-freed Inode.

Post (write_at: success):
- Rejects directories: if `self.type_ == InodeType::Dir`, returns `Err(EISDIR)`.
- If `data.is_empty()`, returns `Ok(0)`.
- Computes `end = offset + data.len()`.
- Phase 1 (write lock):
  - Acquires inner write lock.
  - For each logical block in `[offset/block_size .. end.div_ceil(block_size)]`:
    calls `get_or_alloc_block(iblock, true)` to ensure physical block exists.
  - If `end > current file size`:
    - Calls `page_cache.resize(end.align_up(BLOCK_SIZE))` to extend PageCache.
    - Updates `desc.size = end as u64`.
  - Releases write lock.
  - On alloc failure: calls `write_failed_cleanup` (truncate back to old size),
    returns error.
- Phase 2 (read lock):
  - Acquires inner read lock.
  - Writes data via `page_cache.pages().write_bytes(offset, data)`.
  - Releases read lock.
- Phase 3 (write lock):
  - Acquires inner write lock.
  - Updates `desc.mtime` and `desc.ctime` to current time.
  - Calls `persist_inode_and_sync`.
  - Releases write lock.
- Returns `Ok(data.len())`.

Post (write_at: failure):
- `Err(EISDIR)` if inode is a directory.
- `Err(EIO)` if fs dropped or PageCache I/O fails.
- `Err(ENOSPC)` if block allocation fails.
- On allocation failure mid-write: `write_failed_cleanup` truncates back to
  original size (Linux ext2_write_failed, fs/ext2/inode.c:59).

Pre (resize):
- `self` is a valid, non-freed Inode.

Post (resize: success):
- Acquires inner write lock for entire operation.
- Rejects non-regular/dir/symlink types: returns `Err(EINVAL)`.
- Rejects fast symlinks (blocks==0 && size<=60): returns `Err(EINVAL)`.
- Rejects APPEND_ONLY/IMMUTABLE flags: returns `Err(EPERM)`.
- If `new_size == old_size`, returns `Ok(())`.
- If shrinking and `new_size % block_size != 0`:
  zeroes tail of last block via PageCache (read page, zero from offset, write back).
- Calls `page_cache.resize(new_size.align_up(BLOCK_SIZE))`.
- Updates `desc.size = new_size as u64`.
- Calls `truncate_blocks(new_size)`.
- Updates timestamps, persists inode.

Post (resize: failure):
- `Err(EINVAL)` for invalid inode type or fast symlink.
- `Err(EPERM)` for immutable/append-only.
- `Err(EIO)` for I/O failures.

Invariant:
- All file data I/O goes through PageCache, never direct block device access.
- PageCacheBackend is implemented on Inode (outer struct), not InodeInner.
- read_page_async acquires inner read lock; callers holding read lock are safe (reentrant).
- write_at MUST release write lock before accessing PageCache to avoid deadlock.
- write_page_async expects blocks to be pre-allocated; None mapping is a bug (error! + EIO).
- Sparse holes (unallocated blocks) are zero-filled on read only.
- PageCache capacity is synchronized with inode size on resize/truncate/write-extend.
- No separate Backend struct needed; Inode itself is the PageCacheBackend.

[DIFF]
Linux: File data I/O uses VFS page cache with `address_space_operations` (ext2_aops).
  `ext2_read_folio` calls `mpage_read_folio(folio, ext2_get_block)` which fills
  page cache folios via the block mapping callback.
  → Asterinas: `impl PageCacheBackend for Inode` directly. `read_page_async`
  acquires inner read lock and calls `get_block` for the same mapping.
  Reason: Follows ExfatInode pattern (exfat/inode.rs:136). No separate backend struct.

Linux: `ext2_write_begin` calls `block_write_begin(mapping, pos, len, foliop, ext2_get_block)`
  which allocates blocks via `ext2_get_block(create=1)` and prepares the folio.
  → Asterinas: `write_at` pre-allocates blocks in Phase 1 (write lock), then writes
  through PageCache in Phase 2 (read lock). Block allocation is separated from
  PageCache writeback.
  Reason: Avoids deadlock — write lock cannot be held when PageCache triggers
  `write_page_async` callback which needs read lock.

Linux: `ext2_write_failed` truncates back to i_size on write failure.
  → Asterinas: `write_failed_cleanup` in write_at does the same rollback.
  Reason: Direct equivalent.

Linux: ext2_old `InodeBlockManager` implements `PageCacheBackend` as a separate
  struct with its own block_ptrs copy and IndirectBlockCache.
  → Asterinas (new): No separate backend struct. Inode itself implements
  PageCacheBackend, delegating to `InodeInner::get_block()` via read lock.
  Reason: Simpler architecture, no block_ptrs duplication, follows exFAT precedent.

[TEST]
## Inode::new
- Construct inode with size > 0 → PageCache created with correct capacity (blocks-based)
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
- Inode with blocks=16 (block_size=4096, sector_size=512) → returns 2
- Inode with blocks=0 → returns 0

## Inode::read_at
- Read from file with data → correct bytes returned via PageCache
- Read at offset >= file_size → Ok(0)
- Read with empty buffer → Ok(0)
- Read near EOF (buf extends past EOF) → clamped to file_size - offset
- Read from directory → Err(EISDIR)
- Read triggers cache miss → read_page_async called, data loaded from disk

## Inode::write_at
- Write within existing file size → data written via PageCache, timestamps updated
- Write extending file → blocks allocated, PageCache resized, size updated
- Write to directory → Err(EISDIR)
- Write empty data → Ok(0)
- Block allocation fails mid-write → write_failed_cleanup truncates back, Err(ENOSPC)
- Verify 3-phase lock protocol: no deadlock on cache miss during Phase 2

## Inode::resize
- Truncate file to smaller size → PageCache shrunk, blocks freed, tail zeroed
- Extend file to larger size → PageCache extended, size updated
- Resize to same size → Ok(()), no-op
- Resize non-regular/dir/symlink → Err(EINVAL)
- Resize fast symlink → Err(EINVAL)
- Resize immutable file → Err(EPERM)
- Resize append-only file → Err(EPERM)
