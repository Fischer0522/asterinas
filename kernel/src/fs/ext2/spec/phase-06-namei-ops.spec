[PROMPT]
Provide additions to `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_create              → /root/linux/fs/ext2/namei.c:102
ext2_link                → /root/linux/fs/ext2/namei.c:204
ext2_unlink              → /root/linux/fs/ext2/namei.c:273
ext2_rename              → /root/linux/fs/ext2/namei.c:318
ext2_add_link            → /root/linux/fs/ext2/dir.c:476
ext2_delete_entry        → /root/linux/fs/ext2/dir.c:571
ext2_find_entry          → /root/linux/fs/ext2/dir.c:342
ext2_set_link            → /root/linux/fs/ext2/dir.c:450
ext2_dotdot              → /root/linux/fs/ext2/dir.c:412
ext2_empty_dir           → /root/linux/fs/ext2/dir.c:659
ext2_new_inode           → /root/linux/fs/ext2/ialloc.c:419
ext2_free_inode          → /root/linux/fs/ext2/ialloc.c:79
EXT2_DIR_REC_LEN         → /root/linux/include/linux/ext2_fs.h:295

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::dir::{DirEntry, DirEntryIter};
```

```rust
use super::fs::{Ext2, ROOT_INO};
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
    ctime: UnixTime,
    mtime: UnixTime,
    dtime: UnixTime,
    flags: FileFlags,
    block_ptrs: [u32; 15],
}
```

```rust
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
impl InodeInner {
    pub(super) fn add_entry(&mut self, name: &str, ino: u32, file_type: DirEntryFileType) -> Result<()>;
    pub(super) fn delete_entry(&mut self, name: &str) -> Result<()>;
    pub(super) fn find_entry(&self, name: &str) -> Result<u32>;
    pub(super) fn empty_dir(&self) -> bool;
    pub(super) fn get_block(&self, iblock: u32) -> Result<Option<Bid>>;
}
```

```rust
impl Ext2 {
    pub(super) fn create_inode(&self, parent_ino: u32, inode_type: InodeType, perm: FilePerm) -> Result<Arc<Inode>>;
    pub(super) fn read_inode(&self, ino: u32) -> Result<Arc<Inode>>;
    pub(super) fn free_inode(&self, ino: u32) -> Result<()>;
    pub(super) fn write_inode_desc(&self, ino: u32, raw: &RawInode) -> Result<()>;
    pub fn sync_metadata(&self) -> Result<()>;
}
```

[GUARANTEE]
impl Inode {
    /// Creates a child inode and directory entry under this directory.
    ///
    /// # Arguments
    /// * `name` - Child name, byte length in `[1, 255]`.
    /// * `type_` - Child inode type to create.
    /// * `perm` - Child permission bits merged into ext2 mode.
    ///
    /// # Returns
    /// * `Ok(Arc<Inode>)` - Created child inode handle.
    /// * `Err(ENOTDIR)` - `self` is not a directory.
    /// * `Err(EEXIST)` - `name` already exists in this directory.
    /// * `Err(EINVAL)` - Invalid name/type for this phase.
    /// * `Err(ENOSPC)` - No inode/data block available.
    /// * `Err(EIO)` - Metadata/data I/O failure.
    pub(super) fn create(&self, name: &str, type_: InodeType, perm: FilePerm) -> Result<Arc<Inode>>;

    /// Adds a hard link in this directory to an existing inode.
    ///
    /// # Arguments
    /// * `old` - Existing inode to be linked.
    /// * `name` - New directory entry name in this directory.
    ///
    /// # Returns
    /// * `Ok(())` - Hard link added.
    /// * `Err(ENOTDIR)` - `self` is not a directory.
    /// * `Err(EEXIST)` - `name` already exists.
    /// * `Err(EPERM)` - `old` is a directory.
    /// * `Err(EINVAL)` - Cross-filesystem link or invalid name.
    /// * `Err(EIO)` - Metadata/data I/O failure.
    pub(super) fn link(&self, old: &Arc<Inode>, name: &str) -> Result<()>;

    /// Removes a non-directory name from this directory.
    ///
    /// # Arguments
    /// * `name` - Child entry name to remove.
    ///
    /// # Returns
    /// * `Ok(())` - Entry removed and inode link count updated.
    /// * `Err(ENOTDIR)` - `self` is not a directory.
    /// * `Err(ENOENT)` - Entry not found.
    /// * `Err(EISDIR)` - Target entry is a directory.
    /// * `Err(EINVAL)` - Invalid name (`""`, `"."`, `".."`, too long).
    /// * `Err(EIO)` - Metadata/data I/O failure.
    pub(super) fn unlink(&self, name: &str) -> Result<()>;

    /// Renames or moves an entry from this directory to `target` directory.
    ///
    /// # Arguments
    /// * `old_name` - Existing entry name in `self`.
    /// * `target` - Destination parent directory inode.
    /// * `new_name` - Destination entry name in `target`.
    ///
    /// # Returns
    /// * `Ok(())` - Rename/move completes.
    /// * `Err(ENOTDIR)` - `self`/`target` is not a directory, or dir/file replacement mismatch.
    /// * `Err(ENOENT)` - `old_name` not found.
    /// * `Err(ENOTEMPTY)` - Replacing a non-empty target directory.
    /// * `Err(EISDIR)` - Invalid `old_name`/`new_name` (`.` or `..`).
    /// * `Err(EINVAL)` - Cross-filesystem rename or invalid arguments.
    /// * `Err(EIO)` - Metadata/data I/O failure.
    pub(super) fn rename(&self, old_name: &str, target: &Arc<Inode>, new_name: &str) -> Result<()>;

    /// Rewrites an existing directory entry to point to a new inode.
    ///
    /// Linux intent alignment: `ext2_set_link`.
    ///
    /// # Arguments
    /// * `name` - Existing entry name to rewrite in this directory.
    /// * `new_ino` - Inode number that the entry should point to after rewrite.
    /// * `file_type` - ext2 dirent file type value to store for this entry.
    /// * `update_times` - Whether to update this directory's ctime/mtime like Linux `update_times=true` path.
    ///
    /// # Returns
    /// * `Ok(())` - Entry is rewritten and persisted.
    /// * `Err(ENOTDIR)` - `self` is not a directory.
    /// * `Err(ENOENT)` - Target entry `name` does not exist.
    /// * `Err(EINVAL)` - Invalid `name` or `new_ino` range.
    /// * `Err(EIO)` - Directory stream corruption or I/O failure.
    pub(super) fn set_link(
        &self,
        name: &str,
        new_ino: u32,
        file_type: DirEntryFileType,
        update_times: bool,
    ) -> Result<()>;
}

[SPECIFICATION]
Pre (create):
- `self` refers to a valid inode; this phase requires `self.type_ == InodeType::Dir`.
- `name` is non-empty, `name.len() <= 255`, and `name != "." && name != ".."`.
- For Phase 6.3, supported create targets are `InodeType::File` and `InodeType::Dir` only.

Post (create: success, file):
- Mirrors Linux `ext2_create` logical intent with Asterinas primitives:
  1. Allocates child inode through `fs.create_inode(parent_ino, InodeType::File, perm)`.
  2. Adds parent entry with `add_entry(name, child_ino, DirEntryFileType::File)`.
  3. Child inode is returned and reachable by `find_entry(name) == child_ino`.
- Child link count is 1.

Post (create: success, directory):
- Mirrors Linux `ext2_mkdir` logical intent by delegating to existing directory-create state machine:
  - parent link reservation,
  - child inode allocation,
  - `make_empty`-equivalent `.`/`..` initialization,
  - parent directory entry insertion,
  - metadata persistence.
- Child link count is 2; parent link count increases by exactly 1.

Post (create: failure):
- `Err(ENOTDIR)` when parent is not a directory.
- `Err(EEXIST)` when `name` already exists.
- `Err(EINVAL)` for invalid `name`/unsupported `type_` in this phase.
- `Err(ENOSPC)` for inode or block exhaustion.
- `Err(EIO)` for metadata/data I/O failures.
- Rollback requirement: if failure occurs after inode allocation, no leaked inode bits/data blocks/dir entries remain.

Pre (link):
- `self.type_ == InodeType::Dir`.
- `name` is non-empty, `name.len() <= 255`, and `name != "." && name != ".."`.
- `old.type_ != InodeType::Dir`.
- `self` and `old` belong to the same Ext2 instance.

Post (link: success):
- Mirrors Linux `ext2_link` intent:
  1. Increments `old` link count by 1 before publish.
  2. Updates `old` ctime.
  3. Inserts `name -> old.ino()` into parent dir stream via `add_entry`.
  4. Persists inode and fs metadata.
- Final state: `find_entry(name) == old.ino()` and `old.nlink == old.nlink_before + 1`.

Post (link: failure):
- `Err(ENOTDIR)` if parent is not a directory.
- `Err(EEXIST)` if `name` already exists.
- `Err(EPERM)` if `old` is a directory.
- `Err(EINVAL)` for invalid args/cross-fs link.
- `Err(EIO)` for metadata/data I/O failures.
- Rollback requirement: if `old` link count was incremented but entry insertion fails, decrement it back and persist rollback.

Pre (unlink):
- `self.type_ == InodeType::Dir`.
- `name` is non-empty, `name.len() <= 255`, and `name != "." && name != ".."`.

Post (unlink: success):
- Mirrors Linux `ext2_unlink` intent for non-directory targets:
  1. Resolves `child_ino` from `find_entry(name)`.
  2. Loads child inode and requires `child.type_ != InodeType::Dir`.
  3. Removes parent directory entry via `delete_entry(name)`.
  4. Updates child ctime and decrements child link count by 1.
  5. Persists child inode metadata.
  6. If child link count reaches 0, marks deletion time and releases inode allocation.
- Returns `Ok(())` only if all required persistence/cleanup succeed.

Post (unlink: failure):
- `Err(ENOTDIR)` if parent is not a directory.
- `Err(ENOENT)` if `name` does not exist.
- `Err(EISDIR)` if `name` resolves to a directory.
- `Err(EINVAL)` for invalid `name`.
- `Err(EIO)` for metadata/data I/O failures.
- If failure occurs before `delete_entry`, parent directory bytes remain unchanged.

Pre (rename):
- `self.type_ == InodeType::Dir` and `target.type_ == InodeType::Dir`.
- `old_name` and `new_name` are non-empty, max 255 bytes, and are not `.`/`..`.
- `self` and `target` belong to the same Ext2 instance.

Pre (set_link):
- `self.type_ == InodeType::Dir`.
- `name` is non-empty, `name.len() <= 255`, and `name != "."`.
- `new_ino` is in `[ROOT_INO, sb.total_inodes()]`.

Post (set_link: success):
- Mirrors Linux `ext2_set_link` intent in Asterinas form:
  1. Locates the existing entry `name` in this directory stream.
  2. Rewrites this entry's `inode` field to `new_ino`.
  3. Rewrites this entry's `file_type` field to `file_type`.
  4. Persists modified directory block bytes.
  5. If `update_times == true`, updates this directory `ctime/mtime` and persists inode metadata.
  6. If `update_times == false`, keeps this directory timestamps unchanged.
- Returns `Ok(())` only after required persistence succeeds.

Post (set_link: failure):
- `Err(ENOTDIR)` if not directory.
- `Err(ENOENT)` if `name` does not exist.
- `Err(EINVAL)` for invalid `name`/`new_ino`.
- `Err(EIO)` for malformed dir entries or I/O failures.
- If failure occurs before persistence, on-disk directory bytes remain unchanged.

Post (rename: success):
- Mirrors Linux `ext2_rename` intent using existing ext2 helpers:
  1. Resolves source entry `old_name` in `self` to `old_ino`.
  2. If destination `new_name` exists in `target`:
     - For directory replacement, target must be empty; otherwise `ENOTEMPTY`.
     - Replacement type must be compatible (dir↔dir or non-dir↔non-dir).
     - Replaces destination entry by calling
       `target.set_link(new_name, old_ino, moved_type, true)`
       (Linux-equivalent `ext2_set_link(..., update_times=true)` behavior).
     - Updates/decrements replaced inode link counts as needed.
  3. If destination does not exist, creates target entry `new_name -> old_ino`
     via Linux-equivalent `ext2_add_link` intent.
  4. Removes source entry `old_name` from `self`.
  5. If moving a directory across parents, updates moved directory `..` by calling
     `moved_dir.set_link("..", target.ino(), DirEntryFileType::Dir, false)`
     (Linux-equivalent `ext2_set_link(..., update_times=false)` behavior),
     and adjusts parent link counts (`self -= 1`, `target += 1`).
  6. Updates moved inode ctime and persists all touched inode/fs metadata.
- Rename to itself (`self == target` and `old_name == new_name`) is a no-op success.

Post (rename: failure):
- `Err(ENOTDIR)` for invalid parent/type mismatches.
- `Err(ENOENT)` if source entry is missing.
- `Err(ENOTEMPTY)` when replacing non-empty target directory.
- `Err(EISDIR)` for `.`/`..` name usage.
- `Err(EINVAL)` for cross-fs rename or unsupported flags/args.
- `Err(EIO)` for metadata/data I/O failures.
- Rollback requirement: no leaked inode bits and no duplicate live entries for the moved inode.

Invariant:
- Directory entry stream remains ext2-valid (`rec_len` aligned/non-zero, exact-byte name matching).
- Link count updates are balanced with entry publication/removal and rollback paths.
- On-disk inode/table and bitmap state remain mutually consistent after each successful operation.

[DIFF]
Linux: `ext2_create`/`ext2_link`/`ext2_unlink`/`ext2_rename` are dentry-based operations with quota hooks (`dquot_initialize`).
  → Asterinas: Uses name-based inode methods without dentry/quota layers.
  Reason: Current Asterinas Ext2 phase has no quota subsystem and routes namespace ops through inode objects.

Linux: `ext2_rename` updates entries in folio/pagecache chunks via `ext2_set_link` and helper folio paths.
  → Asterinas: Rewrites directory entries through block-buffer mutation + `BlockDevice` writeback.
  Reason: No folio-backed ext2 mutation path in this phase.

Linux: unlink final inode reclamation is deferred to inode eviction/orphan lifecycle.
  → Asterinas: Zero-link transition removes the inode from the main cache
  immediately, may perform fast reclaim if only the transient local reference
  remains, and otherwise leaves final reclaim to the eventual fallback path.
  Reason: This phase models deleted inodes as runtime-only `DeletionPending`
  objects without a filesystem-global weak tracking set.

## Refine Prompt
[RELY]
```rust
use ostd::sync::RwMutexWriteGuard;
```

```rust
impl Inode {
    pub(super) fn ino(&self) -> u32;
}
```

[SPECIFICATION of create/link/unlink/rename/set_link]
Pre (locking):
- Caller does not hold mutable locks on `self`, `target`, or child inodes before entering these methods.

Post (locking on success/failure):
- Every `RwMutexWriteGuard` acquired in the operation is released before return.
- No lock-order inversion is introduced when multiple inode write locks are needed.

System Algorithm (lock discipline):
- Single-directory operations (`create`, `unlink`, `set_link`): acquire only `self.inner.write()` for mutation-critical sections.
- `link`: when both parent and `old` inode mutable state are touched, acquire write locks in ascending inode number order.
- `rename`:
  - If `self` and `target` differ, acquire directory write locks by ascending directory inode number.
  - Any additional child inode write locks (moved inode or replaced inode) are acquired after parent-directory locks, also by ascending inode number.
  - Release in reverse acquisition order.
- On any error path after partial metadata change, rollback is performed while holding the same lock set needed to keep state consistent.
