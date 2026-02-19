[PROMPT]
Provide modifications to `kernel/src/fs/ext2/inode.rs` and
`kernel/src/fs/ext2/impl_for_vfs/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_dio_read_iter   → fs/ext2/file.c:168
ext2_dio_write_iter  → fs/ext2/file.c:214
ext2_dio_write_end_io → fs/ext2/file.c:183
ext2_write_failed    → fs/ext2/inode.c:59
ext2_file_read_iter  → fs/ext2/file.c:283
ext2_file_write_iter → fs/ext2/file.c:295
ExfatInode::read_direct_at  → kernel/src/fs/exfat/inode.rs:635
ExfatInode::write_direct_at → kernel/src/fs/exfat/inode.rs:734
Ext2Inode(old)::read_direct_at  → kernel/src/fs/ext2_old/inode.rs:713
Ext2Inode(old)::write_direct_at → kernel/src/fs/ext2_old/inode.rs:754

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
/// RwMutex with upgradeable read locks.
impl<T> RwMutex<T> {
    pub fn read(&self) -> RwMutexReadGuard<'_, T>;
    pub fn write(&self) -> RwMutexWriteGuard<'_, T>;
    pub fn upread(&self) -> RwMutexUpgradeableGuard<'_, T>;
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
    pub(super) fn get_block(&self, iblock: u32) -> Result<Option<Bid>>;
    pub(super) fn get_or_alloc_block(&mut self, iblock: u32, create: bool) -> Result<Option<Bid>>;
    pub(super) fn truncate_blocks(&mut self, new_size: usize) -> Result<()>;
    pub(super) fn persist_inode_and_sync(&self, fs: &Ext2) -> Result<()>;

    /// Direct block-device read: iterates logical blocks, reads via BioSegment.
    /// Replaces old `read_at(&self, offset, &mut [u8])` with VmWriter interface.
    /// Caller must hold at least read lock. Does NOT touch PageCache.
    pub fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize>;

    /// Direct block-device write: iterates logical blocks, writes via BioSegment.
    /// Replaces old `write_at(&mut self, offset, &[u8])` with VmReader interface.
    /// Blocks MUST be pre-allocated by caller. Does NOT touch PageCache.
    /// Does NOT update desc.size/timestamps (caller's responsibility).
    pub fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize>;
}
```

```rust
/// Existing buffered I/O methods (from spec 2-inode-data-page-cache).
impl Inode {
    /// Buffered read via PageCache. Lock: inner read lock.
    pub(super) fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize>;
    /// Buffered write via PageCache. Lock: write → release → page cache → write.
    pub(super) fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize>;
    /// Cleanup on write failure: discard_range + truncate_blocks.
    fn write_failed_cleanup(inner: &mut InodeInner, old_size: usize, end: usize, block_size: usize);
    /// Resize (truncate/extend).
    pub(super) fn resize(&self, new_size: usize) -> Result<()>;
}
```

```rust
/// VFS traits.
pub trait InodeIo {
    fn read_at(&self, offset: usize, writer: &mut VmWriter, status_flags: StatusFlags) -> Result<usize>;
    fn write_at(&self, offset: usize, reader: &mut VmReader, status_flags: StatusFlags) -> Result<usize>;
}

pub trait Inode: Any + InodeIo + Send + Sync {
    fn page_cache(&self) -> Option<Arc<Vmo>> { None }
    // ... other methods
}
```

```rust
/// Block device I/O primitives.
impl dyn BlockDevice {
    fn read_blocks_async(&self, bid: BlockId, segment: BioSegment) -> Result<BioWaiter>;
    fn write_blocks_async(&self, bid: BlockId, segment: BioSegment) -> Result<BioWaiter>;
    fn read_blocks(&self, bid: BlockId, segment: BioSegment) -> Result<()>;
    fn write_blocks(&self, bid: BlockId, segment: BioSegment) -> Result<()>;
}
```

[GUARANTEE]
```rust
impl InodeInner {
    /// Reads file data directly from block device via BioSegment.
    ///
    /// Replaces old `read_at(&self, offset: usize, buf: &mut [u8])`.
    /// Iterates logical blocks, resolves via get_block, reads via BioSegment.
    /// Sparse holes return zeros. Does NOT touch PageCache.
    ///
    /// # Lock
    /// Caller must hold at least read lock on InodeInner.
    pub fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize>;

    /// Writes file data directly to block device via BioSegment.
    ///
    /// Replaces old `write_at(&mut self, offset: usize, data: &[u8])`.
    /// Iterates logical blocks, resolves via get_block, writes via BioSegment.
    /// Blocks MUST be pre-allocated. Does NOT update size/timestamps.
    /// Does NOT touch PageCache.
    ///
    /// # Lock
    /// Caller must hold at least read lock on InodeInner.
    /// (Only needs &self since blocks are pre-allocated.)
    pub fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize>;
}
```

```rust
impl Inode {
    /// Direct I/O read: alignment check + discard_range + delegate to InodeInner::read_at.
    ///
    /// Linux: fs/ext2/file.c:168 (ext2_dio_read_iter)
    ///
    /// # Lock
    /// Acquires inner read lock. discard_range has no callbacks — safe.
    pub(super) fn read_direct_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize>;

    /// Direct I/O write: alloc blocks + discard_range + delegate to InodeInner::write_at.
    ///
    /// Linux: fs/ext2/file.c:214 (ext2_dio_write_iter)
    ///
    /// # Lock
    /// Phase 1 (write lock): alloc blocks, extend size, discard overlapping pages.
    /// Phase 2 (read lock): delegate to InodeInner::write_at for block device I/O.
    /// Phase 3 (write lock): update timestamps, persist.
    pub(super) fn write_direct_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize>;
}
```

```rust
/// VFS InodeIo dispatch: O_DIRECT vs buffered.
impl InodeIo for Inode {
    fn read_at(
        &self,
        offset: usize,
        writer: &mut VmWriter,
        status_flags: StatusFlags,
    ) -> Result<usize>;

    fn write_at(
        &self,
        offset: usize,
        reader: &mut VmReader,
        status_flags: StatusFlags,
    ) -> Result<usize>;
}
```

```rust
/// VFS Inode: expose PageCache for mmap.
impl VfsInode for Inode {
    fn page_cache(&self) -> Option<Arc<Vmo>>;
}
```

[SPECIFICATION]

## InodeInner::read_at (direct block device read)

Replaces old `read_at(&self, offset: usize, buf: &mut [u8])`.

Pre:
- Caller holds at least read lock on InodeInner.

Post (success):
- Obtains `file_size = desc.size as usize`, `block_size` from fs.
- Clamps read range: `read_len = min(writer.avail(), file_size - offset)`. If 0, returns `Ok(0)`.
- Iterates logical blocks covering `offset..offset+read_len`:
  - `get_block(iblock)` → physical Bid.
  - If `Some(bid)`: allocates BioSegment, `fs.block_device().read_blocks(bid, segment)`,
    copies to writer. Handles partial first/last block via sub-block offset.
  - If `None` (sparse hole): writes zeros to writer for the block portion.
- Returns `Ok(read_len)`.

Post (failure):
- `Err(EISDIR)` if directory.
- `Err(EIO)` if fs dropped or block device read fails.

## InodeInner::write_at (direct block device write)

Replaces old `write_at(&mut self, offset: usize, data: &[u8])`.

Pre:
- Caller holds at least read lock on InodeInner.
- All target blocks MUST be pre-allocated by caller.

Post (success):
- Obtains `block_size` from fs.
- Iterates logical blocks covering `offset..offset+reader.remain()`:
  - `get_block(iblock)` → physical Bid (must exist).
  - Partial block: read-modify-write (read full block, patch, write back).
  - Full block: allocates BioSegment, copies from reader, writes via block device.
- Returns `Ok(bytes_written)`.
- Does NOT update `desc.size` or timestamps — caller's responsibility.

Post (failure):
- `Err(EIO)` if block mapping missing or block device I/O fails.

## Inode::read_direct_at (thin wrapper)

Pre:
- `self.type_ != InodeType::Dir`.
- `offset` is block-aligned.
- `writer.avail()` is block-aligned.

Post (success):
- Rejects directories: `Err(EISDIR)`.
- Validates alignment: `Err(EINVAL)` if not block-aligned.
- Acquires inner read lock.
- Computes read range clamped to file size (block-aligned).
- `inner.page_cache.discard_range(start..end)` — invalidate stale cache.
  - No callbacks — safe under read lock.
- Delegates to `inner.read_at(start, writer)`.
- Updates `atime`.
- Returns `Ok(read_len)`.

Post (failure):
- `Err(EISDIR)`, `Err(EINVAL)`, `Err(EIO)`.

## Inode::write_direct_at (thin wrapper)

Pre:
- `self.type_ != InodeType::Dir`.
- `offset` is block-aligned.
- `reader.remain()` is block-aligned.

Post (success):
- Rejects directories: `Err(EISDIR)`.
- Validates alignment: `Err(EINVAL)` if not block-aligned.
- Computes `write_len = reader.remain()`, `end = offset + write_len`.

- Phase 1 (write lock): alloc blocks, extend, discard overlap.
  - Acquires inner write lock.
  - `old_size = desc.size as usize`.
  - Allocates blocks via `get_or_alloc_block` for each logical block.
  - If `end > old_size`: `page_cache.resize`, update `desc.size`.
  - Discards overlapping cached pages: `discard_range(min(offset,old_size)..min(end,old_size))`.
  - On failure: `write_failed_cleanup`, return error.
  - Releases write lock.

- Phase 2 (read lock): delegate to `inner.write_at(offset, reader)`.
  - Acquires inner read lock.
  - Blocks pre-allocated in Phase 1 — `InodeInner::write_at` only does I/O.
  - Releases read lock.

- Phase 3 (write lock): timestamps + persist.
  - Acquires inner write lock.
  - Updates `desc.mtime`, `desc.ctime`.
  - Calls `persist_inode_and_sync`.

- Returns `Ok(write_len)`.

Post (failure):
- `Err(EISDIR)`, `Err(EINVAL)`, `Err(ENOSPC)`, `Err(EIO)`.

## InodeIo dispatch

Post (InodeIo::read_at):
- If `status_flags.contains(StatusFlags::O_DIRECT)`:
  - Calls `Inode::read_direct_at(self, offset, writer)`.
  - Linux: `ext2_file_read_iter` (file.c:289) dispatches to `ext2_dio_read_iter`.
- Else:
  - Calls `Inode::read_at(self, offset, writer)`.
  - Linux: `ext2_file_read_iter` (file.c:292) dispatches to `generic_file_read_iter`.

Post (InodeIo::write_at):
- If `status_flags.contains(StatusFlags::O_DIRECT)`:
  - Calls `Inode::write_direct_at(self, offset, reader)`.
  - Linux: `ext2_file_write_iter` (file.c:301) dispatches to `ext2_dio_write_iter`.
- Else:
  - Calls `Inode::write_at(self, offset, reader)`.
  - Linux: `ext2_file_write_iter` (file.c:303) dispatches to `generic_perform_write`.

## VfsInode::page_cache

Post:
- Returns `Some(inner.page_cache.pages().clone())`.
- Acquires inner read lock to access page_cache.
- Linux: `inode->i_mapping` is always available for regular files.
- Enables mmap support via VFS layer.

## Deadlock Analysis

| Operation                  | Lock held     | I/O path            | Callback needs    | Safe? |
|----------------------------|---------------|---------------------|-------------------|-------|
| read_direct_at             | read          | discard_range       | no callbacks      | YES |
| read_direct_at             | read          | block_device.read   | none              | YES |
| write_direct_at Phase 1    | write         | resize (grow)       | no callbacks      | YES |
| write_direct_at Phase 1    | write         | discard_range       | no callbacks      | YES |
| write_direct_at Phase 2    | read          | block_device.write  | none              | YES |
| write_direct_at Phase 3    | write         | persist_inode       | none              | YES |
| buffered read_at           | read          | pages().read        | read_page → read  | YES (reentrant) |
| buffered write_at Phase 2  | NONE          | pages().write       | read_page → read  | YES |

[DIFF]
Linux: `ext2_file_read_iter` (file.c:283) checks `IOCB_DIRECT` flag and dispatches
  to `ext2_dio_read_iter` (iomap-based DIO) or `generic_file_read_iter` (buffered).
  → Asterinas: `InodeIo::read_at` checks `StatusFlags::O_DIRECT` and dispatches to
  `read_direct_at` (block device direct) or `read_at` (PageCache buffered).
  Reason: Same dispatch pattern. Asterinas uses per-block I/O instead of iomap
  because ext2 block mapping is simple (no extent trees).

Linux: `ext2_dio_read_iter` (file.c:168) acquires `inode_lock_shared`, calls
  `iomap_dio_rw` which invalidates page cache and does direct I/O.
  → Asterinas: `read_direct_at` acquires inner read lock, calls `discard_range`
  to invalidate overlapping pages, then reads blocks directly.
  Reason: `discard_range` matches Linux's page cache invalidation in iomap_dio_rw.
  exFAT and ext2_old use the same pattern.

Linux: `ext2_dio_write_iter` (file.c:214) acquires `inode_lock` (exclusive),
  calls `iomap_dio_rw` with `ext2_dio_write_ops`. On extending write,
  `ext2_dio_write_end_io` updates i_size. Falls back to buffered for partial writes.
  → Asterinas: `write_direct_at` uses three-phase protocol:
  Phase 1 (write lock): alloc blocks, extend size, discard overlapping pages.
  Phase 2 (read lock): direct block device writes.
  Phase 3 (write lock): timestamps, persist.
  No fallback to buffered (simplification — aligned DIO only).
  Reason: Three-phase matches buffered write_at structure. Size update in Phase 1
  (before data write) matches `ext2_dio_write_end_io` updating i_size before
  page cache invalidation.

Linux: Page cache invalidation for DIO happens inside `iomap_dio_rw` via
  `kiocb_invalidate_pages` / `invalidate_inode_pages2_range`.
  → Asterinas: `discard_range` before direct I/O. For reads, discards the read
  range. For writes, discards the overlap with existing file data.
  Reason: Ensures no stale cached data after direct I/O. discard (not evict)
  because we don't need to write back — DIO data goes directly to/from disk.

VFS page_cache():
  Linux: `inode->i_mapping` always exists, used by mmap.
  → Asterinas: `page_cache()` returns `Some(Arc<Vmo>)` for regular files.
  Previously returned `None` (page cache not yet integrated).

[TEST]
## Inode::read_direct_at
- Read aligned range within file → correct data, bypasses PageCache
- Read at offset beyond EOF → Ok(0)
- Read range clamped to file size boundary
- Sparse hole block → zeros returned
- Non-aligned offset → Err(EINVAL)
- Non-aligned length → Err(EINVAL)
- Directory inode → Err(EISDIR)
- Verify discard_range called before direct read (stale cache invalidated)

## Inode::write_direct_at
- Write aligned range within file → data written directly to disk
- Write extending file → size updated, blocks allocated
- Non-aligned offset → Err(EINVAL)
- Non-aligned length → Err(EINVAL)
- Directory inode → Err(EISDIR)
- Block allocation fails → Err(ENOSPC), rollback via write_failed_cleanup
- Verify discard_range called for overlap with existing data
- Verify PageCache does NOT contain written data after direct write

## InodeIo dispatch
- O_DIRECT read → dispatches to read_direct_at
- O_DIRECT write → dispatches to write_direct_at
- Non-O_DIRECT read → dispatches to buffered read_at
- Non-O_DIRECT write → dispatches to buffered write_at

## VfsInode::page_cache
- Regular file → returns Some(Arc<Vmo>)
- Returned Vmo is the same object as used by buffered read/write
