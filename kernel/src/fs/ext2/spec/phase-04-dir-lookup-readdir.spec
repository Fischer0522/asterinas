[PROMPT]
Provide additions to `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_find_entry             → fs/ext2/dir.c:342
ext2_readdir                → fs/ext2/dir.c:257

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
use crate::fs::utils::DirentVisitor;
```

```rust
/// The Ext2 inode (in-memory).
#[derive(Debug)]
pub struct Inode {
    /// Inode number.
    ino: u32,
    /// Inode type.
    type_: InodeType,
    /// File size in bytes.
    size: u64,
    /// Directory start hint (unused for now).
    dir_start_lookup: u32,
    /// Block pointers (i_block).
    block_ptrs: [u32; 15],
    /// Owning filesystem.
    fs: Weak<Ext2>,
}
```

```rust
/// Directory entry type mapping (ext2 file_type field).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DirEntryFileType {
    /// Unknown file type.
    Unknown = 0,
    /// Regular file.
    File = 1,
    /// Directory.
    Dir = 2,
    /// Character device.
    Char = 3,
    /// Block device.
    Block = 4,
    /// FIFO.
    Fifo = 5,
    /// Socket.
    Socket = 6,
    /// Symlink.
    Symlink = 7,
}
```

[GUARANTEE]
impl Inode {
    /// Finds a directory entry by name and returns its inode number.
    ///
    /// # Arguments
    /// * `name` - The target entry name (UTF-8).
    ///
    /// # Returns
    /// * `Ok(u32)` - The inode number of the matching entry.
    /// * `Err(ENOENT)` - Not found.
    /// * `Err(ENOTDIR)` - `self` is not a directory.
    /// * `Err(EIO)` - Directory data is corrupted or I/O fails.
    pub(super) fn find_entry(&self, name: &str) -> Result<u32>;

    /// Reads directory entries starting at byte offset and feeds visitor.
    ///
    /// # Arguments
    /// * `offset` - Byte offset within the directory file.
    /// * `visitor` - Visitor that consumes entries.
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes advanced from `offset`.
    /// * `Err(ENOTDIR)` - `self` is not a directory.
    /// * `Err(EIO)` - Directory data is corrupted or I/O fails.
    pub(super) fn readdir_at(
        &self,
        offset: usize,
        visitor: &mut dyn DirentVisitor,
    ) -> Result<usize>;
}

[SPECIFICATION]
Pre (find_entry):
- `self.type_` is `InodeType::Dir`.

Post (find_entry: success):
- Iterates over directory blocks in order (no wrap-around).
- For each block:
  - `block_size = fs.block_size()`.
  - `block_offset = block_idx * block_size`.
  - `limit = min(block_size, self.size - block_offset)`; if `limit == 0`, stop.
  - `max_inumber = sb.total_inodes()`.
  - `max_blocks = self.blocks >> 3` (i_blocks in 512-byte sectors; 4K blocks).
  - If `block_idx > max_blocks`, returns `Err(ENOENT)`.
  - Reads the data block via `self.get_block(block_idx)`; if `None`, returns `Err(EIO)`.
  - Uses `DirEntryIter` to parse entries within `[0, limit)`.
  - Skips entries with `inode == 0`.
  - If `name_len == name.len()` and bytes equal, returns `Ok(inode)`.
- If no match found, returns `Err(ENOENT)`.

Post (find_entry: failure):
- Returns `Err(ENOTDIR)` if `self.type_` is not directory.
- Returns `Err(EIO)` for invalid entry layout or I/O failures.

Pre (readdir_at):
- `self.type_` is `InodeType::Dir`.

Post (readdir_at: success):
- Let `min_rec_len = DirEntry::dir_rec_len(1)`.
- If `self.size < min_rec_len` or `offset > self.size - min_rec_len`, returns `Ok(0)`.
- Computes:
  - `block_size = fs.block_size()`.
  - `start_block = offset / block_size`.
  - `start_inner = offset % block_size`.
- Iterates blocks from `start_block` to end of directory size:
  - Computes `block_offset = block_idx * block_size` and `limit` as in `find_entry`.
  - Reads data block via `self.get_block(block_idx)`; if `None`, returns `Err(EIO)`.
  - Uses `DirEntryIter` to parse entries within `[0, limit)`.
  - Skips entries with `inode == 0`.
  - For the first block, skip entries until `entry_offset >= start_inner`.
  - For each entry, determines `type_` from `file_type`:
    - 0 → `InodeType::Unknown`.
    - 1..7 → map to the corresponding `InodeType` (File/Dir/Char/Block/Fifo/Socket/SymLink).
  - Calls `visitor.visit(name, inode as u64, type_, entry_global_offset)`.
  - If visitor returns `Err`, stops and returns bytes advanced so far.
- Returns total bytes advanced from `offset`.

Post (readdir_at: failure):
- Returns `Err(ENOTDIR)` if `self.type_` is not directory.
- Returns `Err(EIO)` for invalid entry layout or I/O failures.

Invariant:
- Entry parsing uses `DirEntryIter` rules from Module 4.1 and matches Linux `ext2_check_folio` constraints.

[DIFF]
Linux: Uses folio/page cache and `dir_context` with `ctx->pos`.
  → Asterinas: Uses direct block reads and `DirentVisitor` with byte offsets (no folio).
  Reason: Asterinas has no folio abstraction; PageCache integration comes later.

Linux: Uses `i_dir_start_lookup` and wrap-around search in `ext2_find_entry`.
  → Asterinas: Always scans from block 0 without wrap-around.
  Reason: No inode-local lookup cache yet.

Linux: Uses `EXT2_FEATURE_INCOMPAT_FILETYPE` to interpret `file_type`.
  → Asterinas: Maps `file_type` directly and treats 0 as `Unknown`.
  Reason: Feature gating not implemented in this phase.

Linux: Uses `need_revalidate` + `inode_query_iversion` and `ext2_validate_entry` to
re-sync `ctx->pos` after directory changes.
  → Asterinas: No iversion/ctx->pos mechanism; no `ext2_validate_entry` offset fixup.
  Reason: Asterinas VFS readdir API lacks per-iteration version tracking.
