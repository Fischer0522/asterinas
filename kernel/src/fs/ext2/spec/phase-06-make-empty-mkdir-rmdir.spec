[PROMPT]
Provide additions to `kernel/src/fs/ext2/fs.rs` and `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_make_empty          → /root/linux/fs/ext2/dir.c:617
ext2_empty_dir           → /root/linux/fs/ext2/dir.c:659
ext2_mkdir               → /root/linux/fs/ext2/namei.c:228
ext2_unlink              → /root/linux/fs/ext2/namei.c:273
ext2_rmdir               → /root/linux/fs/ext2/namei.c:302
ext2_find_entry          → /root/linux/fs/ext2/dir.c:342
ext2_add_link            → /root/linux/fs/ext2/dir.c:476
ext2_delete_entry        → /root/linux/fs/ext2/dir.c:571
ext2_new_inode           → /root/linux/fs/ext2/ialloc.c:419
EXT2_DIR_REC_LEN         → /root/linux/include/linux/ext2_fs.h:295

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
#[derive(Clone, Copy, Debug)]
pub struct FilePerm(u16);
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
    size: u64,
    links_count: u16,
    blocks: u32,
    flags: FileFlags,
    block_ptrs: [u32; 15],
}
```

```rust
impl InodeInner {
    pub(super) fn add_entry(&mut self, name: &str, ino: u32, file_type: DirEntryFileType) -> Result<()>;
    pub(super) fn delete_entry(&mut self, name: &str) -> Result<()>;
    pub(super) fn find_entry(&self, name: &str) -> Result<u32>;
    pub(super) fn get_block(&self, iblock: u32) -> Result<Option<Bid>>;
}
```

```rust
impl Ext2 {
    pub(super) fn create_inode(&self, parent_ino: u32, inode_type: InodeType, perm: FilePerm) -> Result<Arc<Inode>>;
    pub(super) fn alloc_inode(&self, parent_ino: u32, inode_type: InodeType) -> Result<u32>;
    pub(super) fn free_inode(&self, ino: u32) -> Result<()>;
    pub(super) fn alloc_blocks(&self, count: u32) -> Result<Range<u32>>;
    pub(super) fn free_blocks(&self, start: u32, count: u32) -> Result<()>;
    pub(super) fn read_inode(&self, ino: u32) -> Result<Arc<Inode>>;
    pub(super) fn write_inode_desc(&self, ino: u32, raw: &RawInode) -> Result<()>;
    pub fn sync_metadata(&self) -> Result<()>;
}
```

[GUARANTEE]
impl Ext2 {
    /// Allocates and initializes a new inode (ext2_new_inode equivalent).
    ///
    /// # Arguments
    /// * `parent_ino` - Parent inode number used for group selection.
    /// * `inode_type` - Inode type to create.
    /// * `perm` - Permission bits merged into mode type bits.
    ///
    /// # Returns
    /// * `Ok(Arc<Inode>)` - Allocated and initialized inode.
    /// * `Err(EINVAL)` - Invalid input arguments.
    /// * `Err(ENOSPC)` - No free inode.
    /// * `Err(EIO)` - Bitmap or inode-table I/O failure.
    pub(super) fn create_inode(&self, parent_ino: u32, inode_type: InodeType, perm: FilePerm) -> Result<Arc<Inode>>;
}

impl InodeInner {
    /// Initializes a newly allocated directory inode with `.` and `..` entries.
    ///
    /// # Arguments
    /// * `parent_ino` - Parent directory inode number written into `..` entry.
    ///
    /// # Returns
    /// * `Ok(())` - First directory chunk is initialized and persisted.
    /// * `Err(ENOTDIR)` - `self` is not a directory inode.
    /// * `Err(EINVAL)` - `parent_ino` is out of valid inode range.
    /// * `Err(ENOSPC)` - Data block allocation failed due no space.
    /// * `Err(EIO)` - Metadata/data I/O failure or layout corruption.
    pub(super) fn make_empty(&mut self, parent_ino: u32) -> Result<()>;

    /// Checks whether this directory contains only `.` and `..` as live entries.
    ///
    /// # Returns
    /// * `true` - Directory is empty by ext2 `rmdir` rule.
    /// * `false` - Directory has extra live entries, malformed entries, or read failure.
    pub(super) fn empty_dir(&self) -> bool;

    /// Creates a subdirectory under this directory inode.
    ///
    /// # Arguments
    /// * `name` - Child directory name, byte length in `[1, 255]`.
    /// * `perm` - Permission bits for child inode mode (`S_IFDIR | perm`).
    ///
    /// # Returns
    /// * `Ok(Arc<Inode>)` - Newly created child directory inode handle.
    /// * `Err(ENOTDIR)` - Parent is not a directory.
    /// * `Err(EEXIST)` - `name` already exists in parent directory.
    /// * `Err(EINVAL)` - Invalid `name` (empty/too long or reserved `.`/`..`).
    /// * `Err(ENOSPC)` - No free inode or no free data block.
    /// * `Err(EIO)` - Metadata/data I/O failure.
    pub(super) fn mkdir(&mut self, name: &str, perm: FilePerm) -> Result<Arc<Inode>>;

    /// Removes an existing empty subdirectory from this parent directory.
    ///
    /// # Arguments
    /// * `name` - Child directory name to remove.
    ///
    /// # Returns
    /// * `Ok(())` - Entry removed and child inode released.
    /// * `Err(ENOTDIR)` - Parent is not a directory, or target is not a directory.
    /// * `Err(ENOENT)` - `name` not found in parent directory.
    /// * `Err(ENOTEMPTY)` - Target directory is not empty by ext2 rule.
    /// * `Err(EINVAL)` - Invalid `name` (empty/too long or reserved `.`/`..`).
    /// * `Err(EIO)` - Metadata/data I/O failure.
    pub(super) fn rmdir(&mut self, name: &str) -> Result<()>;
}

[SPECIFICATION]
Pre (create_inode):
- `parent_ino` is in `[ROOT_INO, sb.total_inodes()]`.
- `inode_type != InodeType::Unknown`.

Post (create_inode: success):
- Follows Linux `ext2_new_inode` intent for allocation + initialization:
  1. Allocates inode number via `alloc_inode(parent_ino, inode_type)`.
  2. Initializes on-disk descriptor fields before publishing the inode:
     - mode combines `inode_type` bits and `perm` bits,
     - `size = 0`, `blocks = 0`, `block_ptrs = [0; 15]`,
     - `links_count = 2` for directories, else `links_count = 1`.
  3. Persists descriptor via `write_inode_desc`.
  4. Returns loaded handle via `read_inode(ino)`.

Post (create_inode: failure):
- Returns `Err(EINVAL)` for invalid inputs.
- Returns `Err(ENOSPC)` if allocation cannot find a free inode.
- Returns `Err(EIO)` for bitmap/inode-table I/O failures.
- If failure happens after inode bitmap allocation, rollback via `free_inode(ino)`.

Pre (make_empty):
- `self.desc.type_ == InodeType::Dir`.
- `parent_ino` is in `[1, sb.total_inodes()]`.
- `self.weak_self.upgrade()` succeeds to obtain current inode number (`self_ino`).

Post (make_empty: success):
- Follows Linux `ext2_make_empty` chunk-initialization intent:
  - `chunk_size = fs.block_size()`.
  - Allocates exactly one new data block for logical block 0.
  - Zero-fills the whole chunk before writing entries.
  - Writes entry `.` at offset `0`:
    - `inode = self_ino`
    - `name = "."`, `name_len = 1`
    - `rec_len = DirEntry::dir_rec_len(1)`
    - `file_type = Dir`.
  - Writes entry `..` immediately after `.`:
    - `inode = parent_ino`
    - `name = ".."`, `name_len = 2`
    - `rec_len = chunk_size - DirEntry::dir_rec_len(1)`
    - `file_type = Dir`.
- Persists the initialized data block to disk.
- Updates this inode descriptor:
  - `size = chunk_size`.
  - `blocks += chunk_size / SECTOR_SIZE` (i_blocks in 512-byte sectors).
- Persists inode metadata (`write_inode_desc`) and filesystem metadata (`sync_metadata`).

Post (make_empty: failure):
- `Err(ENOTDIR)` if target inode is not directory.
- `Err(EINVAL)` if inode ranges/parameters are invalid.
- `Err(ENOSPC)` if block allocation cannot satisfy one chunk.
- `Err(EIO)` on write failure or malformed layout state.
- No leaked allocation: if a new block was allocated but operation fails, block is released and inode block pointer/counters rollback to pre-state.

Pre (empty_dir):
- `self.desc.type_ == InodeType::Dir`.

Post (empty_dir):
- Scans directory entries block-by-block in ascending order (using DirEntryIter for each block), with per-block `limit = min(block_size, size - block_offset)`.
- Returns `false` immediately when:
  - any block read/parsing fails,
  - any entry has `rec_len == 0` (corrupt stream),
  - any live entry (`inode != 0`) is neither `.` nor `..`,
  - `.` entry inode is not equal to this inode number,
  - `name_len > 2`, or two-byte name not equal to `..`.
- Returns `true` only if every live entry is valid `.` or `..` and no corruption/read fault is observed.

Pre (mkdir):
- `self.desc.type_ == InodeType::Dir`.
- `name` is non-empty, `name.len() <= 255`, and `name != "." && name != ".."`.
- `self.weak_self.upgrade()` succeeds to obtain `parent_ino`.

Post (mkdir: success):
- Mirrors Linux `ext2_mkdir` state machine with Asterinas primitives:
  1. Parent link count is incremented first (reservation for child `..`).
  2. Creates child inode handle by `create_inode(parent_ino, InodeType::Dir, perm)`.
  3. Obtains `child_ino` from the returned child inode handle.
  4. Calls `make_empty(parent_ino)` for child inode, producing canonical `.`/`..` layout.
  5. Inserts parent dirent by `add_entry(name, child_ino, DirEntryFileType::Dir)`.
  6. Persists parent/child inode metadata and syncs filesystem metadata.
- Final successful state:
  - parent link count increased by exactly 1,
  - child has `links_count == 2`,
  - parent contains exactly one new dir entry `name -> child_ino`.

Post (mkdir: failure):
- `Err(ENOTDIR)` if parent is not a directory.
- `Err(EINVAL)` for invalid `name`.
- `Err(EEXIST)` when `name` already exists.
- `Err(ENOSPC)` for inode/block exhaustion.
- `Err(EIO)` for metadata/data I/O failures.
- Rollback requirement (Linux-style unwind intent):
  - if parent link count was incremented, decrement it before return;
  - if `create_inode` already returned child inode, free it via `free_inode(child_ino)`;
  - if child data block was allocated during `make_empty`, release it;
  - if parent entry insertion already happened before later failure, remove that entry;
  - no leaked inode bits, data blocks, or directory entries.

Pre (rmdir):
- `self.desc.type_ == InodeType::Dir`.
- `name` is non-empty, `name.len() <= 255`, and `name != "." && name != ".."`.

Post (rmdir: success):
- Mirrors Linux `ext2_rmdir` + `ext2_unlink` intent:
  1. Resolves target by exact name (`find_entry(name)`), obtaining `child_ino`.
  2. Loads child inode metadata and verifies `child.type_ == InodeType::Dir`.
  3. Requires `child.empty_dir() == true`; otherwise fail with `ENOTEMPTY`.
  4. Removes parent directory entry by `delete_entry(name)`.
  5. Updates child inode metadata: `size = 0`, drop `.` and `..` links (`links_count -= 2`).
  6. Updates parent inode metadata: `links_count -= 1`.
  7. Persists inode metadata and frees child inode allocation (`free_inode(child_ino)`).
  8. Syncs filesystem metadata.
- Returns `Ok(())` only when all persistence/free steps succeed.

Post (rmdir: failure):
- `Err(ENOTDIR)` if parent is not a directory or target is not a directory.
- `Err(EINVAL)` for invalid `name`.
- `Err(ENOENT)` if target name does not exist.
- `Err(ENOTEMPTY)` if target has non-dot live entries or fails `empty_dir` validation.
- `Err(EIO)` for metadata/data I/O failures.
- If failure occurs before `delete_entry`, parent directory bytes and link counts remain unchanged.

Invariant:
- Directory entry stream always respects ext2 alignment (`rec_len % 4 == 0`) and non-zero record length for valid entries.
- `make_empty` always generates canonical first-chunk layout of exactly two live entries (`.` then `..`).
- `rmdir` never removes non-empty directory trees.

## Refine Prompt
[RELY]
```rust
impl Inode {
    pub fn ino(&self) -> u32;
}
```

```rust
use ostd::sync::RwMutexWriteGuard;
```

[SPECIFICATION of mkdir/rmdir]
Pre:
- Caller holds the parent inode write lock for the full mutation sequence.
- If child inode lock is needed, lock acquisition order is parent inode number first, then child inode number (ascending inode-number order).

Post (success):
- All temporary locks acquired during child setup/check are released before return.
- Parent lock remains under caller ownership contract.

Post (failure):
- Any child/auxiliary lock acquired during operation is released on every error path before returning `Err`.
- No lock-order inversion is introduced relative to global ordering (`SuperBlock -> BlockGroup -> Inode`).

[DIFF]
Linux: `ext2_mkdir`/`ext2_rmdir` are VFS+dentry operations and include quota initialization (`dquot_initialize`).
  → Asterinas: Name-based inode methods on `InodeInner`, without dentry/quota layers.
  Reason: Current Asterinas Ext2 phase has no quota subsystem and uses direct inode methods.

Linux: `ext2_make_empty` mutates folio/pagecache chunks (`ext2_prepare_chunk`/`ext2_commit_chunk`).
  → Asterinas: Mutates block-sized byte buffers and writes via `BlockDevice`.
  Reason: No folio-backed ext2 mutation path in this phase.

Linux: `ext2_empty_dir` returns boolean and treats read/corruption as “not empty” for `rmdir` gating.
  → Asterinas: Keeps boolean gate semantics (`false` on read/parse failure), then `rmdir` maps to `ENOTEMPTY`.
  Reason: Preserve Linux `rmdir` decision behavior while using Rust parsing helpers.

Linux: link count mutations use VFS helpers (`inode_inc_link_count` / `inode_dec_link_count`) and inode eviction flow.
  → Asterinas: Explicit `links_count` updates in `InodeDesc` plus `free_inode` release.
  Reason: Current implementation phase lacks full VFS inode lifecycle helpers.

Linux: `ext2_new_inode` returns an inode with initialized metadata used by higher-level ops.
  → Asterinas: `Ext2::create_inode` wraps `alloc_inode` + descriptor initialization + persistence.
  Reason: Avoid duplicating manual inode initialization at each call site.
