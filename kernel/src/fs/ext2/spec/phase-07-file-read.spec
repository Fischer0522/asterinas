[PROMPT]
Provide additions to `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_get_blocks (read path) → fs/ext2/inode.c:624
ext2_read_folio            → fs/ext2/inode.c:917
ext2_file_read_iter        → (generic_file_read_iter via ext2_aops)

[RELY]
```rust
use super::prelude::*;
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

[GUARANTEE]
impl InodeInner {
    /// Reads file data into the provided buffer starting at `offset`.
    ///
    /// # Arguments
    /// * `offset` - Byte offset within the file to start reading.
    /// * `buf` - Destination buffer to fill with file data.
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes actually read (may be less than `buf.len()`
    ///   if offset is near or past EOF).
    /// * `Err(EISDIR)` - `self` is a directory (use readdir instead).
    /// * `Err(EIO)` - Block device I/O failure or filesystem reference lost.
    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize>;
}

[SPECIFICATION]
Pre (read_at):
- `self` refers to a valid, non-freed `InodeInner`.
- `self.fs` can be upgraded to a live `Arc<Ext2>`.
- `self.desc.block_ptrs` reflects the current on-disk `i_block` values.
- `offset` is an arbitrary byte offset (may be beyond EOF).
- `buf` is a caller-provided mutable byte slice of arbitrary length (may be empty).

Post (read_at: success):
- Mirrors the read-only data path of Linux `generic_file_read_iter` → `ext2_read_folio` →
  `ext2_get_block` (with `create = 0`), adapted to Asterinas direct block-device I/O.
- Algorithm:
  1. Rejects directories: if `self.desc.type_ == InodeType::Dir`, returns `Err(EISDIR)`.
  2. Obtains `file_size = self.desc.size` as `usize`.
  3. If `offset >= file_size` or `buf.is_empty()`, returns `Ok(0)` (nothing to read).
  4. Computes `read_len = min(buf.len(), file_size - offset)` — the effective byte count
     clamped to EOF.
  5. Obtains `block_size = fs.block_size()`.
  6. Iterates over the byte range `[offset, offset + read_len)` one block at a time:
     - `iblock = current_offset / block_size` (logical block number).
     - `offset_in_block = current_offset % block_size` (byte offset within the block).
     - `bytes_this_block = min(block_size - offset_in_block, remaining)`.
     - Calls `self.get_block(iblock)` to resolve the physical block:
       - If `Ok(Some(bid))`: reads `bytes_this_block` bytes from the block device at
         `bid.to_offset() + offset_in_block` into the corresponding slice of `buf`.
       - If `Ok(None)`: the block is a sparse hole; fills the corresponding slice of
         `buf` with zeroes (Linux sparse file semantics).
       - If `Err(_)`: propagates the error immediately.
     - Advances `current_offset` and `buf_pos` by `bytes_this_block`.
  7. Returns `Ok(read_len)` after all bytes are transferred.

Post (read_at: failure):
- `Err(EISDIR)` if `self.desc.type_ == InodeType::Dir`.
- `Err(EIO)` if `self.fs.upgrade()` fails (filesystem dropped).
- `Err(EIO)` if `BlockDevice::read_bytes` fails for any data block.
- Propagates any error from `get_block` (typically `Err(EIO)` or `Err(EINVAL)`).
- On mid-read I/O failure, `buf` contents up to the failing block are undefined;
  the error is returned immediately without partial-read count.

Invariant:
- `read_at` is a pure read-only operation: no inode metadata, block pointers, or on-disk
  state is modified. `self.desc` is not marked dirty.
- Sparse holes (blocks where `get_block` returns `Ok(None)`) yield zeroes, matching
  POSIX sparse file read semantics.
- The returned byte count never exceeds `buf.len()` and never exceeds `file_size - offset`.
- Block device reads are aligned to `bid.to_offset() + offset_in_block` with exact
  `bytes_this_block` length; no full-block buffer allocation is required when a partial
  block read suffices.

[DIFF]
Linux: File reads go through the page cache via `generic_file_read_iter` → `ext2_read_folio` →
  `mpage_read_folio` → `ext2_get_block`, which maps logical blocks to physical blocks and
  fills page cache folios. Subsequent reads of the same data hit the page cache.
  → Asterinas: Reads bypass the page cache entirely. Each `read_at` call performs direct
  `BlockDevice::read_bytes` for every block in the requested range.
  Reason: Current Asterinas Ext2 phase has no page cache integration. All data I/O uses
  direct block device access into caller-provided or stack/heap buffers.

Linux: `ext2_get_blocks` supports multi-block contiguous mapping (`maxblocks > 1`) and
  returns the count of contiguous physical blocks for readahead optimization.
  → Asterinas: `get_block` maps one logical block at a time. No multi-block coalescing
  or readahead is performed.
  Reason: Without page cache or readahead infrastructure, single-block mapping is sufficient.

Linux: `ext2_get_block` with `create = 1` allocates blocks for write paths.
  → Asterinas: `read_at` uses `get_block` in read-only mode only (`create = 0` equivalent).
  Block allocation for writes is out of scope for this module.

Linux: Read operations update `atime` via `file_accessed()` / `touch_atime()`.
  → Asterinas: `read_at` does not update `atime`.
  Reason: atime update policy (noatime, relatime, strictatime) is not yet integrated.
