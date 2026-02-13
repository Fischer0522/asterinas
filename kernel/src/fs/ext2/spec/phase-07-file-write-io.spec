[PROMPT]
Provide additions to `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_write_begin             → fs/ext2/inode.c:928
ext2_write_end               → fs/ext2/inode.c:939
ext2_write_failed            → fs/ext2/inode.c:59
ext2_setsize                 → fs/ext2/inode.c:1275
__ext2_truncate_blocks       → fs/ext2/inode.c:1172
ext2_find_shared             → fs/ext2/inode.c:1037
ext2_free_branches           → fs/ext2/inode.c:1136
ext2_free_data               → fs/ext2/inode.c:1096

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
pub(super) struct BlockPath {
    pub depth: usize,
    pub offsets: [u32; 4],
    pub boundary: u32,
}
```

```rust
impl InodeInner {
    pub(super) fn block_to_path(&self, iblock: u32) -> Result<BlockPath>;
    pub(super) fn get_block(&self, iblock: u32) -> Result<Option<Bid>>;
    pub(super) fn get_or_alloc_block(&mut self, iblock: u32, create: bool) -> Result<Option<Bid>>;
    fn persist_inode_and_sync(&self, fs: &Ext2) -> Result<()>;
}
```

```rust
impl Ext2 {
    pub fn block_device(&self) -> &dyn BlockDevice;
    pub fn block_size(&self) -> usize;
    pub(super) fn free_blocks(&self, start: u32, count: u32) -> Result<()>;
}
```

[GUARANTEE]
impl InodeInner {
    /// Writes file data starting at `offset` from the provided buffer.
    ///
    /// Linux: ext2_write_begin + ext2_write_end + ext2_get_block(create=1)
    ///
    /// # Arguments
    /// * `offset` - Byte offset within the file to start writing.
    /// * `data` - Source data to write.
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes written (always `data.len()` on success).
    /// * `Err(EISDIR)` - Inode is a directory.
    /// * `Err(ENOSPC)` - No space for block allocation.
    /// * `Err(EIO)` - I/O failure or filesystem dropped.
    pub fn write_at(&mut self, offset: usize, data: &[u8]) -> Result<usize>;

    /// Resizes the file to `new_size` bytes.
    ///
    /// Linux: fs/ext2/inode.c:1275 (ext2_setsize)
    ///
    /// # Arguments
    /// * `new_size` - Target file size in bytes.
    ///
    /// # Returns
    /// * `Ok(())` - Resize completed.
    /// * `Err(EINVAL)` - Inode type does not support resize, or fast symlink.
    /// * `Err(EPERM)` - Inode has APPEND or IMMUTABLE flags.
    /// * `Err(EIO)` - I/O failure.
    pub fn resize(&mut self, new_size: usize) -> Result<()>;

    /// Frees blocks beyond `new_size`, releasing direct blocks, partial
    /// indirect chains, and whole indirect subtrees.
    ///
    /// Linux: fs/ext2/inode.c:1172 (__ext2_truncate_blocks)
    ///
    /// # Arguments
    /// * `new_size` - The new file size; blocks beyond this are freed.
    fn truncate_blocks(&mut self, new_size: usize) -> Result<()>;

    /// Recursively frees an indirect block tree.
    ///
    /// Linux: fs/ext2/inode.c:1136 (ext2_free_branches)
    ///
    /// # Arguments
    /// * `fs` - Reference to the live `Ext2` filesystem.
    /// * `block_nr` - Physical block number of the block to free.
    /// * `depth` - Remaining depth: 0 = data block, 1 = single indirect, etc.
    fn free_branches(&mut self, fs: &Ext2, block_nr: u32, depth: u32);
}

[SPECIFICATION]

## 7.2.0  write_at — File Data Write

Pre (write_at):
- `self` refers to a valid, non-freed `InodeInner`.
- `self.fs` can be upgraded to a live `Arc<Ext2>`.
- `self.desc.block_ptrs` reflects the current in-memory block pointer state.
- `offset` is an arbitrary byte offset (may be beyond current EOF, creating a hole).
- `data` is a caller-provided byte slice of arbitrary length (may be empty).

Post (write_at: success):
- Mirrors the buffered write path of Linux `ext2_write_begin` → `ext2_get_block(create=1)`
  → `ext2_write_end`, adapted to Asterinas direct block-device I/O (no page cache).
- Algorithm:
  1. Rejects directories: if `self.desc.type_ == InodeType::Dir`, returns `Err(EISDIR)`.
  2. If `data.is_empty()`, returns `Ok(0)`.
  3. Obtains `fs = self.fs.upgrade()`, `block_size = fs.block_size()`.
  4. Computes `end = offset + data.len()` (the byte position after the last written byte).
  5. Iterates over the byte range `[offset, end)` one block at a time:
     - `iblock = current_offset / block_size` (logical block number).
     - `offset_in_block = current_offset % block_size` (byte offset within the block).
     - `bytes_this_block = min(block_size - offset_in_block, remaining)`.
     - Calls `self.get_or_alloc_block(iblock, true)` to obtain the physical block:
       - If `Ok(Some(bid))`: proceed to write.
       - If `Ok(None)`: unreachable when `create=true`; treat as `Err(EIO)`.
       - If `Err(_)`: on ENOSPC or EIO, jump to write-failed cleanup.
     - **Partial block handling** (Linux: block_write_begin prepares the folio):
       - If `offset_in_block != 0` or `bytes_this_block < block_size`:
         This is a partial block write. Read the existing block content first
         (read-modify-write), then overwrite the relevant byte range.
       - If `offset_in_block == 0` and `bytes_this_block == block_size`:
         Full block overwrite — no need to read existing content.
     - Writes the block to disk via
       `fs.block_device().write_bytes(bid.to_offset(), &block_buf)`.
     - Advances `current_offset` and `data_pos` by `bytes_this_block`.
  6. Updates inode metadata:
     - If `end as u64 > self.desc.size`:
       `self.desc.size = end as u64`.
       (Linux: generic_write_end updates i_size if pos+copied > i_size.)
     - Updates timestamps: `self.desc.mtime = now()`, `self.desc.ctime = now()`.
     - Marks `self.desc` dirty.
  7. Persists inode via `self.persist_inode_and_sync(&fs)`.
  8. Returns `Ok(data.len())`.

Post (write_at: failure):
- `Err(EISDIR)` if `self.desc.type_ == InodeType::Dir`.
- `Err(EIO)` if `self.fs.upgrade()` fails or block device I/O fails.
- `Err(ENOSPC)` if block allocation fails.
- **Write-failed cleanup** (Linux: ext2_write_failed, inode.c:59):
  If the intended write end (`end`) exceeds `self.desc.size` at the point of
  failure, truncate back to `self.desc.size` (which has NOT been updated yet,
  since size update happens only on success in step 6):
    - Call `self.truncate_blocks(self.desc.size as usize)` to free any blocks
      allocated beyond the current file size.
  This matches Linux, where `ext2_write_failed` checks `to > inode->i_size`
  and truncates to `inode->i_size` (not to the pre-write size, since i_size
  is only updated in `generic_write_end` on success).
- On mid-write I/O failure, bytes already written to earlier blocks are
  committed to disk; the error is returned with the partial state.

Invariant:
- `write_at` is a mutating operation: it may allocate blocks, modify block
  pointers, extend file size, and update timestamps.
- Sparse holes between old EOF and `offset` are created implicitly: blocks
  in the gap are not allocated until read (returning zeroes) or written.
- The `blocks` counter in `self.desc` is updated by `get_or_alloc_block`
  for each newly allocated block.
- Block device writes are aligned to `bid.to_offset() + offset_in_block`
  with exact `bytes_this_block` length for partial writes, or full-block
  writes for complete block overwrites.
- After successful completion, `self.desc.size >= end`.

---

## 7.2.1  resize — File Truncation / Extension

Pre (resize):
- `self` refers to a valid, non-freed `InodeInner`.
- `self.fs` can be upgraded to a live `Arc<Ext2>`.
- `new_size` is the target file size in bytes.

Post (resize: success):
- Mirrors Linux `ext2_setsize` (inode.c:1275), adapted to Asterinas.
- Algorithm:
  1. Validates inode type: only `File`, `Dir`, and `SymLink` are allowed.
     Otherwise returns `Err(EINVAL)`.
     (Linux: `S_ISREG || S_ISDIR || S_ISLNK` check.)

  2. Rejects fast symlinks: if `self.desc.type_ == InodeType::SymLink` and
     `self.desc.blocks == 0` and `self.desc.size <= 60`, returns `Err(EINVAL)`.
     (Linux: `ext2_inode_is_fast_symlink(inode)` → `-EINVAL`.
     Fast symlinks store data inline in `block_ptrs`; resize is meaningless.)

  3. Rejects append-only / immutable inodes: if `self.desc.flags` has
     `APPEND` or `IMMUTABLE` set, returns `Err(EPERM)`.
     (Linux: `IS_APPEND(inode) || IS_IMMUTABLE(inode)` → `-EPERM`.)

  4. Obtains `fs = self.fs.upgrade()`, `block_size = fs.block_size()`.
     Let `old_size = self.desc.size as usize`.

  5. If `new_size == old_size`, no-op — return `Ok(())`.

  6. **Zero partial tail block** (Linux: block_truncate_page, inode.c:1293):
     Called unconditionally for both extension and truncation (Linux calls
     `block_truncate_page` before `truncate_setsize` regardless of direction).
     - If `new_size` is not block-aligned (`new_size % block_size != 0`):
       - `tail_iblock = new_size / block_size`.
       - `zero_from = new_size % block_size`.
       - Calls `self.get_block(tail_iblock as u32)`:
         - If `Ok(Some(bid))`: read the block, zero bytes from `zero_from`
           to end of block, write back. On I/O error, return `Err(EIO)`.
         - If `Ok(None)`: sparse hole, nothing to zero.
     (Linux returns the error from `block_truncate_page` before proceeding.)

  7. **Extension** (`new_size > old_size`):
     - Sets `self.desc.size = new_size as u64`.
     - No block preallocation: the gap between `old_size` and `new_size` is a
       sparse hole. Blocks are allocated on demand when written.
     - Updates timestamps: `self.desc.mtime = now()`, `self.desc.ctime = now()`.
     - Marks `self.desc` dirty.
     - Persists inode.

  8. **Truncation** (`new_size < old_size`):
     (Linux: truncate_setsize + __ext2_truncate_blocks)

     a. **Update file size**:
        - `self.desc.size = new_size as u64`.
        (Linux: `truncate_setsize(inode, newsize)` sets i_size and
        truncates page cache. Asterinas has no page cache to truncate.)

     b. **Free blocks beyond new size** — `self.truncate_blocks(new_size)`.

     c. Updates timestamps: `self.desc.mtime = now()`, `self.desc.ctime = now()`.
     d. Marks `self.desc` dirty.
     e. Persists inode via `self.persist_inode_and_sync(&fs)`.

  9. Returns `Ok(())`.

Post (resize: failure):
- `Err(EINVAL)` if inode type is not File, Dir, or SymLink, or if inode is
  a fast symlink.
- `Err(EPERM)` if inode has APPEND or IMMUTABLE flags.
- `Err(EIO)` if `self.fs.upgrade()` fails, or block device I/O fails during
  tail-block zeroing or indirect block reads.
- On partial truncation failure, some blocks may already be freed; the inode
  is left in a consistent but potentially over-allocated state. The `blocks`
  counter reflects the actual freed count.

---

## 7.2.2  truncate_blocks — Free Blocks Beyond New Size

Pre (truncate_blocks):
- `self` refers to a valid, non-freed `InodeInner` (caller holds `&mut self`).
- `self.fs` can be upgraded to a live `Arc<Ext2>`.
- `new_size` is the target file size; all blocks beyond this are to be freed.

Post (truncate_blocks: success):
- Mirrors Linux `__ext2_truncate_blocks` (inode.c:1172).
- Algorithm:
  1. Obtains `fs = self.fs.upgrade()`, `block_size = fs.block_size()`.
  2. Computes `iblock = (new_size + block_size - 1) / block_size`
     (first logical block to free, i.e., ceiling division).
  3. Calls `self.block_to_path(iblock as u32)` → `offsets`, `depth`.
     If `depth == 0` (new_size encompasses all addressable blocks), return `Ok(())`.
     (Linux: `n = ext2_block_to_path(inode, iblock, offsets, NULL)`)

  4. **Case depth == 1** (truncation starts in direct range):
     (Linux: `if (n == 1) { ext2_free_data(...); goto do_indirects; }`)
     For each `i` in `offsets[0]..12`:
       If `self.desc.block_ptrs[i] != 0`:
         Free the block via `fs.free_blocks(ptr, 1)`.
         Set `self.desc.block_ptrs[i] = 0`.
         Decrement `self.desc.blocks` by `block_size / SECTOR_SIZE`.
     Then jump to step 6 (do_indirects).

  5. **Case depth > 1** (truncation falls within an indirect tree):
     (Linux: `partial = ext2_find_shared(...)` + free loop)

     **find_shared** (Linux: ext2_find_shared, inode.c:1037):
     i.  Compute `k = depth`. Walk backward: while `k > 1` and
         `offsets[k-1] == 0`, decrement `k`.
         (Linux: `for (k = depth; k > 1 && !offsets[k-1]; k--)`)
         This skips trailing zero offsets — if the truncation point is
         at the start of an indirect block, we don't need to descend.
     ii. Call `get_branch` with depth `k` and `offsets[0..k]` → `branch`.
         Let `partial` point to the deepest level reached.
         If the chain is complete (`partial_level == k`), set
         `partial = k - 1`.
         (Linux: `partial = ext2_get_branch(inode, k, offsets, chain, &err)`)
     iii. Detach the top of the branch to free:
          - `nr = chain[partial].key` (the block number to detach).
          - Set the pointer slot to 0 (in inode block_ptrs or in the
            parent indirect block buffer) and write back if needed.
          (Linux: `*top = *p->p; *p->p = 0`)
     iv. Free the detached subtree:
         `self.free_branches(fs, nr, (depth - 1) - partial)`.
         (Linux: `ext2_free_branches(inode, &nr, &nr+1, ...)`)
     v.  Walk back up from `partial` to level 1, freeing the tail of
         each indirect block (entries after the truncation offset to
         end of block):
         For each level from `partial` down to 1:
           Free all non-zero entries from `offsets[level] + 1` to
           `block_size / 4` in that indirect block via `free_branches`
           with depth `(depth - 1) - level`.
           Write the modified indirect block back to disk.
         (Linux: `while (partial > chain) { ext2_free_branches(...); }`)

  6. **do_indirects** — Free whole indirect subtrees beyond the
     truncation point (Linux: `do_indirects:` switch/fallthrough):
     Using `offsets[0]` to determine which indirect levels are entirely
     beyond the truncation point:
     - If `offsets[0] < EXT2_IND_BLOCK (12)`:
       If `self.desc.block_ptrs[12] != 0`:
         `self.free_branches(fs, self.desc.block_ptrs[12], 1)`.
         Set `self.desc.block_ptrs[12] = 0`.
       Fallthrough:
     - If `offsets[0] <= EXT2_IND_BLOCK (12)`:
       If `self.desc.block_ptrs[13] != 0`:
         `self.free_branches(fs, self.desc.block_ptrs[13], 2)`.
         Set `self.desc.block_ptrs[13] = 0`.
       Fallthrough:
     - If `offsets[0] <= EXT2_DIND_BLOCK (13)`:
       If `self.desc.block_ptrs[14] != 0`:
         `self.free_branches(fs, self.desc.block_ptrs[14], 3)`.
         Set `self.desc.block_ptrs[14] = 0`.

  7. Returns `Ok(())`.

Post (truncate_blocks: failure):
- `Err(EIO)` if `self.fs.upgrade()` fails or indirect block reads fail.
- On partial failure, some blocks may already be freed; `self.desc.blocks`
  reflects the actual freed count.

---

## 7.2.3  free_branches — Recursive Indirect Block Freeing

Pre (free_branches):
- `block_nr` is a valid physical block number.
- `depth` indicates the remaining depth: 0 = data block, 1 = single indirect, etc.
- `fs` is a live reference to the `Ext2` filesystem.

Post (free_branches: success):
- Mirrors Linux `ext2_free_branches` (inode.c:1136).
- If `depth == 0`: frees `block_nr` as a data block via `fs.free_blocks(block_nr, 1)`.
  Decrements `self.desc.blocks` by `block_size / SECTOR_SIZE`.
- If `depth > 0`:
  - Reads the indirect block at `block_nr`.
  - For each non-zero entry in the block (up to `block_size / 4` entries):
    Recursively calls `self.free_branches(fs, entry, depth - 1)`.
  - Frees the indirect block itself via `fs.free_blocks(block_nr, 1)`.
  - Decrements `self.desc.blocks` by `block_size / SECTOR_SIZE`.

Post (free_branches: failure):
- On read failure of an indirect block, logs the error and continues
  freeing remaining entries (best-effort, matching Linux behavior).

[DIFF]
Linux: `ext2_write_begin` / `ext2_write_end` operate through the VFS page cache,
  preparing folios and marking them dirty for writeback.
  → Asterinas: `write_at` performs synchronous read-modify-write directly to
  the block device. No page cache integration for data writes.
  Reason: Current phase uses direct block device I/O. Page cache integration
  is specified separately in `spec/page_cache/2-inode-data-page-cache.spec`.

Linux: `ext2_write_failed` calls `truncate_pagecache` + `ext2_truncate_blocks`
  to clean up failed extending writes, truncating to `inode->i_size`.
  → Asterinas: On write failure during extension, blocks beyond `self.desc.size`
  are freed via `truncate_blocks(self.desc.size)`. No page cache to invalidate.
  Reason: Same logical cleanup; `self.desc.size` has not been updated yet at
  the point of failure (size update is step 6, after the write loop), so this
  is equivalent to Linux truncating to `i_size`.

Linux: `ext2_setsize` calls `inode_dio_wait` to drain direct I/O before truncation.
  → Asterinas: No direct I/O or DIO infrastructure; not needed.

Linux: `ext2_setsize` calls `block_truncate_page` before `truncate_setsize`
  unconditionally (for both extension and truncation). On extension, this
  zeros the tail of the last block at the old size boundary.
  → Asterinas: `resize` calls tail-block zeroing unconditionally before
  updating size, matching Linux behavior.
  Reason: Faithful replication. Zeroing on extension ensures that data
  between old EOF and the end of its block reads as zero after the extension.

Linux: `__ext2_truncate_blocks` uses `truncate_mutex` to serialize against
  concurrent `ext2_get_blocks` callers.
  → Asterinas: The caller holds `&mut self` (exclusive write lock on
  `InodeInner`), providing equivalent serialization.
  Reason: Asterinas's coarse-grained `RwMutex<InodeInner>` subsumes the
  fine-grained `truncate_mutex`.

Linux: `ext2_find_shared` acquires `i_meta_lock` (write lock) to atomically
  detach the top of the branch being freed, and checks for concurrent
  allocation that may have filled in a previously-zero pointer (`!partial->key
  && *partial->p`). It also uses `all_zeroes` to walk back up the chain to
  find the highest indirect block that can be completely freed.
  → Asterinas: The caller holds `&mut self` (exclusive write lock on
  `InodeInner`), so no concurrent allocation can occur during truncation.
  The `i_meta_lock` acquisition and the `!partial->key && *partial->p` race
  check are unnecessary. The `all_zeroes` optimization is preserved: walk
  back up to find the highest fully-freeable indirect block.
  Reason: Asterinas's coarse-grained `RwMutex<InodeInner>` prevents the
  allocation-during-truncation race that `ext2_find_shared` guards against.

Linux: `ext2_free_data` coalesces contiguous block ranges before calling
  `ext2_free_blocks` for efficiency.
  → Asterinas: Frees blocks one at a time via `fs.free_blocks(ptr, 1)`.
  Reason: Simplicity for initial implementation; coalescing is an optimization.

Linux: Truncation updates `i_blocks` via `mark_inode_dirty` after each
  `ext2_free_blocks` / `ext2_free_branches` call.
  → Asterinas: Decrements `self.desc.blocks` inline after each free, persists
  once at the end.
  Reason: Single persistence point is sufficient with exclusive InodeInner lock.
