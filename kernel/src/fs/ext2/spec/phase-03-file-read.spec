[PROMPT]
Provide additions to `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_file_read_iter        → fs/ext2/file.c:283
ext2_read_folio             → fs/ext2/inode.c:917
ext2_get_block              → fs/ext2/inode.c:783

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::fs::Ext2;
```

```rust
use crate::fs::utils::CachePage;
```

```rust
/// The Ext2 inode (in-memory).
#[derive(Debug)]
pub struct Inode {
    /// Inode type.
    type_: InodeType,
    /// File size in bytes.
    size: u64,
    /// Block pointers (i_block).
    block_ptrs: [u32; 15],
    /// Owning filesystem.
    fs: Weak<Ext2>,
}
```

[GUARANTEE]
impl Inode {
    /// Reads file data at the given byte offset.
    ///
    /// # Arguments
    /// * `offset` - Byte offset within the file.
    /// * `writer` - Destination buffer writer.
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes read.
    /// * `Err(EISDIR)` - `self` is a directory.
    /// * `Err(EIO)` - I/O failure or invalid block mapping.
    pub(super) fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize>;

    /// Reads a full block-sized page for page cache.
    ///
    /// # Arguments
    /// * `page_idx` - Block/page index within the file (block size == PAGE_SIZE).
    /// * `page` - Target cache page to fill.
    ///
    /// # Returns
    /// * `Ok(())` - Page filled with data (zero-filled for holes/EOF tail).
    /// * `Err(EISDIR)` - `self` is a directory.
    /// * `Err(EIO)` - I/O failure or invalid block mapping.
    pub(super) fn read_page(&self, page_idx: usize, page: &CachePage) -> Result<()>;
}

[SPECIFICATION]
Pre (read_at):
- `offset` is a byte offset within the file.

Post (read_at: success):
- If `self.type_` is `InodeType::Dir`, returns `Err(EISDIR)`.
- If `offset >= self.size`, returns `Ok(0)`.
- Computes `read_len = min(writer.avail(), self.size - offset)`.
- Iterates over file blocks covering `[offset, offset + read_len)`:
  - `block_size = fs.block_size()`.
  - `block_idx = cur_off / block_size`, `block_off = cur_off % block_size`.
  - `chunk = min(block_size - block_off, remaining)`.
  - Calls `self.get_block(block_idx)` to map logical to physical block.
    - If `None`, writes `chunk` zero bytes into `writer` and advances.
    - If `Some(bid)`, reads the full block from `fs.block_device()` and copies the
      `[block_off, block_off + chunk)` slice into `writer`.
- Returns `Ok(read_len)` after all chunks are consumed.

Post (read_at: failure):
- Returns `Err(EIO)` if any block I/O fails or `self.fs` cannot be upgraded.

Pre (read_page):
- `page_idx` is a block index (page size == `BLOCK_SIZE`).

Post (read_page: success):
- If `self.type_` is `InodeType::Dir`, returns `Err(EISDIR)`.
- Computes `block_size = fs.block_size()` and `page_offset = page_idx * block_size`.
- If `page_offset >= self.size`, fills `page` with zeros and returns `Ok(())`.
- Otherwise, maps `page_idx` using `self.get_block(page_idx)`:
  - If `None`, fills `page` with zeros.
  - If `Some(bid)`, reads the full block into `page`.
- If `page_offset + block_size > self.size`, zero-fills the tail beyond EOF.
- Returns `Ok(())` on success.

Post (read_page: failure):
- Returns `Err(EIO)` if any block I/O fails or `self.fs` cannot be upgraded.

Invariant:
- Unmapped blocks (sparse regions) are read as zeroes.

[DIFF]
Linux: Uses `generic_file_read_iter` and page cache readahead with `ext2_read_folio`.
  → Asterinas: Direct block reads for now; PageCache backend is a thin wrapper over block reads.
  Reason: Asterinas does not yet have ext2 page cache wiring or readahead.

Linux: Supports DAX and O_DIRECT fast paths in `ext2_file_read_iter`.
  → Asterinas: No DAX/O_DIRECT in this phase.
  Reason: DAX and direct I/O are out of scope and blocked on VFS support.
