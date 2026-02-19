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
/// Scan result for directory slot search.
enum DirScanResult {
    /// Found a usable slot in an existing block.
    Slot(DirSlotInfo),
    /// No slot found; directory must grow by one block.
    NeedGrowth,
}

/// Information about a candidate directory entry slot.
struct DirSlotInfo {
    /// Byte offset within the directory (block_idx * block_size + offset_in_block).
    dir_offset: usize,
    /// Current rec_len of the candidate slot.
    slot_rec_len: usize,
    /// Minimal occupied length of the existing entry head (0 if slot is free).
    used_rec_len: usize,
}

/// Located directory entry for delete/set_link.
struct DirEntryTarget {
    /// Byte offset of the target entry within the directory.
    dir_offset: usize,
    /// rec_len of the target entry.
    entry_rec_len: usize,
}
```

```rust
/// InodeInner: phased directory operations.
/// Each method's &self / &mut self matches the required lock level:
///   &self  → caller holds upread or read (PageCache I/O safe)
///   &mut self → caller holds write (no PageCache I/O)
impl InodeInner {
    /// Phase 1: Scans directory blocks for a free slot or duplicate name.
    /// Reads via PageCache — caller must hold upread or read.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:476 (ext2_add_link scan loop)
    fn scan_dir_for_slot(&self, name: &str, fs: &Ext2)
        -> Result<DirScanResult>;

    /// Phase 2: Grows directory by one block.
    /// Calls get_or_alloc_block + page_cache.resize — caller must hold write.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:476 (ext2_add_link growth path)
    fn grow_dir_block(&mut self, fs: &Ext2) -> Result<DirSlotInfo>;

    /// Phase 3: Writes a new entry into a slot via PageCache.
    /// Caller must hold upread (PageCache I/O triggers read_page_async → read lock).
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:476 (ext2_add_link commit)
    fn write_dir_entry_to_cache(
        &self, slot: &DirSlotInfo, name: &str, ino: u32, ft: u8,
    ) -> Result<()>;

    /// Phase 4: Updates directory timestamps and persists inode.
    /// Caller must hold write (metadata mutation).
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:84 (ext2_commit_chunk)
    fn commit_dir_metadata(&mut self, fs: &Ext2) -> Result<()>;

    /// Locates a directory entry by name. Returns target info for delete/set_link.
    /// Reads via PageCache — caller must hold upread or read.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:342 (ext2_find_entry)
    fn find_entry_target(&self, name: &str, fs: &Ext2)
        -> Result<DirEntryTarget>;

    /// Deletes a located entry by zeroing inode and merging rec_len.
    /// Writes via PageCache — caller must hold upread.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:560 (ext2_delete_entry)
    fn delete_entry_in_cache(&self, target: &DirEntryTarget) -> Result<()>;

    /// Rewrites a located entry's inode/type via PageCache.
    /// Caller must hold upread.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:450 (ext2_set_link)
    fn set_link_in_cache(
        &self, target: &DirEntryTarget, new_ino: u32, ft: u8,
    ) -> Result<()>;
}
```

```rust
/// InodeInner: read-only directory operations (caller holds read or upread).
impl InodeInner {
    /// Finds a directory entry by name and returns its inode number.
    /// Linux: /root/linux/fs/ext2/dir.c:342 (ext2_find_entry)
    pub(super) fn find_entry(&self, name: &str) -> Result<u32>;

    /// Reads directory entries starting at byte offset and feeds visitor.
    /// Linux: /root/linux/fs/ext2/dir.c:257 (ext2_readdir)
    pub(super) fn readdir_at(
        &self, offset: usize, visitor: &mut dyn DirentVisitor,
    ) -> Result<usize>;

    /// Checks whether directory contains only `.` and `..` as live entries.
    /// Linux: /root/linux/fs/ext2/dir.c:659 (ext2_empty_dir)
    pub(super) fn empty_dir(&self) -> bool;
}
```

```rust
/// Inode: public directory API (manages upread/upgrade internally).
impl Inode {
    /// Adds a new directory entry. Acquires upread, upgrades as needed.
    /// Linux: /root/linux/fs/ext2/dir.c:476 (ext2_add_link)
    pub(super) fn add_entry(
        &self, name: &str, ino: u32, file_type: DirEntryFileType,
    ) -> Result<()>;

    /// Deletes a directory entry by name. Acquires upread, upgrades for metadata.
    /// Linux: /root/linux/fs/ext2/dir.c:560 (ext2_delete_entry)
    pub(super) fn delete_entry(&self, name: &str) -> Result<()>;

    /// Rewrites an existing entry's inode/type. Acquires upread, upgrades for metadata.
    /// Linux: /root/linux/fs/ext2/dir.c:450 (ext2_set_link)
    pub(super) fn set_link(
        &self, name: &str, new_ino: u32,
        file_type: DirEntryFileType, update_times: bool,
    ) -> Result<()>;

    /// Initializes directory with `.` and `..`. Acquires write, downgrades for I/O.
    /// Linux: /root/linux/fs/ext2/dir.c:617 (ext2_make_empty)
    pub(super) fn make_empty(&self, parent_ino: u32) -> Result<()>;

    /// Reads directory entries. Acquires read lock.
    pub(super) fn readdir_at(
        &self, offset: usize, visitor: &mut dyn DirentVisitor,
    ) -> Result<usize>;

    /// Finds entry by name. Acquires read lock.
    pub(super) fn lookup(&self, name: &str) -> Result<Arc<Inode>>;

    /// Checks if directory is empty. Acquires read lock.
    pub(super) fn empty_dir(&self) -> bool;
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

## Write operations — phased protocol

### InodeInner::scan_dir_for_slot

Pre:
- `self.desc.type_ == InodeType::Dir`.
- Caller holds upread or read lock.

Post (success):
- Obtains `size`, `block_size`, `max_inumber`, `reclen`.
- Scans blocks `0..data_blocks` via `page_cache.pages().read_bytes()`:
  - Checks for duplicate name → `Err(EEXIST)`.
  - Finds free slot (inode==0 with enough rec_len, or split of occupied entry).
- Returns `DirScanResult::Slot(info)` or `DirScanResult::NeedGrowth`.

### InodeInner::grow_dir_block

Pre:
- Caller holds write lock.

Post (success):
- Calls `get_or_alloc_block(data_blocks, true)`.
- Updates `desc.size += block_size`, `desc.blocks`.
- Calls `page_cache.resize(new_size)` — grow path, no callbacks.
- Returns `DirSlotInfo` for the new block (offset=0, rec_len=block_size).

Post (failure rollback):
- `page_cache.discard_range(old_size..new_size)`.
- Restore `desc.size`, `desc.blocks`.
- `truncate_blocks(old_size)` — free excess blocks.

### InodeInner::write_dir_entry_to_cache

Pre:
- Caller holds upread lock.
- `slot` is a valid `DirSlotInfo` from scan or grow.

Post (success):
- If splitting occupied entry, updates predecessor's rec_len first.
- Writes new entry at `slot.dir_offset` via `page_cache.pages().write_bytes()`.
- PageCache I/O may trigger read_page_async → read lock, compatible with upread.

### InodeInner::commit_dir_metadata

Pre:
- Caller holds write lock.

Post (success):
- Updates `desc.mtime` and `desc.ctime`.
- Calls `persist_inode_and_sync`.

### InodeInner::find_entry_target / delete_entry_in_cache / set_link_in_cache

find_entry_target:
- Caller holds upread or read. Scans via PageCache, returns `DirEntryTarget`.

delete_entry_in_cache:
- Caller holds upread. Reads block, zeroes inode, merges rec_len, writes back.

set_link_in_cache:
- Caller holds upread. Reads block, modifies inode/type, writes back.

### Inode::add_entry (orchestrator)

Pre:
- `self.type_ == InodeType::Dir`.
- `name` non-empty, <= 255 bytes. `ino > 0`, `ino <= max_inumber`.

Post (success):
- Acquires upread. Calls `scan_dir_for_slot`.
- If NeedGrowth: upgrades → write, calls `grow_dir_block`, downgrades → upread.
- Calls `write_dir_entry_to_cache` (under upread).
- Upgrades → write. Calls `commit_dir_metadata`.

Post (failure):
- `Err(EEXIST)` if duplicate. `Err(ENOSPC)` if growth fails. `Err(EIO)` on I/O.

### Inode::delete_entry (orchestrator)

Pre:
- `self.type_ == InodeType::Dir`. `name` non-empty, <= 255 bytes.

Post (success):
- Acquires upread. Calls `find_entry_target`.
- Calls `delete_entry_in_cache` (under upread).
- Upgrades → write. Calls `commit_dir_metadata`.

Post (failure):
- `Err(EIO)` if not found or I/O fails.

### Inode::set_link (orchestrator)

Pre:
- `self.type_ == InodeType::Dir`. `name` non-empty, not `.`.
- `new_ino >= ROOT_INO`, `new_ino <= max_inumber`.

Post (success):
- Acquires upread. Calls `find_entry_target`.
- Calls `set_link_in_cache` (under upread).
- Upgrades → write. If `update_times`: updates timestamps. Else: clears INDEX_DIR.
- Calls `persist_inode_and_sync`.

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
  - buf.len() == block_size == PAGE_SIZE, so Vmo treats this as a full-page mid
    segment with WILL_OVERWRITE → commit_overwrite skips disk read. No
    read_page_async callback triggered. Safe even under upread.
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

New: `Inode::mkdir(&self, ...)` uses phased InodeInner methods:
- Acquires upread on parent. Calls `scan_dir_for_slot(child_name)` to check
  EEXIST and find slot (PageCache I/O under upread — safe).
- Upgrades → write. Increments `desc.links_count`.
  If NeedGrowth: calls `grow_dir_block` (under write — no PageCache I/O).
- Downgrades → upread (keep lock held — prevents TOCTOU on slot).
- Creates child inode via `fs.create_inode()` (no lock conflict).
  Calls child `make_empty(parent_ino)` (child manages its own lock;
  parent upread + child write = safe, different lock instances).
- Calls `write_dir_entry_to_cache` on parent (still under upread —
  PageCache I/O safe).
- Upgrades → write. Calls `commit_dir_metadata`.
- On failure at any step: rollback parent links_count, free child inode/blocks.

### rmdir (on Inode, not InodeInner)

Current: `InodeInner::rmdir(&mut self, ...)` holds write lock throughout.

New: `Inode::rmdir(&self, ...)` uses phased methods:
- Acquires upread on self. Calls `find_entry(name)` (PageCache I/O safe).
- Loads child inode. Acquires read lock on child, checks `empty_dir`.
- Calls `find_entry_target(name)` + `delete_entry_in_cache(target)` (under
  upread — PageCache I/O safe).
- Upgrades self → write. Calls `commit_dir_metadata`.
- Updates child metadata under child's write lock (links_count, dtime).
- Updates parent `links_count`. Calls `persist_inode_and_sync`.

### rename (on Inode)

Current: `rename_same_dir` holds single write lock; `rename_inner` holds
two write locks via `write_lock_two_inodes`.

New: Uses phased InodeInner methods. PageCache I/O always under upread,
metadata mutation under write.

For same-dir rename:
- Acquires upread on self.
- `find_entry(old_name)` / `find_entry(new_name)` on `&InodeInner`.
- If replacing: `find_entry_target` + `set_link_in_cache` (under upread).
- Else: `scan_dir_for_slot` (under upread).
- Upgrades → write.
  - If NeedGrowth: `grow_dir_block`.
  - Downgrades → upread. `write_dir_entry_to_cache`. Upgrades → write.
- `find_entry_target(old_name)` under upread (downgrade first if needed).
  `delete_entry_in_cache`. Upgrades → write.
- Update `links_count`, `commit_dir_metadata`.

For cross-dir rename:
- `upread_two_inodes(a, b)` — ascending ino order.
- `find_entry` on each `&InodeInner` via held guards.
- PageCache I/O phases (scan, write_entry, delete_entry_in_cache,
  set_link_in_cache) under upread on the relevant directory.
- Upgrade individual guards → write for `grow_dir_block`,
  `commit_dir_metadata`, `links_count` updates.
- No `_with_guard` variants — upgrade gives `&mut InodeInner` directly.

## Invariants

- All directory data I/O goes through PageCache, never direct block device access.
- Directory operations share the same Inode PageCache as file data operations.
- read_page_async and write_page_async both acquire inner read lock only.
- **No PageCache I/O under write lock.** write lock is incompatible with read lock
  on the same RwMutex — if PageCache I/O triggers read_page_async (which acquires
  inner.read()), it will deadlock. All PageCache read/write must happen under upread.
- Write operations use upread + upgrade protocol (exFAT pattern):
  - upread for PageCache I/O (compatible with read_page_async's read lock).
  - upgrade to write for metadata mutation (alloc, size, timestamps, persist).
  - upread is exclusive with other upread/write — prevents concurrent dir mutation.
- persist_inode_and_sync is safe under write lock: it writes to the block group's
  inode table page cache (separate lock domain), not the inode's own data page cache.
- page_cache.resize (grow path) is safe under write lock: no callbacks triggered.
- page_cache.discard_range is safe under write lock: only clears LruCache, no callbacks.
- make_empty uses write → downgrade → upread → upgrade because block allocation
  (`get_or_alloc_block`) requires `&mut InodeInner`. The PageCache write of `.`/`..`
  entries writes a full block (block_size == PAGE_SIZE), so Vmo uses WILL_OVERWRITE
  (commit_overwrite) which skips disk read — no read_page_async callback triggered.
- Compound operations (mkdir, rmdir, unlink, link, rename) hold upread for the
  entire operation to prevent TOCTOU. Upgrade to write only for &mut InodeInner access.

[DIFF]
Current Asterinas: All directory operations (find_entry, add_entry, delete_entry,
  set_link, make_empty) use direct block device I/O via
  `fs.block_device().read_bytes(bid.to_offset(), &mut buf)` — completely bypassing
  the inode's PageCache.
  → New: All directory data I/O goes through `page_cache.pages().read_bytes()` /
  `write_bytes()`, matching Linux's `ext2_get_folio` (dir.c:189) which calls
  `read_mapping_folio(mapping, n, NULL)` — same page cache as file data.
  Consequences: (1) Directory reads can hit cache (no disk I/O on repeated lookups).
  (2) Directory writes mark pages dirty; data is not immediately durable until
  sync/fsync — matching Linux's `ext2_commit_chunk` which only marks dirty.
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
  `upread_two_inodes()`. PageCache I/O under upread, then upgrade individual
  guards to write for `&mut InodeInner` access (add_entry, delete_entry, etc.).
  No `_with_guard` variants needed — upgrade gives `&mut InodeInner` directly.

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
| add_entry metadata     | write         | persist_inode_sync  | BG inode table PC | YES (different lock domain) |
| delete_entry r-m-w     | upread        | read+write_bytes    | read_page → read  | YES (upread compat read) |
| delete_entry metadata  | write         | persist_inode_sync  | BG inode table PC | YES (different lock domain) |
| set_link r-m-w         | upread        | read+write_bytes    | read_page → read  | YES (upread compat read) |
| make_empty alloc       | write         | resize (grow)       | no callbacks      | YES |
| make_empty write ./../ | upread        | pages().write_bytes | WILL_OVERWRITE    | YES (full page, no read_page) |
| make_empty persist     | write         | persist_inode_sync  | BG inode table PC | YES (different lock domain) |
| make_empty rollback    | write         | discard_range       | no callbacks      | YES |
| rmdir (parent)         | upread→write  | find+delete via PC  | read_page → read  | YES (upread phase for PC I/O) |
| rmdir (child)          | write(child)  | none (metadata only)| none              | YES |
| mkdir (parent)         | upread→write  | add_entry via PC    | read_page → read  | YES (upread phase for PC I/O) |
| link (parent dir)      | upread→write  | add_entry via PC    | read_page → read  | YES (upread phase for PC I/O) |
| link (target file)     | write         | none (metadata only)| none              | YES |
| rename same-dir        | upread→write  | find+add+del via PC | read_page → read  | YES (upread phase for PC I/O) |
| rename cross-dir       | upread(both)→ | find+add+del via PC | read_page → read  | YES (upread compat read) |
|                        | upgrade each  | persist_inode_sync  | BG inode table PC | YES (different lock domain) |
| evict/sync (external)  | NONE          | evict_range         | write_page → read | YES |

NOTE on link: `link` uses upread+upgrade on parent dir for add_entry (PageCache I/O
under upread, metadata under write). Target file only needs write lock for metadata
(increment links_count, persist) — no PageCache I/O on target.
