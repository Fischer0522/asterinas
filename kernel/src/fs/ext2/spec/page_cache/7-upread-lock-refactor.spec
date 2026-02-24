[PROMPT]
Refactor Ext2 inode locking from three-phase lock protocol to upread/upgrade
pattern in `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_write_begin     → fs/ext2/inode.c:928
ext2_write_end       → fs/ext2/inode.c:939
ext2_write_failed    → fs/ext2/inode.c:59
ext2_setsize         → fs/ext2/inode.c:1275
block_truncate_page  → fs/buffer.c:2654
generic_write_end    → fs/buffer.c:2300
ext2_dio_write_iter  → fs/ext2/file.c:214
generic_buffers_fsync → fs/buffer.c:646
ExfatInode::write_at → kernel/src/fs/exfat/inode.rs:685

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
/// RwMutex upgradeable read guard.
/// Provides immutable access like a read guard, but can be atomically
/// upgraded to a write guard without releasing the lock.
///
/// Key properties:
/// - Only ONE upread guard can exist at a time (prevents upgrade deadlock).
/// - Upreaders do NOT block new readers until upgrade() is called.
/// - upgrade() spins until all regular readers release, then becomes a write guard.
/// - downgrade() on a WriteGuard atomically converts back to UpgradeableGuard.
///
/// Lock state transitions:
///   upread()  → RwMutexUpgradeableGuard  (read-like, exclusive among upreaders)
///   upgrade() → RwMutexWriteGuard        (exclusive, consumes upread guard)
///   WriteGuard::downgrade() → RwMutexUpgradeableGuard (atomic, no lock gap)
impl<T> RwMutex<T> {
    pub fn upread(&self) -> RwMutexUpgradeableGuard<'_, T>;
}
impl<'a, T> RwMutexUpgradeableGuard<'a, T> {
    pub fn upgrade(self) -> RwMutexWriteGuard<'a, T>;
}
impl<'a, T> RwMutexWriteGuard<'a, T> {
    pub fn downgrade(self) -> RwMutexUpgradeableGuard<'a, T>;
}
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
    pub fn new(backend: Weak<dyn PageCacheBackend>) -> Result<Self>;
    pub fn with_capacity(capacity: usize, backend: Weak<dyn PageCacheBackend>) -> Result<Self>;
    pub fn pages(&self) -> &Arc<Vmo>;
    pub fn resize(&self, new_size: usize) -> Result<()>;
    pub fn discard_range(&self, range: Range<usize>);
    pub fn evict_range(&self, range: Range<usize>) -> Result<()>;
    pub fn fill_zeros(&self, range: Range<usize>) -> Result<()>;
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
    extension: Extension,
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
    page_cache: PageCache,
}
```

```rust
impl InodeInner {
    pub(super) fn block_to_path(&self, iblock: u32) -> Result<BlockPath>;
    pub(super) fn get_block(&self, iblock: u32) -> Result<Option<Bid>>;
    pub(super) fn get_or_alloc_block(&mut self, iblock: u32, create: bool) -> Result<Option<Bid>>;
    pub(super) fn truncate_blocks(&mut self, new_size: usize) -> Result<()>;
    pub(super) fn persist_inode_and_sync(&mut self, fs: &Ext2) -> Result<()>;
    /// Flushes dirty data pages via evict_range. Takes &self (read-compatible).
    pub(super) fn sync_data(&self) -> Result<()>;
    /// Direct-I/O write: bypasses PageCache, writes directly to block device.
    pub(super) fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize>;
    /// Direct-I/O read: bypasses PageCache, reads directly from block device.
    pub(super) fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize>;
}
```

```rust
impl Ext2 {
    pub fn block_device(&self) -> &dyn BlockDevice;
    pub fn block_size(&self) -> usize;
    pub fn super_block(&self) -> &SuperBlock;
}
```

[GUARANTEE]
```rust
impl Inode {
    /// Writes file data through the PageCache from the provided reader.
    ///
    /// Linux equivalent: generic_file_write_iter → ext2_write_begin/end.
    ///
    /// Uses a two-phase upread lock protocol (replaces three-phase):
    /// - Phase 1 (write lock): pre-allocate blocks, resize PageCache if extending.
    /// - Phase 2 (upread → upgrade): write data through PageCache under upread,
    ///   then atomically upgrade to write lock for metadata update.
    ///
    /// # Lock protocol
    /// Phase 1: `inner.write()` — block allocation + resize (no PageCache I/O callbacks).
    /// Phase 2: `inner.upread()` — PageCache data write (callbacks acquire inner read,
    ///          compatible with upread). Then `upgrade()` — metadata update + persist.
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

    /// Direct-I/O write path with pre-allocation and rollback.
    ///
    /// Linux: /root/linux/fs/ext2/file.c:214 (ext2_dio_write_iter)
    ///
    /// Uses the same two-phase upread protocol as write_at:
    /// - Phase 1 (write lock): pre-allocate blocks, resize, discard cached pages.
    /// - Phase 2 (upread → upgrade): direct I/O write under upread,
    ///   then upgrade for metadata update.
    ///
    /// # Lock protocol
    /// Phase 1: `inner.write()` — block allocation + discard (no I/O callbacks).
    /// Phase 2: `inner.upread()` — direct I/O write (callbacks acquire inner read,
    ///          compatible with upread). Then `upgrade()` — metadata + persist.
    pub(super) fn write_direct_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize>;

    /// Resizes this inode to `new_size` bytes.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1275 (ext2_setsize)
    ///
    /// Uses upread/upgrade for shrink path (replaces read → write):
    /// - Shrink: upread → tail zeroing via fill_zeros → upgrade → discard + truncate.
    /// - Grow: write lock only (no PageCache I/O callbacks).
    ///
    /// # Lock protocol (shrink)
    /// `inner.upread()` — fill_zeros for tail block (callbacks acquire inner read,
    /// compatible with upread). Then `upgrade()` — discard_range + resize +
    /// truncate_blocks + metadata update.
    ///
    /// # Lock protocol (grow)
    /// `inner.write()` — resize PageCache (grow has no callbacks) + metadata update.
    pub(super) fn resize(&self, new_size: usize) -> Result<()>;

    /// Syncs all dirty data and metadata to disk.
    ///
    /// Linux: /root/linux/fs/buffer.c:646 (generic_buffers_fsync)
    ///
    /// Uses upread/upgrade (replaces read → write):
    /// - upread: flush dirty data pages (evict_range triggers write_page_async,
    ///   which acquires inner read — compatible with upread).
    /// - upgrade: persist inode metadata.
    pub(super) fn sync_all(&self) -> Result<()>;
}
```

[SPECIFICATION]

## Motivation

The current three-phase lock protocol (write → drop → read → drop → write) in
`write_at`, `write_direct_at`, `resize`, and `sync_all` is correct but fragile:
- Three separate lock acquisitions per write path create TOCTOU windows.
- Each phase boundary is a potential source of bugs during future refactoring.
- The pattern is non-obvious to new contributors.

The `upread/upgrade` mechanism (already used by `add_entry`, `delete_entry`,
and exfat's `write_at`) provides an equivalent deadlock-free protocol with
fewer lock transitions and atomic phase advancement.

## Key Insight: upread Compatibility with PageCache Callbacks

PageCacheBackend callbacks (`read_page_async`, `write_page_async`) acquire
`inner` READ lock. The RwMutex upread guard is read-compatible: regular readers
can coexist with an upread holder. Therefore:

- Holding `inner.upread()` while performing PageCache I/O is safe.
- Callbacks that acquire `inner.read()` will succeed (no contention with upread).
- After I/O completes, `upgrade()` atomically transitions to write lock.
- No lock gap between the I/O phase and the metadata-update phase.

This is the same reasoning that makes `add_entry`'s upread pattern safe:
`scan_dir_for_slot` does PageCache reads under upread, then `upgrade()` for
metadata commit.

## Scope of Changes

Functions to refactor (all in `impl Inode`):

| Function          | Before                              | After                                |
|-------------------|-------------------------------------|--------------------------------------|
| `write_at`        | write → drop → read → drop → write | write → drop → upread → upgrade      |
| `write_direct_at` | write → drop → read → drop → write | write → drop → upread → upgrade      |
| `resize` (shrink) | read → drop → read → drop → write  | read → drop → upread → upgrade       |
| `sync_all`        | read → drop → write                | upread → upgrade                     |

Functions NOT changed (no benefit from upread):
- `read_at`: single read lock, already optimal.
- `resize` (grow): single write lock, no PageCache I/O callbacks.
- `fallocate`: delegates to `resize` or uses single read lock.
- `sync_data`: single write lock, already optimal.
- `prepare_for_evict`: single write lock path.

## Inode::write_at

Pre:
- `self` is a valid, non-freed Inode.

Post (success):
- Rejects directories: if `self.type_ == InodeType::Dir`, returns `Err(EISDIR)`.
- If `reader.remain() == 0`, returns `Ok(0)`.
- Computes `write_len = reader.remain()`, `end = offset + write_len`.

- Phase 1 — pre-allocate blocks and extend (write lock):
  - Acquires `self.inner.write()`.
  - Obtains `old_size = desc.size as usize`, `block_size = fs.block_size()`.
  - For each logical block in `offset / block_size .. end.div_ceil(block_size)`:
    calls `inner.get_or_alloc_block(iblock, true)` to ensure physical block exists.
  - If `end > old_size`:
    - Calls `inner.page_cache.resize(end.align_up(block_size))` to extend.
      (Grow path: no callbacks — safe under write lock.)
    - Updates `inner.desc.size = end as u64`.
  - Releases write lock (drop).
  - On alloc failure: calls `write_failed_cleanup`, returns error.

- Phase 2 — write data + update metadata (upread → upgrade):
  - Acquires `self.inner.upread()`.
  - Calls `inner.page_cache.pages().write(offset, reader)`.
    - PageCache may trigger `read_page_async` on cache miss →
      acquires `inner.read()` → compatible with upread holder → no deadlock.
  - On I/O failure: drops upread guard, acquires write lock for cleanup,
    calls `write_failed_cleanup`, returns error.
  - On success: calls `inner.upgrade()` — atomically becomes write lock.
    - No lock gap: no concurrent writer can modify metadata between
      data write and timestamp update.
  - Updates `desc.mtime` and `desc.ctime` to `now()`.
  - Calls `inner.persist_inode_and_sync(&fs)`.
  - Returns `Ok(write_len)`.

Post (failure):
- `Err(EISDIR)` if inode is a directory.
- `Err(EIO)` if fs dropped or PageCache I/O fails.
- `Err(ENOSPC)` if block allocation fails.
- On allocation failure: `write_failed_cleanup` rolls back to original state.
- On I/O failure in Phase 2: `write_failed_cleanup` rolls back if file was extended.

## Inode::write_direct_at

Pre:
- `self` is a valid, non-freed Inode.

Post (success):
- Rejects directories: if `self.type_ == InodeType::Dir`, returns `Err(EISDIR)`.
- Validates block alignment of `offset` and `reader.remain()`.
- If `reader.remain() == 0`, returns `Ok(0)`.
- Computes `write_len = reader.remain()`, `end = offset + write_len`.

- Phase 1 — pre-allocate blocks, resize, discard cached pages (write lock):
  - Acquires `self.inner.write()`.
  - Saves `old_size = desc.size as usize`.
  - For each logical block in `offset / block_size .. end.div_ceil(block_size)`:
    calls `inner.get_or_alloc_block(iblock, true)`.
  - If `end > old_size`:
    - Calls `inner.page_cache.resize(end.align_up(block_size))`.
    - Updates `inner.desc.size = end as u64`.
  - Discards cached pages in the write range:
    `inner.page_cache.discard_range(offset.min(old_size)..end.min(old_size))`.
    (discard_range has no callbacks — safe under write lock.)
  - Releases write lock (drop).
  - On alloc failure: calls `write_failed_cleanup`, returns error.

- Phase 2 — direct I/O write + update metadata (upread → upgrade):
  - Acquires `self.inner.upread()`.
  - Calls `inner.write_at(offset, reader)` (InodeInner direct-I/O write).
    - Direct I/O may trigger `read_page_async`/`write_page_async` callbacks →
      acquires `inner.read()` → compatible with upread → no deadlock.
  - On I/O failure: drops upread guard, acquires write lock for cleanup,
    calls `write_failed_cleanup`, returns error.
  - On success: calls `inner.upgrade()`.
  - Updates `desc.mtime` and `desc.ctime` to `now()`.
  - Calls `inner.persist_inode_and_sync(&fs)`.
  - Returns `Ok(write_len)`.

Post (failure):
- `Err(EISDIR)` if inode is a directory.
- `Err(EINVAL)` if offset or length not block-aligned.
- `Err(EIO)` if fs dropped or I/O fails.
- `Err(ENOSPC)` if block allocation fails.
- On failure: `write_failed_cleanup` rolls back if file was extended.

## Inode::resize

Pre:
- `self` is a valid, non-freed Inode.
- VFS layer guarantees no concurrent `write_at` during `resize`
  (Linux: i_rwsem held exclusively by setattr path).

Post (success):
- Acquires inner read lock for pre-checks:
  - Rejects non-regular/dir/symlink types: returns `Err(EINVAL)`.
  - Rejects fast symlinks (is_fast_symlink && size != 0): returns `Err(EINVAL)`.
  - Rejects APPEND_ONLY/IMMUTABLE flags: returns `Err(EPERM)`.
  - Obtains `old_size = desc.size as usize`.
  - If `new_size == old_size`, returns `Ok(())`.
  - Releases read lock.

- If shrinking (`new_size < old_size`) — upread → upgrade:

  - Acquires `self.inner.upread()`.
  - Step 1 — tail block zeroing (under upread):
    - Linux: block_truncate_page (fs/buffer.c:2654).
    - If `new_size % block_size != 0`:
      - `zero_from = new_size`, `zero_to = new_size.align_up(block_size)`.
      - Calls `inner.page_cache.fill_zeros(zero_from..zero_to)`.
        - May trigger `read_page_async` → acquires `inner.read()` →
          compatible with upread → no deadlock.
  - Step 2 — truncate (upgrade to write lock):
    - Calls `inner.upgrade()` — atomically becomes write lock.
    - Re-reads `old_size = desc.size` (defensive TOCTOU guard).
    - If `new_size == old_size` after re-read, releases lock, returns `Ok(())`.
    - `old_size_aligned = old_size.align_up(block_size)`.
    - `new_size_aligned = new_size.align_up(block_size)`.
    - If `new_size_aligned < old_size_aligned`:
      - `inner.page_cache.discard_range(new_size_aligned..old_size_aligned)`.
        (No callbacks — safe under write lock.)
    - `inner.page_cache.resize(new_size_aligned)`.
      (Pages already discarded → no callbacks.)
    - `inner.desc.size = new_size as u64`.
    - `inner.truncate_blocks(new_size)`.
    - Updates `desc.mtime` and `desc.ctime` to `now()`.
    - Calls `inner.persist_inode_and_sync(&fs)`.

- If growing (`new_size > old_size`) — write lock only (unchanged):
  - Acquires `self.inner.write()`.
  - `inner.page_cache.resize(new_size.align_up(block_size))`.
    (Grow: no callbacks — safe under write lock.)
  - `inner.desc.size = new_size as u64`.
  - Updates `desc.mtime` and `desc.ctime` to `now()`.
  - Calls `inner.persist_inode_and_sync(&fs)`.

Post (failure):
- `Err(EINVAL)` for invalid inode type or fast symlink.
- `Err(EPERM)` for immutable/append-only.
- `Err(EIO)` for I/O failures during tail zeroing, truncate, or persist.

## Inode::sync_all

Pre:
- `self` is a valid Inode.

Post (success):
- Acquires `self.inner.upread()`.
- Step 1 — flush dirty data pages (under upread):
  - Calls `inner.sync_data()`.
    - `sync_data` calls `page_cache.evict_range()` which triggers
      `write_page_async` callbacks → acquires `inner.read()` →
      compatible with upread → no deadlock.
- Step 2 — persist inode metadata (upgrade):
  - Calls `inner.upgrade()` — atomically becomes write lock.
  - Calls `inner.persist_inode_and_sync(&fs)`.
- Step 3 — flush device write cache (no lock):
  - Releases write lock.
  - Calls `fs.block_device().sync()`.

Post (failure):
- `Err(EIO)` if data writeback, metadata persist, or device sync fails.

## Deadlock Analysis (Updated)

Lock hierarchy (unchanged):
```
Level 0: Inode (Arc, no lock)
Level 1: inner: RwMutex<InodeInner>  (read, upread, or write)
Level 2: PageCacheManager::pages: Mutex<LruCache>
Level 3: PageCacheManager::ra_state: Mutex<ReadaheadState>
```

PageCache callback lock requirements (unchanged):
- `read_page_async`: acquires inner READ lock (Level 1).
- `write_page_async`: acquires inner READ lock (Level 1).

Upread compatibility rule:
- RwMutex upread is read-compatible: `inner.read()` succeeds while upread is held.
- RwMutex upread is NOT write-compatible: `inner.write()` blocks while upread is held.
- Only ONE upread can exist at a time (prevents upgrade deadlock).
- `upgrade()` blocks new readers (sets BEING_UPGRADED) and spins until
  existing readers release.

Safe paths (updated with upread):
| Operation                    | Held lock      | PageCache op     | Callback needs     | Safe? |
|------------------------------|----------------|------------------|--------------------|-------|
| read_at                      | inner READ     | pages().read     | read_page → READ   | YES (reentrant) |
| write_at Phase 1 alloc       | inner WRITE    | resize (grow)    | no callbacks       | YES |
| write_at Phase 2 data        | inner UPREAD   | pages().write    | read_page → READ   | YES (upread+read compatible) |
| write_at Phase 2 metadata    | inner WRITE *  | none             | none               | YES |
| write_direct_at Phase 1      | inner WRITE    | discard_range    | no callbacks       | YES |
| write_direct_at Phase 2 I/O  | inner UPREAD   | direct write     | read_page → READ   | YES (upread+read compatible) |
| write_direct_at Phase 2 meta | inner WRITE *  | none             | none               | YES |
| resize shrink fill_zeros     | inner UPREAD   | fill_zeros       | read_page → READ   | YES (upread+read compatible) |
| resize shrink truncate       | inner WRITE *  | discard_range    | no callbacks       | YES |
| resize shrink truncate       | inner WRITE *  | resize (shrink)  | no callbacks (**)  | YES |
| resize grow                  | inner WRITE    | resize (grow)    | no callbacks       | YES |
| sync_all data flush          | inner UPREAD   | evict_range      | write_page → READ  | YES (upread+read compatible) |
| sync_all metadata            | inner WRITE *  | none             | none               | YES |

(*) Obtained via upgrade() from upread — no lock gap.
(**) Safe because discard_range removes all pages before resize is called.

Dangerous paths (still prevented):
- inner WRITE + PageCache fill_zeros → read_page → inner READ → DEADLOCK.
  Prevention: resize shrink does fill_zeros under UPREAD (not WRITE).
  fill_zeros callback acquires READ, compatible with UPREAD.
- inner WRITE + PageCache evict_range → write_page → inner READ → DEADLOCK.
  Prevention: sync_all does evict under UPREAD (not WRITE).
  evict callback acquires READ, compatible with UPREAD.

Invariants (updated):
- All file data I/O goes through PageCache, never direct block device access.
- PageCacheBackend is implemented on Inode (outer struct), not InodeInner.
- read_page_async/write_page_async acquire inner read lock only.
- write_at/write_direct_at MUST release write lock before PageCache data path.
- write_at/write_direct_at use upread for data I/O, then upgrade for metadata.
- resize shrink MUST do tail zeroing under upread (not write lock).
- resize shrink MUST upgrade before discard_range + truncate.
- sync_all MUST flush data under upread, then upgrade for metadata persist.
- write_page_async expects blocks to be pre-allocated; None mapping is a bug.
- PageCache capacity tracks file size (block-aligned) on resize/truncate/write-extend.

[DIFF]
This spec is a pure refactoring of the lock protocol. No Linux logic changes.
All on-disk behavior, block allocation order, error handling, and cleanup
semantics remain identical to spec `2-inode-data-page-cache.spec`.

write_at lock protocol change:
  Before: write() → drop → read() → pages().write() → drop → write() → persist
  After:  write() → drop → upread() → pages().write() → upgrade() → persist
  Benefit: Eliminates one lock gap (read→write). The upgrade() from upread to
  write is atomic — no window where another writer can interleave between
  data write and metadata update. Matches ExfatInode::write_at pattern
  (exfat/inode.rs:685-718).

write_direct_at lock protocol change:
  Before: write() → drop → read() → direct_write() → drop → write() → persist
  After:  write() → drop → upread() → direct_write() → upgrade() → persist
  Benefit: Same as write_at — eliminates read→write gap.

resize (shrink) lock protocol change:
  Before: read() → pre-check → drop → read() → fill_zeros → drop → write() → truncate
  After:  read() → pre-check → drop → upread() → fill_zeros → upgrade() → truncate
  Benefit: Eliminates the read→write gap between tail zeroing and truncation.
  The upgrade() ensures no concurrent modification between fill_zeros and
  discard_range + truncate_blocks.

sync_all lock protocol change:
  Before: read() → sync_data → drop → write() → persist → drop → device sync
  After:  upread() → sync_data → upgrade() → persist → drop → device sync
  Benefit: Eliminates the read→write gap between data flush and metadata persist.
  Ensures no concurrent writer can dirty metadata between flush and persist.

[TEST]
## Inode::write_at (upread refactor)
- Write within existing file → data written, timestamps updated via upgrade
- Write extending file → Phase 1 allocs blocks, Phase 2 upread writes data, upgrade persists
- Write to directory → Err(EISDIR)
- Write empty reader → Ok(0)
- Block allocation fails → write_failed_cleanup rolls back under write lock
- PageCache I/O fails in Phase 2 → upread dropped, write lock acquired for cleanup
- Partial block write → read_page_async called under upread (compatible), no deadlock
- Concurrent reads during Phase 2 → succeed (upread does not block readers)

## Inode::write_direct_at (upread refactor)
- Direct write within file → data written bypassing PageCache, timestamps updated via upgrade
- Direct write extending file → Phase 1 allocs + discards cache, Phase 2 upread writes, upgrade persists
- Non-block-aligned offset or length → Err(EINVAL)
- Direct write empty reader → Ok(0)
- Block allocation fails → write_failed_cleanup rolls back
- Direct I/O fails in Phase 2 → upread dropped, write lock acquired for cleanup

## Inode::resize (upread refactor)
- Shrink with partial tail block → fill_zeros under upread, upgrade for truncate
- Shrink to block-aligned size → no fill_zeros needed, upread → upgrade → truncate
- Shrink: fill_zeros triggers read_page_async under upread → no deadlock
- Shrink: discard_range under write lock (post-upgrade) → no callbacks, safe
- Grow → write lock only path, unchanged behavior
- Resize to same size → Ok(()), no-op
- Resize non-regular/dir/symlink → Err(EINVAL)
- Resize fast symlink → Err(EINVAL)
- Resize immutable file → Err(EPERM)

## Inode::sync_all (upread refactor)
- Sync with dirty data pages → evict_range under upread triggers write_page_async (acquires read, compatible), then upgrade for metadata persist
- Sync with clean data → evict_range is no-op, upgrade for metadata persist
- Sync with dirty metadata → upgrade persists inode descriptor
- Device sync failure → Err(EIO) propagated after lock released
