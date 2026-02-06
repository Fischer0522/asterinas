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
    Unknown = 0,
    File = 1,
    Dir = 2,
    Char = 3,
    Block = 4,
    Fifo = 5,
    Socket = 6,
    Symlink = 7,
}
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
    size: u64,
    ctime: UnixTime,
    mtime: UnixTime,
    flags: FileFlags,
    blocks: u32,
    block_ptrs: [u32; 15],
}
```

```rust
impl InodeInner {
    pub(super) fn get_block(&self, iblock: u32) -> Result<Option<Bid>>;
}
```

```rust
impl Ext2 {
    pub(super) fn alloc_blocks(&self, count: u32) -> Result<Range<u32>>;
    pub(super) fn free_blocks(&self, start: u32, count: u32) -> Result<()>;
    pub(super) fn write_inode_desc(&self, ino: u32, raw: &RawInode) -> Result<()>;
    pub fn sync_metadata(&self) -> Result<()>;
}
```

[GUARANTEE]
impl InodeInner {
    /// Adds a new directory entry to this directory inode.
    ///
    /// # Arguments
    /// * `name` - Entry name (byte length in `[1, 255]`).
    /// * `ino` - Target inode number to store in the new entry (1-based).
    /// * `file_type` - On-disk ext2 directory entry file type.
    ///
    /// # Returns
    /// * `Ok(())` - Entry added successfully.
    /// * `Err(EEXIST)` - `name` already exists.
    /// * `Err(ENOSPC)` - No usable slot and directory cannot be extended.
    /// * `Err(ENOTDIR)` - Not a directory.
    /// * `Err(EINVAL)` - Invalid `name` or `ino`.
    /// * `Err(EIO)` - Corruption or I/O failure.
    pub(super) fn add_entry(&self, name: &str, ino: u32, file_type: DirEntryFileType) -> Result<()>;

    /// Deletes a directory entry by name.
    ///
    /// # Arguments
    /// * `name` - Entry name to remove.
    ///
    /// # Returns
    /// * `Ok(())` - Entry removed.
    /// * `Err(ENOENT)` - Not found.
    /// * `Err(ENOTDIR)` - Not a directory.
    /// * `Err(EINVAL)` - Invalid `name`.
    /// * `Err(EIO)` - Corruption or I/O failure.
    pub(super) fn delete_entry(&self, name: &str) -> Result<()>;
}

[SPECIFICATION]
Pre (add_entry):
- `self.desc.type_ == InodeType::Dir`.
- `name` is non-empty and `name.len() <= 255`.
- `ino` is in `[1, sb.total_inodes()]`.

Post (add_entry: success):
- Let `reclen = DirEntry::dir_rec_len(name.len())`, `chunk_size = fs.block_size()`.
- Scans directory blocks in ascending logical order (Linux `ext2_add_link` behavior):
  - Parses each block with ext2 dir entry layout checks.
  - If `rec_len == 0`, returns `Err(EIO)`.
  - If existing entry name matches, returns `Err(EEXIST)`.
  - Finds insertion slot if either:
    - unused entry (`inode == 0`) with `rec_len >= reclen`, or
    - used entry with `rec_len >= used_len + reclen` where `used_len = DirEntry::dir_rec_len(existing_name_len)`.
- If no slot in existing blocks:
  - allocates one new data block via `alloc_blocks(1)`,
  - initializes one free chunk/entry span in that block,
  - links new block into directory block pointer set,
  - updates inode size/blocks accounting in `self.desc`.
- On insertion:
  - If splitting used entry, shrink head to `used_len`, place new entry in tail.
  - Writes `inode/name_len/name/file_type/rec_len` fields.
  - Persists modified directory block.
  - Updates `self.desc.ctime` / `self.desc.mtime`.
  - Clears BTREE/DX intent bit in `self.desc.flags`.
  - Persists inode metadata (`write_inode_desc`) and syncs fs metadata (`sync_metadata`).

Post (add_entry: failure):
- `Err(ENOTDIR)` if not directory.
- `Err(EINVAL)` for invalid `name`/`ino`.
- `Err(EEXIST)` if duplicate name exists.
- `Err(ENOSPC)` if expansion not possible.
- `Err(EIO)` for malformed entries or I/O failures.

Pre (delete_entry):
- `self.desc.type_ == InodeType::Dir`.
- `name` is non-empty and `name.len() <= 255`.

Post (delete_entry: success):
- Locates target entry by exact byte name match.
- In the containing chunk:
  - finds previous entry when present,
  - merges space by extending `prev.rec_len` across removed entry span,
  - sets removed entry `inode = 0`.
- Persists modified directory block.
- Updates `self.desc.ctime` / `self.desc.mtime`.
- Clears BTREE/DX intent bit in `self.desc.flags`.
- Persists inode metadata and syncs fs metadata.
- Returns `Ok(())`.

Post (delete_entry: failure):
- `Err(ENOTDIR)` if not directory.
- `Err(EINVAL)` for invalid `name`.
- `Err(ENOENT)` if no matched entry.
- `Err(EIO)` for malformed entries (`rec_len == 0`) or I/O failures.

Invariant:
- Directory entry `rec_len` is 4-byte aligned and non-zero for valid entries.
- Entry parsing/mutation preserves ext2 directory layout constraints.
- Name matching uses exact byte equality.

[DIFF]
Linux: Uses folio/pagecache mutation (`ext2_prepare_chunk`, `ext2_commit_chunk`) and `ext2_handle_dirsync`.
  → Asterinas: Uses block-buffer mutation and direct block device writeback.
  Reason: folio infrastructure is not used in this phase.

Linux: Mutation logic is associated with ext2 inode private state in VFS inode.
  → Asterinas: Mutation logic is implemented in `InodeInner`, with `Inode` as outer handle.
  Reason: current Rust abstraction splits immutable identity and mutable inode state.

Linux: Parent locking comes from VFS contract.
  → Asterinas: serialization relies on inode-inner lock discipline.
  Reason: different VFS/locking architecture.
