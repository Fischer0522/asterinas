[PROMPT]
Provide modifications to `kernel/src/fs/ext2/inode.rs` and `kernel/src/fs/ext2/fs.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_read_folio      → fs/ext2/inode.c:917
ext2_write_begin     → fs/ext2/inode.c:928
ext2_get_block       → fs/ext2/inode.c:783
ext2_get_folio       → fs/ext2/dir.c:189
ext2_aops            → fs/ext2/inode.c:965
InodeBlockManager (ext2_old) → kernel/src/fs/ext2_old/inode.rs:1855

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
#[derive(Debug)]
pub struct Inode {
    ino: u32,
    type_: InodeType,
    inner: RwMutex<InodeInner>,
    block_group_idx: usize,
    fs: Weak<Ext2>,
}
```

```rust
/// Mutable inode state.
#[derive(Debug)]
pub struct InodeInner {
    desc: Dirty<InodeDesc>,
    is_freed: bool,
    weak_self: Weak<Inode>,
    fs: Weak<Ext2>,
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
    pub(super) fn block_to_path(&self, iblock: u32) -> Result<BlockPath>;
    pub(super) fn get_block(&self, iblock: u32) -> Result<Option<Bid>>;
}
```

```rust
impl Ext2 {
    pub fn block_device(&self) -> &dyn BlockDevice;
    pub fn block_size(&self) -> usize;
}
```

[NEW STRUCT]
```rust
/// Backend for inode data PageCache, translating page indices to physical blocks.
///
/// Each file/directory inode owns a PageCache backed by this struct.
/// On cache miss, it resolves logical block → physical block via the inode's
/// block pointer tree, then issues block device I/O.
///
/// Linux equivalent: address_space_operations (ext2_aops) with ext2_get_block.
/// Asterinas equivalent: ext2_old InodeBlockManager (kernel/src/fs/ext2_old/inode.rs:1855).
struct InodeDataBackend {
    /// Weak reference to the owning Inode, used to access block_ptrs via InodeInner.
    inode: Weak<Inode>,
    /// Weak reference to the filesystem for block device access.
    fs: Weak<Ext2>,
}
```

[GUARANTEE]
```rust
impl Inode {
    /// Creates a new Inode with an associated data PageCache.
    ///
    /// The PageCache is backed by `InodeDataBackend` which uses the inode's
    /// block pointer tree for logical→physical block mapping.
    /// Uses `Arc::new_cyclic` to resolve the self-referential dependency:
    /// Inode holds PageCache, PageCache backend holds Weak<Inode>.
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
impl PageCacheBackend for InodeDataBackend {
    /// Reads one data block for this inode from disk into the cache page.
    ///
    /// # Arguments
    /// * `idx` - Logical block number (0-based) within the inode's data.
    /// * `frame` - Target CachePage to fill.
    ///
    /// Translates: logical block idx → physical block via get_block(),
    /// then issues async read. For sparse holes (get_block returns None),
    /// the frame is zero-filled and no I/O is issued.
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;

    /// Writes one data block for this inode from cache page to disk.
    ///
    /// # Arguments
    /// * `idx` - Logical block number (0-based) within the inode's data.
    /// * `frame` - Source CachePage containing dirty data.
    fn write_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;

    /// Returns the number of pages (blocks) for this inode's data.
    /// Computed from inode size: `size.align_up(BLOCK_SIZE) / BLOCK_SIZE`.
    fn npages(&self) -> usize;
}
```

```rust
impl InodeInner {
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
    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize>;

    /// Writes file data through the PageCache from the provided buffer.
    ///
    /// Linux equivalent: generic_file_write_iter → ext2_write_begin.
    ///
    /// # Arguments
    /// * `offset` - Byte offset within the file to start writing.
    /// * `data` - Source data to write.
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes written.
    /// * `Err(EISDIR)` - Inode is a directory.
    /// * `Err(EIO)` - I/O failure or filesystem dropped.
    pub fn write_at(&self, offset: usize, data: &[u8]) -> Result<usize>;
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
  2. Creates `InodeDataBackend { inode: weak_self.clone(), fs: fs.clone() }`.
  3. Wraps backend in `Arc`, creates `PageCache::with_capacity(desc.num_page_bytes(), backend)`.
     - `num_page_bytes = desc.size as usize` aligned up to PAGE_SIZE.
     - For new inodes with size 0, uses `PageCache::new(backend)`.
  4. Stores `page_cache` in the Inode struct alongside existing fields.
- Returns `Arc<Inode>` with fully initialized PageCache.

Post (Inode::new: failure):
- Panics only if PageCache allocation fails (system OOM, should not happen normally).

Pre (InodeDataBackend::read_page_async):
- `frame` is a valid allocated CachePage.
- `self.inode` and `self.fs` can be upgraded.

Post (read_page_async: success):
- Upgrades `self.inode` to `Arc<Inode>`.
- Acquires read lock on `inode.inner`.
- Calls `inner.get_block(idx as u32)`:
  - If `Ok(Some(bid))`: creates BioSegment from frame with `BioDirection::FromDevice`,
    submits async read via `fs.block_device().read_blocks_async(bid, bio_segment)`.
  - If `Ok(None)`: sparse hole — zero-fills the frame, returns empty BioWaiter.
- Returns the BioWaiter.

Post (read_page_async: failure):
- `Err(EIO)` if inode or fs weak reference cannot be upgraded.
- Propagates errors from `get_block` or block device I/O.

Pre (InodeDataBackend::write_page_async):
- `frame` contains dirty data to write back.
- `self.inode` and `self.fs` can be upgraded.

Post (write_page_async: success):
- Same upgrade and lock acquisition as read path.
- Calls `inner.get_block(idx as u32)`:
  - If `Ok(Some(bid))`: creates BioSegment from frame with `BioDirection::ToDevice`,
    submits async write via `fs.block_device().write_blocks_async(bid, bio_segment)`.
  - If `Ok(None)`: no physical block allocated — returns empty BioWaiter.
    (Block allocation for write is handled separately before page writeback.)

Post (write_page_async: failure):
- `Err(EIO)` if references cannot be upgraded or block device write fails.

Pre (npages):
- `self.inode` can be upgraded.

Post (npages):
- Returns `inode.inner.read().desc.size.align_up(BLOCK_SIZE) / BLOCK_SIZE`.
- If inode reference is dead, returns 0.

Pre (read_at):
- `self` refers to a valid, non-freed `InodeInner`.
- `self.fs` can be upgraded to a live `Arc<Ext2>`.
- The owning `Inode` has an initialized `page_cache`.

Post (read_at: success):
- Rejects directories: if `self.desc.type_ == InodeType::Dir`, returns `Err(EISDIR)`.
- Obtains `file_size = self.desc.size as usize`.
- If `offset >= file_size` or `buf.is_empty()`, returns `Ok(0)`.
- Computes `read_len = min(buf.len(), file_size - offset)`.
- Reads data via PageCache:
  - `inode.page_cache().pages().read_bytes(offset, &mut buf[..read_len])`.
  - PageCache automatically triggers `read_page_async` on cache miss,
    which calls `get_block` for logical→physical mapping.
- Returns `Ok(read_len)`.

Post (read_at: failure):
- `Err(EISDIR)` if inode is a directory.
- `Err(EIO)` if `self.fs.upgrade()` fails or PageCache I/O fails.
- Propagates errors from `get_block` via PageCache backend.

Pre (write_at):
- `self` refers to a valid, non-freed `InodeInner`.
- `self.fs` can be upgraded to a live `Arc<Ext2>`.
- The owning `Inode` has an initialized `page_cache`.

Post (write_at: success):
- Rejects directories: if `self.desc.type_ == InodeType::Dir`, returns `Err(EISDIR)`.
- If `data.is_empty()`, returns `Ok(0)`.
- Computes `end = offset + data.len()`.
- If `end > current file size`, extends the PageCache via `page_cache.resize(end)`.
  (Block allocation for new blocks is a separate concern handled by alloc_block.)
- Writes data via PageCache:
  - `inode.page_cache().pages().write_bytes(offset, data)`.
  - PageCache marks affected pages as Dirty via `update_page`.
- Updates `self.desc.size = max(self.desc.size, end as u64)`.
- Marks `self.desc` dirty.
- Returns `Ok(data.len())`.

Post (write_at: failure):
- `Err(EISDIR)` if inode is a directory.
- `Err(EIO)` if `self.fs.upgrade()` fails or PageCache I/O fails.
- `Err(ENOSPC)` if block allocation fails during page cache extension.
- On partial failure, pages already written remain dirty in cache.

Invariant:
- All file/directory data I/O goes through PageCache, never direct block device access.
- PageCache backend resolves logical→physical blocks via `get_block` on every cache miss.
- Sparse holes (unallocated blocks) are zero-filled on read, consistent with POSIX semantics.
- The `page_cache` field is immutable after Inode construction; only page contents change.
- `InodeDataBackend` holds only Weak references, preventing reference cycles.

[DIFF]
Linux: File data I/O uses VFS page cache with `address_space_operations` (ext2_aops).
  `ext2_read_folio` calls `mpage_read_folio(folio, ext2_get_block)` which fills
  page cache folios via the block mapping callback.
  → Asterinas: Uses per-Inode `PageCache` with `InodeDataBackend` implementing
  `PageCacheBackend`. `read_page_async` calls `get_block` for the same mapping.
  Reason: Asterinas PageCache is the equivalent of Linux's address_space page cache.

Linux: `ext2_write_begin` calls `block_write_begin(mapping, pos, len, foliop, ext2_get_block)`
  which allocates blocks via `ext2_get_block(create=1)` and prepares the folio.
  → Asterinas: `write_at` writes through `PageCache::pages().write_bytes()`.
  Block allocation is a separate step, not integrated into the PageCache backend.
  Reason: Separation of concerns; block allocation is handled by alloc_block module.

Linux: Directory data is accessed via `ext2_get_folio` (dir.c:189) which calls
  `read_mapping_folio(mapping, n, NULL)` — same page cache as file data.
  → Asterinas: Directory data uses the same Inode PageCache. Directory operations
  (find_entry, add_entry, readdir) read/write through `page_cache.pages()`.
  Reason: Unified caching for both file and directory data, matching Linux's model.

Linux: `InodeBlockManager` in ext2_old implements `PageCacheBackend` as a separate
  struct with its own block mapping logic and indirect block cache.
  → Asterinas (new): `InodeDataBackend` is a lightweight struct that delegates
  block mapping to `InodeInner::get_block()`.
  Reason: Block mapping logic already exists in InodeInner; no need to duplicate.
