[PROMPT]
Provide modifications to `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_add_link (growth path)  → fs/ext2/dir.c:496
ext2_get_folio               → fs/ext2/dir.c:189
ext2_make_empty              → fs/ext2/dir.c:617
ext2_empty_dir               → fs/ext2/dir.c:659
ext2_truncate_blocks         → fs/ext2/inode.c:1172

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::dir::{DirEntry, DirEntryIter};
```

```rust
use super::fs::Ext2;
```

```rust
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
#[derive(Debug)]
pub struct InodeInner {
    desc: Dirty<InodeDesc>,
    is_freed: bool,
    weak_self: Weak<Inode>,
    fs: Weak<Ext2>,
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
    fn truncate_blocks(&mut self, new_size: usize) -> Result<()>;
    fn persist_inode_and_sync(&self, fs: &Ext2) -> Result<()>;
    fn update_dir_timestamps_and_flags(&mut self) -> Result<()>;
}
```

```rust
impl Ext2 {
    pub fn block_device(&self) -> &dyn BlockDevice;
    pub fn block_size(&self) -> usize;
    pub(super) fn alloc_blocks(&self, count: u32) -> Result<Range<u32>>;
    pub(super) fn free_blocks(&self, start: u32, count: u32) -> Result<()>;
}
```

[GUARANTEE]
impl InodeInner {
    /// Adds a new directory entry, growing the directory via indirect blocks
    /// if all existing blocks are full.
    ///
    /// Refactored: replaces `link_new_data_block` (direct-only, capped at 12)
    /// with `get_or_alloc_block(iblock, true)` for directory growth.
    ///
    /// Linux: fs/ext2/dir.c:476 (ext2_add_link)
    pub(super) fn add_entry(
        &mut self,
        name: &str,
        ino: u32,
        file_type: DirEntryFileType,
    ) -> Result<()>;

    /// Releases all data blocks of a directory inode for cleanup.
    ///
    /// Refactored: replaces direct-only `block_ptrs[0..12]` iteration
    /// with `truncate_blocks(0)` to handle indirect blocks.
    ///
    /// Used by mkdir/rmdir rollback paths.
    fn release_dir_data_blocks_for_cleanup(&mut self, fs: &Ext2) -> Result<()>;
}

[SPECIFICATION]

## 8.1.0  add_entry — Directory Growth via Indirect Blocks

Pre (add_entry):
- `self.desc.type_ == InodeType::Dir`.
- `name` is non-empty and `name.len() <= 255`.
- `ino` is in `[1, sb.total_inodes()]`.
- `self.fs` can be upgraded to a live `Arc<Ext2>`.
- `get_or_alloc_block` is available and supports the full indirect block tree.

Post (add_entry: success — existing slot found):
- Unchanged from phase-06 spec: scans existing directory blocks, finds a
  reusable slot (free entry or splittable tail), writes the new entry,
  persists the block, updates timestamps.
- Read path uses `get_block()` which already supports indirect blocks.

Post (add_entry: success — directory growth):
- When no reusable slot exists in any current block, the directory must grow.
- **Old behavior** (phase-06): called `link_new_data_block(iblock, bid)` which
  rejected `iblock >= 12` with `Err(ENOSPC)`.
- **New behavior**: replaces the `alloc_blocks(1)` + `link_new_data_block()`
  sequence with a single call to `get_or_alloc_block(data_blocks as u32, true)`:
  1. `data_blocks = self.desc.size as usize / chunk_size` (current block count).
  2. Calls `self.get_or_alloc_block(data_blocks as u32, true)`:
     - Returns `Ok(Some(bid))`: the newly allocated block (may be direct or
       indirect; `get_or_alloc_block` handles the full block tree).
     - Returns `Err(ENOSPC)`: propagated as-is.
     - Returns `Err(EIO)`: propagated as-is.
     - Returns `Ok(None)`: unreachable with `create=true`; treat as `Err(EIO)`.
  3. Initializes the new block: zero-fills, writes a single free entry spanning
     the entire block (`inode=0, rec_len=chunk_size`).
  4. Writes the initialized block to disk.
  5. On write failure: no explicit block free needed — the block is already
     linked into the inode's block tree by `get_or_alloc_block`. The caller
     can retry or the block will be reclaimed on inode deletion.
  6. `self.desc.size` is increased by `chunk_size`.
     (Note: `self.desc.blocks` is already updated by `get_or_alloc_block`
     internally via `alloc_and_splice_branch`, so no manual blocks increment
     is needed for the data block. However, if `get_or_alloc_block` also
     allocated indirect metadata blocks, those are accounted for too.)
  7. Proceeds to insert the new entry into the freshly allocated block.
- After insertion: updates timestamps, clears INDEX_DIR flag, persists inode.

Post (add_entry: failure):
- `Err(ENOTDIR)` if not directory.
- `Err(EINVAL)` for invalid `name`/`ino`.
- `Err(EEXIST)` if duplicate name exists.
- `Err(ENOSPC)` if `get_or_alloc_block` cannot allocate (disk full, or
  maximum file size reached for the block tree depth).
- `Err(EIO)` for I/O failures or corruption.

Invariant:
- Directories are no longer limited to 12 direct blocks (48 KiB with 4 KiB
  block size). They can grow up to the maximum file size supported by the
  ext2 indirect block tree.
- The `blocks` counter in `self.desc` is maintained by `get_or_alloc_block`
  for newly allocated blocks (both data and indirect metadata).
- Only `self.desc.size` needs manual update when a new directory block is
  appended (increased by `chunk_size`).

---

## 8.1.1  release_dir_data_blocks_for_cleanup — Full Block Tree Cleanup

Pre (release_dir_data_blocks_for_cleanup):
- `self` refers to a valid `InodeInner` (caller holds `&mut self`).
- `fs` is a live reference to the `Ext2` filesystem.
- Used on mkdir/rmdir rollback paths to release all blocks of a directory.

Post (release_dir_data_blocks_for_cleanup: success):
- **Old behavior** (phase-06): iterated `self.desc.block_ptrs[0..12]`, freed
  each non-zero block via `fs.free_blocks(ptr, 1)`, zeroed the pointer,
  then set `size = 0`, `blocks = 0`.
- **New behavior**: delegates to `self.truncate_blocks(0)` which handles the
  full indirect block tree (direct blocks, single/double/triple indirect).
  After `truncate_blocks(0)` returns:
  - All data and indirect metadata blocks are freed.
  - `self.desc.block_ptrs` entries for freed blocks are zeroed.
  - `self.desc.blocks` is decremented for each freed block.
  - Sets `self.desc.size = 0`.
- Returns `Ok(())`.

Post (release_dir_data_blocks_for_cleanup: failure):
- `Err(EIO)` if `truncate_blocks` encounters I/O errors reading indirect blocks.
- On partial failure, some blocks may already be freed; `self.desc.blocks`
  reflects the actual freed count (best-effort, matching Linux behavior).

[DIFF]
Linux: `ext2_add_link` grows directories via `ext2_get_folio(dir, n, ...)` which
  uses the page cache backed by `ext2_get_block(create=1)`. The page cache
  transparently handles block allocation through the full indirect tree.
  → Asterinas: `add_entry` calls `get_or_alloc_block(iblock, true)` directly,
  which provides the same full indirect tree support without a page cache layer.
  Reason: Asterinas ext2 currently uses direct block device I/O, not page cache.

Linux: `ext2_add_link` at line 496 iterates `n = 0..=npages` where
  `npages = dir_pages(dir)`. When `n == npages`, it accesses beyond `i_size`
  via the page cache, which triggers `ext2_get_block(create=1)` to allocate
  the new block. The `i_size` update happens in `ext2_commit_chunk`.
  → Asterinas: When `block_idx == data_blocks`, calls `get_or_alloc_block`
  to allocate, then manually updates `self.desc.size += chunk_size`.
  Reason: Without page cache, size update must be explicit.

Linux: Directory block cleanup on inode eviction goes through
  `ext2_evict_inode` → `ext2_truncate_blocks` which handles the full tree.
  → Asterinas: `release_dir_data_blocks_for_cleanup` now delegates to
  `truncate_blocks(0)` instead of the previous direct-only loop.
  Reason: Reuse existing truncation infrastructure for correctness.

Linux: `link_new_data_block` does not exist in Linux; it was an Asterinas-only
  helper that manually set `block_ptrs[iblock]` for direct blocks only.
  → Asterinas: `link_new_data_block` is removed entirely. Its role is
  subsumed by `get_or_alloc_block` which handles the full block tree.
  Reason: Eliminates the 12-block directory size limitation.

[TEST]
## add_entry (directory growth path)
- Add entry to directory with < 12 blocks → grows via direct block, succeeds
- Add entries until directory exceeds 12 blocks → 13th block allocated via
  single indirect, entry inserted successfully
- Add entry when disk is full → Err(ENOSPC) from get_or_alloc_block
- Add duplicate name → Err(EEXIST) (unchanged behavior)
- Add entry with empty name → Err(EINVAL)
- Add entry to non-directory → Err(ENOTDIR)
- Verify `self.desc.blocks` includes indirect metadata block overhead
  (e.g., after 13th data block, blocks count includes the indirect block itself)
- Verify `self.desc.size` increases by exactly `chunk_size` per new block
- Verify timestamps (ctime, mtime) updated after successful add
- Verify INDEX_DIR flag cleared after successful add
- Split existing entry with tail space → new entry placed in tail, no growth
- Reuse free slot (inode == 0) → no growth needed

## add_entry (I/O failure during growth)
- Block device write failure after get_or_alloc_block succeeds → Err(EIO),
  block remains linked in tree (will be reclaimed on inode deletion)
- Filesystem dropped (Weak upgrade fails) → Err(EIO)

## release_dir_data_blocks_for_cleanup
- Directory with only direct blocks → all freed, size=0, blocks=0
- Directory with indirect blocks (>12 data blocks) → all data and indirect
  metadata blocks freed, block_ptrs zeroed, size=0, blocks=0
- Empty directory (size=0, no blocks) → no-op, returns Ok(())
- I/O error reading indirect block during truncation → partial free,
  blocks counter reflects actual freed count

## Integration: mkdir/rmdir with large directories
- mkdir in directory with 12+ blocks → child created, parent grows correctly
- rmdir rollback: if make_empty fails after child inode allocated, cleanup
  frees child's blocks via release_dir_data_blocks_for_cleanup (including
  any indirect blocks if child had been partially populated)
- readdir on directory with indirect blocks → all entries visible across
  direct and indirect block boundaries
- find_entry across indirect block boundary → entry found correctly
- delete_entry in block beyond direct range → entry removed correctly
