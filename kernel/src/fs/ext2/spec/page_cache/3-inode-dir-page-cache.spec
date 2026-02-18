[PROMPT]
Provide modifications to `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_get_folio       → fs/ext2/dir.c:189
ext2_find_entry      → fs/ext2/dir.c:342
ext2_readdir         → fs/ext2/dir.c:257
ext2_add_link        → fs/ext2/dir.c:476
ext2_delete_entry    → fs/ext2/dir.c:560
ext2_make_empty      → fs/ext2/dir.c:617
ext2_empty_dir       → fs/ext2/dir.c:659
ext2_set_link        → fs/ext2/dir.c:450
ext2_commit_chunk    → fs/ext2/dir.c:84

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
    pub fn new(backend: Weak<dyn PageCacheBackend>) -> Result<Self>;
    pub fn with_capacity(capacity: usize, backend: Weak<dyn PageCacheBackend>) -> Result<Self>;
    pub fn pages(&self) -> &Arc<Vmo>;
    pub fn resize(&self, new_size: usize) -> Result<()>;
    pub fn discard_range(&self, range: Range<usize>);
    pub fn evict_range(&self, range: Range<usize>) -> Result<()>;
}
```

```rust
/// RwMutex supports upgradeable read locks (exFAT pattern).
/// - upread() is compatible with read() (no mutual exclusion).
/// - upread() is exclusive with other upread() and write().
/// - upread can be atomically upgraded to write via upgrade().
/// - write can be atomically downgraded to upread via downgrade().
impl<T> RwMutex<T> {
    pub fn read(&self) -> RwMutexReadGuard<'_, T>;
    pub fn write(&self) -> RwMutexWriteGuard<'_, T>;
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
/// The Ext2 inode public handle.
/// PageCacheBackend is implemented directly on Inode (exFAT pattern).
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
    pub(super) fn get_block(&self, iblock: u32) -> Result<Option<Bid>>;
    pub(super) fn get_or_alloc_block(&mut self, iblock: u32, create: bool) -> Result<Option<Bid>>;
    pub(super) fn persist_inode_and_sync(&self, fs: &Ext2) -> Result<()>;
}
```

```rust
impl Ext2 {
    pub fn block_size(&self) -> usize;
    pub fn super_block(&self) -> &SuperBlock;
    pub fn alloc_blocks(&self, count: u32) -> Result<Range<u32>>;
    pub fn free_blocks(&self, start: u32, count: u32) -> Result<()>;
}
```

```rust
pub struct DirEntry {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
    pub name: CStr256,
}
```

```rust
pub struct DirEntryIter<'a> { /* ... */ }
impl<'a> DirEntryIter<'a> {
    pub fn new(buf: &'a [u8], limit: usize, max_inumber: u32) -> Result<Self>;
    pub fn next_entry(&mut self) -> Result<Option<DirEntry>>;
}
```

```rust
pub enum DirEntryFileType {
    Unknown = 0,
    File = 1,
    Dir = 2,
    // ...
}
```

```rust
pub trait DirentVisitor {
    fn visit(&mut self, name: &str, ino: u64, type_: InodeType, offset: usize) -> Result<()>;
}
```

[GUARANTEE]
```rust
impl Inode {
    /// Finds a directory entry by name and returns its inode number.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:342 (ext2_find_entry)
    ///
    /// # Lock
    /// Acquires inner read lock. PageCache read may trigger read_page_async
    /// which re-acquires read lock — safe (RwMutex read locks are reentrant).
    pub fn find_entry(&self, name: &str) -> Result<u32>;

    /// Reads directory entries starting at byte offset and feeds visitor.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:257 (ext2_readdir)
    ///
    /// # Lock
    /// Acquires inner read lock. Same reentrant safety as find_entry.
    pub fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize>;

    /// Checks whether this directory contains only `.` and `..` as live entries.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:659 (ext2_empty_dir)
    ///
    /// # Lock
    /// Acquires inner read lock.
    pub fn empty_dir(&self) -> bool;

    /// Adds a new directory entry to this directory inode.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:476 (ext2_add_link)
    ///
    /// # Lock — upread + upgrade protocol (exFAT pattern)
    /// Acquires inner upread lock. Scans blocks and writes entry via PageCache
    /// under upread (compatible with read_page_async's read lock).
    /// Upgrades to write lock for metadata mutation (alloc, size, timestamps, persist).
    /// upread is exclusive with other upread/write, preventing concurrent dir mutation.
    pub fn add_entry(&self, name: &str, ino: u32, file_type: DirEntryFileType) -> Result<()>;

    /// Deletes a directory entry by name.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:560 (ext2_delete_entry)
    ///
    /// # Lock — upread + upgrade protocol
    /// Acquires inner upread lock. Locates and modifies entry via PageCache
    /// under upread. Upgrades to write lock for timestamps and persist.
    pub fn delete_entry(&self, name: &str) -> Result<()>;

    /// Rewrites an existing entry's inode/type in-place.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:450 (ext2_set_link)
    ///
    /// # Lock — upread + upgrade protocol
    /// Acquires inner upread lock. Locates and modifies entry via PageCache
    /// under upread. Upgrades to write lock for timestamps and persist.
    pub fn set_link(
        &self,
        name: &str,
        new_ino: u32,
        file_type: DirEntryFileType,
        update_times: bool,
    ) -> Result<()>;

    /// Initializes a newly allocated directory inode with `.` and `..` entries.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:617 (ext2_make_empty)
    ///
    /// # Lock — write lock then downgrade to upread
    /// Acquires write lock for block allocation and metadata setup.
    /// Downgrades to upread for PageCache write (`.`/`..` entries).
    /// Upgrades back to write lock for persist.
    /// On failure: discard_range + rollback metadata + free block.
    pub fn make_empty(&self, parent_ino: u32) -> Result<()>;
}
```

[SPECIFICATION]

## Read-only operations

Pre (find_entry):
- `self.type_ == InodeType::Dir`.

Post (find_entry: success):
- Acquires inner read lock.
- Obtains `size`, `block_size`, `max_inumber` from desc and fs.
- Iterates logical blocks `0..size.div_ceil(block_size)`:
  - For each block, reads block data via `page_cache.pages().read_bytes(block_offset, &mut buf)`.
  - Parses entries with `DirEntryIter`.
  - If entry matches `name` (same length and bytes), returns `Ok(entry.inode)`.
- If no match found, returns `Err(ENOENT)`.

Post (find_entry: failure):
- `Err(ENOTDIR)` if not a directory.
- `Err(ENOENT)` if name not found.
- `Err(EIO)` if PageCache read fails.

Pre (readdir_at):
- `self.type_ == InodeType::Dir`.

Post (readdir_at: success):
- Acquires inner read lock.
- Obtains `size`, `block_size`, `max_inumber`.
- Starting from `offset`, iterates blocks:
  - Reads block data via `page_cache.pages().read_bytes(block_offset, &mut buf)`.
  - Parses entries, calls `visitor.visit()` for each valid entry (inode != 0).
  - If visitor returns error, stops and returns bytes advanced so far.
- Returns `Ok(bytes_advanced)`.

Post (readdir_at: failure):
- `Err(ENOTDIR)` if not a directory.
- `Err(EIO)` if PageCache read fails.

Pre (empty_dir):
- `self.type_ == InodeType::Dir`.

Post (empty_dir):
- Acquires inner read lock.
- Iterates all directory blocks via `page_cache.pages().read_bytes()`.
- Returns `true` if only `.` and `..` entries have non-zero inode.
- Returns `false` if any other live entry exists, or on any I/O error.

## Write operations — upread + upgrade protocol

### add_entry

Pre:
- `self.type_ == InodeType::Dir`.
- `name` is non-empty and <= 255 bytes.
- `ino > 0` and `ino <= max_inumber`.

Post (success):
- Acquires inner upread lock.
- Obtains `size`, `block_size`, `max_inumber`, `reclen = DirEntry::dir_rec_len(name.len())`.
- Scans existing blocks `0..data_blocks` via `page_cache.pages().read_bytes()`:
  - read_page_async callback acquires read lock — compatible with upread, no deadlock.
  - Checks for duplicate name → `Err(EEXIST)`.
  - Finds free slot (inode==0 with enough rec_len, or split of occupied entry).
  - Records candidate slot info (block_idx, offset, rec_len, used_rec_len).
- If no slot found in existing blocks (growth needed):
  - Upgrades upread → write lock.
  - Calls `get_or_alloc_block(data_blocks, true)` to allocate new block.
  - Updates `desc.size += block_size`, `desc.blocks`.
  - Calls `page_cache.resize(new_size_aligned)` to extend PageCache.
    - Grow path: no callbacks — safe under write lock.
  - Downgrades write → upread lock.
  - New block slot: offset=0, rec_len=block_size.
- Writes new entry into slot via `page_cache.pages().write_bytes()`:
  - If splitting occupied entry, updates predecessor's rec_len first.
  - Writes new entry at computed offset.
  - PageCache write may trigger read_page_async — compatible with upread.
- Upgrades upread → write lock.
- Updates `desc.mtime` and `desc.ctime`.
- Calls `persist_inode_and_sync`.

Post (add_entry: growth failure rollback):
- If block allocation or page_cache.resize fails after upgrade:
  - `page_cache.discard_range(old_size_aligned..new_size_aligned)` — discard any
    pages for the partially-allocated region. No callbacks — safe under write lock.
  - Restore `desc.size`, `desc.blocks` to pre-growth values.
  - `truncate_blocks(old_size)` — free excess blocks.
  - Return the original error.

Post (add_entry: failure):
- `Err(ENOTDIR)` if not a directory.
- `Err(EEXIST)` if name already exists.
- `Err(EINVAL)` if name empty/too long or ino invalid.
- `Err(ENOSPC)` if block allocation fails during growth.
- `Err(EIO)` if PageCache I/O fails.

### delete_entry

Pre:
- `self.type_ == InodeType::Dir`.
- `name` is non-empty and <= 255 bytes.

Post (success):
- Acquires inner upread lock.
- Scans blocks via `page_cache.pages().read_bytes()` to locate target entry.
  - Records target info (block_idx, entry_offset, rec_len, predecessor info).
  - If not found, returns `Err(EIO)`.
- Reads target block into buffer via `page_cache.pages().read_bytes()`.
- Modifies buffer: zeroes target entry's inode field, merges rec_len into predecessor.
- Writes modified block back via `page_cache.pages().write_bytes()`.
  - All PageCache I/O under upread — compatible with read_page_async.
- Upgrades upread → write lock.
- Updates `desc.mtime` and `desc.ctime`.
- Calls `persist_inode_and_sync`.

Post (delete_entry: failure):
- `Err(ENOTDIR)` if not a directory.
- `Err(EINVAL)` if name empty/too long.
- `Err(EIO)` if entry not found or PageCache I/O fails.

### set_link

Pre:
- `self.type_ == InodeType::Dir`.
- `name` is non-empty and <= 255 bytes, not `.`.
- `new_ino >= ROOT_INO` and `new_ino <= max_inumber`.

Post (success):
- Acquires inner upread lock.
- Locates target entry via `page_cache.pages().read_bytes()` scan.
- Reads target block, modifies inode number and file_type in buffer.
- Writes modified block back via `page_cache.pages().write_bytes()`.
- Upgrades upread → write lock.
- If `update_times`: updates `desc.mtime` and `desc.ctime`.
- Else: clears `INDEX_DIR` flag only.
- Calls `persist_inode_and_sync`.

Post (set_link: failure):
- `Err(ENOTDIR)` if not a directory.
- `Err(EINVAL)` if name invalid or ino out of range.
- `Err(ENOENT)` if entry not found.
- `Err(EIO)` if PageCache I/O fails.

### make_empty

Pre:
- `self.type_ == InodeType::Dir`.
- `parent_ino > 0` and `parent_ino <= max_inumber`.
- Directory has no existing data blocks (block_ptrs[0] == 0).

Post (success):
- Acquires inner write lock (need `get_or_alloc_block` which requires `&mut self`).
- Saves rollback state: `old_ptr0`, `old_size`, `old_blocks`.
- Allocates one data block via `get_or_alloc_block(0, true)`.
- Updates `desc.size = block_size`, `desc.blocks`.
- Calls `page_cache.resize(block_size)` to extend PageCache.
  - Grow path: no callbacks — safe under write lock.
- Downgrades write → upread lock.
- Constructs `.` and `..` entries in a buffer (zero-filled, canonical layout).
- Writes buffer via `page_cache.pages().write_bytes(0, &buf)`.
  - PageCache write may trigger read_page_async — compatible with upread.
- Upgrades upread → write lock.
- Calls `persist_inode_and_sync`.

Post (make_empty: failure rollback):
- On allocation failure: return error directly (no state changed yet).
- On page_cache.resize or write failure after allocation:
  - `page_cache.discard_range(0..block_size)` — discard dirty page. No callbacks.
  - Restore `desc.block_ptrs[0]`, `desc.size`, `desc.blocks` to saved values.
  - `fs.free_blocks(new_bid, 1)` — free allocated block.
  - Return the original error.
- On persist failure:
  - Same rollback as above: discard_range, restore desc, free block.

Post (make_empty: failure):
- `Err(ENOTDIR)` if not a directory.
- `Err(EINVAL)` if parent_ino out of range.
- `Err(ENOSPC)` if block allocation fails.
- `Err(EIO)` if block_ptrs[0] already occupied or PageCache I/O fails.

## Compound operations — lock protocol changes

### mkdir (on Inode, not InodeInner)

Current: `InodeInner::mkdir(&mut self, ...)` holds write lock throughout,
calls `make_empty` and `add_entry` on InodeInner directly.

New: `Inode::mkdir(&self, ...)` orchestrates lock transitions:
- Acquires upread lock for validation and parent link reservation.
- Upgrades to write for `desc.links_count` increment.
- Downgrades to upread, then drops lock before calling child `make_empty`.
- Calls `self.add_entry(...)` which acquires its own upread lock.
- On failure at any step: rollback parent links_count, free child inode/blocks.

### rmdir (on Inode, not InodeInner)

Current: `InodeInner::rmdir(&mut self, ...)` holds write lock throughout.

New: `Inode::rmdir(&self, ...)`:
- Calls `self.find_entry(name)` (read lock, via page cache).
- Loads child inode, checks empty_dir (read lock on child).
- Calls `self.delete_entry(name)` (upread + upgrade on self).
- Updates child metadata under child's write lock.
- Updates parent links_count under self's write lock.

### rename (on Inode)

Current: `rename_same_dir` holds single write lock; `rename_inner` holds
two write locks via `write_lock_two_inodes`.

New: Sub-operations (`find_entry`, `add_entry`, `delete_entry`, `set_link`)
each manage their own upread/upgrade locks internally. The outer rename
function does NOT hold a persistent lock across sub-operations.

For same-dir rename:
- `find_entry(old_name)` — read lock (page cache).
- `find_entry(new_name)` — read lock (page cache).
- If replacing: `set_link(new_name, old_ino, ...)` — upread + upgrade.
- Else: `add_entry(new_name, old_ino, ...)` — upread + upgrade.
- `delete_entry(old_name)` — upread + upgrade.
- Update links_count under write lock.

For cross-dir rename:
- Lock ordering: acquire upread on both dirs by ascending ino to avoid deadlock.
  Helper: `upread_two_inodes(a, b) -> (UpgradeableGuard, UpgradeableGuard)`.
- Sub-operations on each dir use the already-held upread lock (passed as guard).
- Upgrade individual guards to write as needed for metadata updates.

Note: Cross-dir rename requires sub-operations to accept an existing upread
guard rather than acquiring their own. This requires internal variants:
`add_entry_with_guard`, `delete_entry_with_guard`, `set_link_with_guard`
that take `&RwMutexUpgradeableGuard<InodeInner>` instead of acquiring upread.

## Invariants

- All directory data I/O goes through PageCache, never direct block device access.
- Directory operations share the same Inode PageCache as file data operations.
- read_page_async acquires inner read lock only.
- Write operations use upread + upgrade protocol (exFAT pattern):
  - upread for PageCache I/O (compatible with read_page_async's read lock).
  - upgrade to write for metadata mutation (alloc, size, timestamps, persist).
  - upread is exclusive with other upread/write — prevents concurrent dir mutation.
- This replaces the split-lock protocol (which had TOCTOU issues between phases).
- make_empty uses write → downgrade → upread → upgrade because block allocation
  (`get_or_alloc_block`) requires `&mut InodeInner`.
- Rollback on failure uses `page_cache.discard_range()` to drop dirty pages
  without writeback (no callbacks — safe under write lock), then restores metadata.

[DIFF]
Linux: Directory data is accessed via `ext2_get_folio` (dir.c:189) which calls
  `read_mapping_folio(mapping, n, NULL)` — same page cache as file data.
  → Asterinas: Directory data uses the same Inode PageCache. Directory operations
  (find_entry, add_entry, readdir, delete_entry, make_empty, set_link) read/write
  through `page_cache.pages()`.
  Reason: Unified caching for both file and directory data, matching Linux's model.

Linux: `ext2_find_entry` (dir.c:342) uses `ext2_get_folio` per page, iterates
  with `ext2_next_page` and `ext2_last_byte` for boundary handling.
  → Asterinas: Iterates logical blocks, reads via `page_cache.pages().read_bytes()`,
  parses with `DirEntryIter`. Block-based iteration instead of page-based.
  Reason: Asterinas PageCache is block-aligned, not page-aligned.

Linux: `ext2_add_link` (dir.c:476) scans pages for free slot, calls
  `ext2_get_folio` then `lock_page`/`ext2_commit_chunk` for atomic update.
  The folio lock serializes concurrent modifications to the same page.
  → Asterinas: upread lock serializes all write operations on the same directory
  inode (upread is exclusive with other upread/write). PageCache I/O under upread
  is safe because read_page_async only needs read lock (compatible with upread).
  Reason: RwMutex upread replaces Linux's per-page folio lock for serialization.
  exFAT uses the same pattern (exfat/inode.rs:1097).

Linux: `ext2_delete_entry` (dir.c:560) takes a pre-located `ext2_dir_entry_2 *dir`
  and its containing page, then zeroes the inode field in-place under folio lock.
  → Asterinas: Single upread phase — locate entry, read-modify-write block via
  PageCache, then upgrade to write for metadata persist. No TOCTOU because upread
  is held continuously (exclusive with other writers).
  Reason: Eliminates the re-validation needed by the split-lock approach.

Linux: `ext2_make_empty` (dir.c:617) allocates a page via `grab_cache_page`,
  maps it with `ext2_get_block`, writes `.`/`..` entries, then commits.
  → Asterinas: write lock for allocation (get_or_alloc_block needs &mut),
  downgrade to upread for PageCache write, upgrade back for persist.
  On failure: discard_range drops dirty pages, then rollback metadata and free block.
  Reason: get_or_alloc_block requires &mut InodeInner, forcing initial write lock.
  Downgrade to upread enables safe PageCache I/O without deadlock.

Linux: `ext2_set_link` (dir.c:450) modifies an existing entry's inode/type
  under folio lock, then commits.
  → Asterinas: upread for locate + read-modify-write via PageCache, upgrade for
  metadata persist. Same pattern as delete_entry.

Linux: Rollback on failure (e.g., ext2_write_failed) uses truncate_pagecache
  to invalidate cached pages.
  → Asterinas: `page_cache.discard_range()` drops dirty pages without writeback
  (no callbacks — safe under write lock). Matches data path write_failed_cleanup.

Cross-dir rename lock ordering:
  Linux: acquires i_rwsem on both directories in inode number order.
  → Asterinas: acquires upread on both directories in ascending ino order via
  `upread_two_inodes()`. Sub-operations use the held guards directly.

[TEST]
## Inode::find_entry
- Find existing entry → returns correct ino
- Find non-existent name → Err(ENOENT)
- Find in empty directory (only . and ..) → Err(ENOENT)
- Find on non-directory inode → Err(ENOTDIR)
- Name at boundary: max-length name (255 bytes)
- Multi-block directory: entry in second block found correctly
- Verify PageCache read triggers read_page_async on cache miss

## Inode::readdir_at
- Read all entries from offset 0 → visitor receives all entries
- Read from mid-directory offset → skips earlier entries
- Offset past end of directory → Ok(0)
- Non-directory inode → Err(ENOTDIR)
- Visitor returns error mid-iteration → stops, returns bytes advanced so far

## Inode::empty_dir
- Directory with only . and .. → true
- Directory with additional entry → false
- Non-directory inode → false
- I/O error during scan → false

## Inode::add_entry
- Add entry to directory with free slot → entry written, timestamps updated
- Add entry requiring split of existing entry → predecessor rec_len updated
- Add entry requiring directory growth → new block allocated, PageCache resized
- Add duplicate name → Err(EEXIST)
- Add to non-directory → Err(ENOTDIR)
- Empty name or name > 255 bytes → Err(EINVAL)
- Block allocation fails during growth → Err(ENOSPC), rollback via discard_range
- Verify upread protocol: no deadlock on PageCache read_page_async callback

## Inode::delete_entry
- Delete existing entry → inode field zeroed, rec_len merged, timestamps updated
- Delete non-existent entry → Err(EIO)
- Delete on non-directory → Err(ENOTDIR)
- Empty name → Err(EINVAL)
- Verify upread protocol: no deadlock on PageCache access

## Inode::set_link
- Rewrite existing entry's inode → entry updated, timestamps updated
- Entry not found → Err(ENOENT)
- update_times=false → only INDEX_DIR flag cleared
- Non-directory → Err(ENOTDIR)

## Inode::make_empty
- Initialize new directory → . and .. entries written via PageCache
- Verify . points to self_ino, .. points to parent_ino
- block_ptrs[0] already occupied → Err(EIO)
- Block allocation fails → Err(ENOSPC), no state change
- PageCache write fails → rollback: discard_range, restore desc, free block
- Persist fails → same rollback
- Non-directory inode → Err(ENOTDIR)
- parent_ino out of range → Err(EINVAL)

## Deadlock Analysis
| Operation              | Lock held     | PageCache op        | Callback needs    | Safe? |
|------------------------|---------------|---------------------|-------------------|-------|
| find_entry             | read          | pages().read_bytes  | read_page → read  | YES (reentrant) |
| readdir_at             | read          | pages().read_bytes  | read_page → read  | YES (reentrant) |
| empty_dir              | read          | pages().read_bytes  | read_page → read  | YES (reentrant) |
| add_entry scan         | upread        | pages().read_bytes  | read_page → read  | YES (upread compat read) |
| add_entry write entry  | upread        | pages().write_bytes | read_page → read  | YES (upread compat read) |
| add_entry growth       | write         | resize (grow)       | no callbacks      | YES |
| delete_entry r-m-w     | upread        | read+write_bytes    | read_page → read  | YES (upread compat read) |
| set_link r-m-w         | upread        | read+write_bytes    | read_page → read  | YES (upread compat read) |
| make_empty alloc       | write         | resize (grow)       | no callbacks      | YES |
| make_empty write ./../ | upread        | pages().write_bytes | read_page → read  | YES (upread compat read) |
| evict/sync (external)  | NONE          | evict_range         | write_page → read | YES |
