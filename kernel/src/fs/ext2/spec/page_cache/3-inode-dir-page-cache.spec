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
    /// Returns the physical Bid for a logical block, or None for sparse holes.
    pub(super) fn get_block(&self, iblock: u32) -> Result<Option<Bid>>;
    /// Returns the physical Bid for a logical block, allocating if `create` is true.
    pub(super) fn get_or_alloc_block(&mut self, iblock: u32, create: bool) -> Result<Option<Bid>>;
    /// Persists inode descriptor to disk.
    pub(super) fn persist_inode_and_sync(&self, fs: &Ext2) -> Result<()>;
}
```

```rust
impl Ext2 {
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

```rust
/// Directory entry as parsed from disk.
pub struct DirEntry {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
    pub name: String,
}
```

```rust
/// Iterator over directory entries in a byte buffer.
pub struct DirEntryIter<'a> { /* ... */ }
impl<'a> DirEntryIter<'a> {
    pub fn new(buf: &'a [u8], limit: usize, max_inumber: u32) -> Result<Self>;
    pub fn next_entry(&mut self) -> Result<Option<DirEntry>>;
}
```

```rust
/// Directory entry file type constants.
pub enum DirEntryFileType {
    Unknown = 0,
    File = 1,
    Dir = 2,
    // ...
}
```

```rust
/// Visitor trait for readdir.
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
    /// # Arguments
    /// * `name` - Name to search for.
    ///
    /// # Returns
    /// * `Ok(u32)` - Inode number of the found entry.
    /// * `Err(ENOTDIR)` - Inode is not a directory.
    /// * `Err(ENOENT)` - Entry not found.
    /// * `Err(EIO)` - I/O failure.
    ///
    /// # Lock
    /// Acquires inner read lock. Reads directory blocks via PageCache.
    pub fn find_entry(&self, name: &str) -> Result<u32>;

    /// Reads directory entries starting at byte offset and feeds visitor.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:257 (ext2_readdir)
    ///
    /// # Arguments
    /// * `offset` - Byte offset within the directory to start reading.
    /// * `visitor` - Visitor callback for each directory entry.
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes advanced.
    ///
    /// # Lock
    /// Acquires inner read lock. Reads directory blocks via PageCache.
    pub fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize>;

    /// Checks whether this directory contains only `.` and `..` as live entries.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:659 (ext2_empty_dir)
    ///
    /// # Lock
    /// Acquires inner read lock. Reads directory blocks via PageCache.
    pub fn empty_dir(&self) -> bool;

    /// Adds a new directory entry to this directory inode.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:476 (ext2_add_link)
    ///
    /// # Arguments
    /// * `name` - Name of the new entry.
    /// * `ino` - Inode number for the new entry.
    /// * `file_type` - Directory entry file type.
    ///
    /// # Returns
    /// * `Ok(())` - Entry added successfully.
    /// * `Err(ENOTDIR)` - Inode is not a directory.
    /// * `Err(EEXIST)` - Entry with same name already exists.
    /// * `Err(ENOSPC)` - No space and cannot grow directory.
    /// * `Err(EIO)` - I/O failure.
    ///
    /// # Lock
    /// Phase 1: read lock — scan existing blocks via PageCache, find slot.
    /// Phase 2: write lock (if growth needed) — alloc block, resize PageCache.
    /// Phase 3: read lock — write new entry via PageCache.
    /// Phase 4: write lock — update timestamps, persist inode.
    pub fn add_entry(&self, name: &str, ino: u32, file_type: DirEntryFileType) -> Result<()>;

    /// Deletes a directory entry by name.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:560 (ext2_delete_entry)
    ///
    /// # Arguments
    /// * `name` - Name of the entry to delete.
    ///
    /// # Returns
    /// * `Ok(())` - Entry deleted successfully.
    /// * `Err(ENOTDIR)` - Inode is not a directory.
    /// * `Err(EINVAL)` - Invalid name.
    /// * `Err(EIO)` - Entry not found or I/O failure.
    ///
    /// # Lock
    /// Phase 1: read lock — scan blocks via PageCache, locate target entry.
    /// Phase 2: write lock — modify entry in PageCache (zero inode), update timestamps, persist.
    pub fn delete_entry(&self, name: &str) -> Result<()>;

    /// Initializes a newly allocated directory inode with `.` and `..` entries.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:617 (ext2_make_empty)
    ///
    /// # Arguments
    /// * `parent_ino` - Inode number of the parent directory.
    ///
    /// # Returns
    /// * `Ok(())` - Directory initialized.
    /// * `Err(ENOTDIR)` - Inode is not a directory.
    /// * `Err(ENOSPC)` - Block allocation failed.
    /// * `Err(EIO)` - I/O failure.
    ///
    /// # Lock
    /// Phase 1: write lock — alloc first block, resize PageCache, update block_ptrs/size.
    /// Phase 2: read lock — write `.` and `..` entries via PageCache.
    /// Phase 3: write lock — persist inode.
    pub fn make_empty(&self, parent_ino: u32) -> Result<()>;
}
```

[SPECIFICATION]
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

Pre (add_entry):
- `self.type_ == InodeType::Dir`.
- `name` is non-empty and <= 255 bytes.
- `ino > 0` and `ino <= max_inumber`.

Post (add_entry: success):
- Phase 1 (read lock):
  - Acquires inner read lock.
  - Scans existing blocks via `page_cache.pages().read_bytes()` for:
    a) Duplicate name check → `Err(EEXIST)` if found.
    b) Free slot (inode==0 with enough rec_len, or split of occupied entry).
  - Records candidate slot info (block_idx, offset, rec_len).
  - Releases read lock.
- Phase 2 (write lock, if growth needed):
  - If no slot found in existing blocks:
    - Acquires write lock.
    - Calls `get_or_alloc_block(data_blocks, true)` to allocate new block.
    - Calls `page_cache.resize(new_size)` to extend PageCache.
    - Updates `desc.size` and `desc.blocks`.
    - Releases write lock.
- Phase 3 (read lock):
  - Acquires inner read lock.
  - Writes the new entry into the slot via `page_cache.pages().write_bytes()`.
  - If splitting an occupied entry, updates the predecessor's rec_len first.
  - Releases read lock.
- Phase 4 (write lock):
  - Acquires write lock.
  - Updates `desc.mtime` and `desc.ctime`.
  - Calls `persist_inode_and_sync`.
  - Releases write lock.

Post (add_entry: failure):
- `Err(ENOTDIR)` if not a directory.
- `Err(EEXIST)` if name already exists.
- `Err(EINVAL)` if name empty/too long or ino invalid.
- `Err(ENOSPC)` if block allocation fails during growth.
- `Err(EIO)` if PageCache I/O fails.

Pre (delete_entry):
- `self.type_ == InodeType::Dir`.
- `name` is non-empty and <= 255 bytes.

Post (delete_entry: success):
- Phase 1 (read lock):
  - Acquires inner read lock.
  - Scans blocks via `page_cache.pages().read_bytes()` to locate target entry.
  - Records target info (block_idx, entry_offset, rec_len, predecessor info).
  - Releases read lock.
- Phase 2 (write lock):
  - Acquires write lock.
  - Reads the target block via `page_cache.pages().read_bytes()`.
  - Zeroes the target entry's inode field (merges rec_len into predecessor if applicable).
  - Writes modified block back via `page_cache.pages().write_bytes()`.
  - Updates `desc.mtime` and `desc.ctime`.
  - Calls `persist_inode_and_sync`.
  - Releases write lock.

Post (delete_entry: failure):
- `Err(ENOTDIR)` if not a directory.
- `Err(EINVAL)` if name empty/too long.
- `Err(EIO)` if entry not found or PageCache I/O fails.

Pre (make_empty):
- `self.type_ == InodeType::Dir`.
- `parent_ino > 0` and `parent_ino <= max_inumber`.
- Directory has no existing data blocks (block_ptrs[0] == 0).

Post (make_empty: success):
- Phase 1 (write lock):
  - Acquires write lock.
  - Allocates one data block via `fs.alloc_blocks(1)`.
  - Sets `desc.block_ptrs[0] = new_bid`.
  - Calls `page_cache.resize(block_size)` to extend PageCache.
  - Updates `desc.size = block_size` and `desc.blocks`.
  - Releases write lock.
- Phase 2 (read lock):
  - Acquires read lock.
  - Constructs `.` and `..` entries in a buffer.
  - Writes buffer via `page_cache.pages().write_bytes(0, &buf)`.
  - Releases read lock.
- Phase 3 (write lock):
  - Acquires write lock.
  - Calls `persist_inode_and_sync`.
  - Releases write lock.
- On failure at any phase: rolls back block_ptrs, size, blocks; frees allocated block.

Post (make_empty: failure):
- `Err(ENOTDIR)` if not a directory.
- `Err(EINVAL)` if parent_ino out of range.
- `Err(ENOSPC)` if block allocation fails.
- `Err(EIO)` if block_ptrs[0] already occupied or PageCache I/O fails.

Invariant:
- All directory data I/O goes through PageCache, never direct block device access.
- Directory operations share the same Inode PageCache as file data operations.
- read_page_async acquires inner read lock; callers holding read lock are safe (reentrant).
- Write operations (add_entry, delete_entry, make_empty) follow split-lock protocol:
  write lock for metadata/allocation → read lock for PageCache I/O → write lock for persist.
- This avoids deadlock since PageCache callbacks (read_page_async) need read lock.

[DIFF]
Linux: Directory data is accessed via `ext2_get_folio` (dir.c:189) which calls
  `read_mapping_folio(mapping, n, NULL)` — same page cache as file data.
  → Asterinas: Directory data uses the same Inode PageCache. Directory operations
  (find_entry, add_entry, readdir, delete_entry, make_empty) read/write through
  `page_cache.pages()`. Write operations follow the same 3-phase lock protocol.
  Reason: Unified caching for both file and directory data, matching Linux's model.

Linux: `ext2_find_entry` (dir.c:342) uses `ext2_get_folio` per page, iterates
  with `ext2_next_page` and `ext2_last_byte` for boundary handling.
  → Asterinas: Iterates logical blocks, reads via `page_cache.pages().read_bytes()`,
  parses with `DirEntryIter`. Block-based iteration instead of page-based.
  Reason: Asterinas PageCache is block-aligned, not page-aligned.

Linux: `ext2_add_link` (dir.c:476) scans pages for free slot, calls
  `ext2_get_folio` then `lock_page`/`ext2_commit_chunk` for atomic update.
  → Asterinas: Split-lock protocol — read lock to scan, write lock if growth needed,
  read lock to write entry, write lock to persist. No page-level locking.
  Reason: RwMutex-based concurrency model replaces Linux page lock.

Linux: `ext2_delete_entry` (dir.c:560) takes a pre-located `ext2_dir_entry_2 *dir`
  and its containing page, then zeroes the inode field in-place.
  → Asterinas: Two-phase — read lock to locate entry, write lock to modify and persist.
  Entry is re-read in write phase to avoid stale data.
  Reason: Lock release between phases requires re-validation.

Linux: `ext2_make_empty` (dir.c:617) allocates a page via `grab_cache_page`,
  maps it with `ext2_get_block`, writes `.`/`..` entries, then commits.
  → Asterinas: Three-phase — write lock to alloc block and resize PageCache,
  read lock to write entries via PageCache, write lock to persist.
  Reason: Same split-lock pattern as other write operations.

[TEST]
## Inode::find_entry
- Find existing entry → returns correct ino
- Find non-existent name → Err(ENOENT)
- Find in empty directory (only . and ..) → Err(ENOENT)
- Find on non-directory inode → Err(ENOTDIR)
- Name at boundary: max-length name (255 bytes)
- Multi-block directory: entry in second block found correctly

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
- Block allocation fails during growth → Err(ENOSPC)
- Verify split-lock protocol: no deadlock on PageCache access

## Inode::delete_entry
- Delete existing entry → inode field zeroed, rec_len merged, timestamps updated
- Delete non-existent entry → Err(EIO)
- Delete on non-directory → Err(ENOTDIR)
- Empty name → Err(EINVAL)

## Inode::make_empty
- Initialize new directory → . and .. entries written via PageCache
- Verify . points to self_ino, .. points to parent_ino
- block_ptrs[0] already occupied → Err(EIO)
- Block allocation fails → Err(ENOSPC), state rolled back
- Non-directory inode → Err(ENOTDIR)
- parent_ino out of range → Err(EINVAL)
