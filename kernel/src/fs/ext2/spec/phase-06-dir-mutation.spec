[PROMPT]
Provide additions to `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_add_link               → fs/ext2/dir.c:476
ext2_delete_entry           → fs/ext2/dir.c:571
ext2_set_link               → fs/ext2/dir.c:450
ext2_prepare_chunk          → fs/ext2/dir.c:435
ext2_handle_dirsync         → fs/ext2/dir.c:440
EXT2_DIR_REC_LEN            → include/linux/ext2_fs.h:295

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
    /// Inode flags (includes BTREE/DX compatibility bits).
    flags: u32,
    /// Owning filesystem.
    fs: Weak<Ext2>,
}
```

```rust
impl Inode {
    /// Maps a logical block index to a physical block id.
    pub(super) fn get_block(&self, iblock: u32) -> Result<Option<Bid>>;
}
```

```rust
impl Ext2 {
    /// Allocates up to `count` contiguous blocks.
    pub(super) fn alloc_blocks(&self, count: u32) -> Result<Range<u32>>;
}
```

```rust
/// A parsed directory entry.
#[derive(Clone, Debug)]
pub(super) struct DirEntry {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
    pub name: CStr256,
}
impl DirEntry {
      pub(super) fn parse_at(
        buf: &[u8],
        offset: usize,
        limit: usize,
        max_inumber: u32,
    ) -> Result<DirEntry>;
}
/// Directory entry iterator over a single block buffer.
pub(super) struct DirEntryIter<'a> {
    buf: &'a [u8],
    offset: usize,
    limit: usize,
    max_inumber: u32,
}
```


[GUARANTEE]
impl Inode {
    /// Adds a new directory entry to this directory inode.
    ///
    /// # Arguments
    /// * `name` - Entry name (byte length in `[1, 255]`).
    /// * `ino` - Target inode number to store in the new entry (1-based).
    /// * `file_type` - On-disk ext2 directory entry file type.
    ///
    /// # Returns
    /// * `Ok(())` - Entry added successfully.
    /// * `Err(EEXIST)` - `name` already exists in this directory.
    /// * `Err(ENOSPC)` - No usable slot and directory cannot be extended.
    /// * `Err(ENOTDIR)` - `self` is not a directory.
    /// * `Err(EINVAL)` - Invalid `name` or `ino` range.
    /// * `Err(EIO)` - Directory corruption or I/O failure.
    pub(super) fn add_entry(&self, name: &str, ino: u32, file_type: DirEntryFileType) -> Result<()>;

    /// Deletes a directory entry by name.
    ///
    /// # Arguments
    /// * `name` - Entry name to remove.
    ///
    /// # Returns
    /// * `Ok(())` - Entry removed successfully.
    /// * `Err(ENOENT)` - `name` not found.
    /// * `Err(ENOTDIR)` - `self` is not a directory.
    /// * `Err(EINVAL)` - Invalid `name`.
    /// * `Err(EIO)` - Directory corruption or I/O failure.
    pub(super) fn delete_entry(&self, name: &str) -> Result<()>;
}

[SPECIFICATION]
Pre (add_entry):
- `self.type_ == InodeType::Dir`.
- `name` is non-empty and `name.len() <= 255`.
- `ino` is in `[1, sb.total_inodes()]`.

Post (add_entry: success):
- Let `reclen = DirEntry::dir_rec_len(name.len())` and `chunk_size = fs.block_size()`.
- Scans directory blocks in ascending logical order, following Linux `ext2_add_link` search rules:
  - Parses each block with ext2 directory entry layout checks.
  - If a zero-length entry (`rec_len == 0`) is observed, returns `Err(EIO)`.
  - If an existing entry matches `name`, returns `Err(EEXIST)`.
  - Finds insertion slot if either:
    - an unused entry (`inode == 0`) has `rec_len >= reclen`, or
    - a used entry has `rec_len >= used_len + reclen`, where
      `used_len = DirEntry::dir_rec_len(existing_name_len)`.
- If insertion requires directory growth, allocates one new directory data block via filesystem allocator,
  initializes a free entry spanning one `chunk_size`, and retries insertion in that block.
- On insertion:
  - If splitting a used entry, keeps head entry with `used_len` and creates tail free slot.
  - Writes new entry fields: `inode = ino`, `name_len`, `name bytes`, `file_type`, and valid `rec_len`.
  - Persists the modified directory block.
  - Updates directory metadata timestamps (`ctime`, `mtime`) and clears BTREE/DX flag intent.

Post (add_entry: failure):
- `Err(ENOTDIR)` if `self` is not directory.
- `Err(EINVAL)` for invalid `name`/`ino`.
- `Err(EEXIST)` if `name` already exists.
- `Err(ENOSPC)` if no slot exists and directory cannot be expanded.
- `Err(EIO)` for malformed entries or I/O failures.

Pre (delete_entry):
- `self.type_ == InodeType::Dir`.
- `name` is non-empty and `name.len() <= 255`.

Post (delete_entry: success):
- Locates target entry by exact name match.
- If target is found in a block chunk:
  - Identifies previous entry in the same chunk when present.
  - Merges free space like Linux `ext2_delete_entry`:
    - if previous entry exists, extends `prev.rec_len` to cover removed entry span,
    - sets removed entry `inode = 0`.
  - Persists the modified directory block.
  - Updates directory metadata timestamps (`ctime`, `mtime`) and clears BTREE/DX flag intent.
- Returns `Ok(())`.

Post (delete_entry: failure):
- `Err(ENOTDIR)` if `self` is not directory.
- `Err(EINVAL)` for invalid `name`.
- `Err(ENOENT)` if `name` not found.
- `Err(EIO)` for malformed entries (`rec_len == 0`) or I/O failures.

Invariant:
- Directory entry `rec_len` is 4-byte aligned and never zero for valid on-disk entries.
- Entry parsing and mutation preserve ext2 directory layout constraints.
- Name matching is exact byte equality on entry name bytes.

[DIFF]
Linux: Uses folio/pagecache mutation (`ext2_prepare_chunk`, `ext2_commit_chunk`) and `ext2_handle_dirsync`.
  → Asterinas: Uses block-buffer based mutation and direct device writeback in this phase.
  Reason: Folio APIs are not available; write-path PageCache integration is staged.

Linux: `ext2_add_link` receives `dentry` and extracts raw name bytes (`qstr`).
  → Asterinas: Uses `&str` API (`name`) and validates byte length constraints.
  Reason: Asterinas VFS layer currently exposes string-based lookup/create paths.

Linux: Directory mutation assumes VFS parent locking contract (`Parent is locked`).
  → Asterinas: Mutation serialization is provided by inode-level synchronization in Ext2 methods.
  Reason: Different VFS locking model and Rust ownership-based synchronization.
