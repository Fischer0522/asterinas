[PROMPT]
Phase 4.4 (directory mutations + dependent namei ops): rewire directory entry
mutations to the split-lock layout, and rewire `link`/`unlink` which depend on
the mutation primitives.

Covered ops:

- add_entry
- delete_entry
- make_empty
- mkdir
- rmdir
- create
- link
- unlink

Design decision (confirmed): directory mutations are fully serialized by holding
the directory `meta.write()` for the duration of the mutation. This blocks
concurrent `lookup` / `readdir_at`.

Provide modifications to `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_add_link           → fs/ext2/dir.c:476
ext2_delete_entry       → fs/ext2/dir.c:560
ext2_make_empty         → fs/ext2/dir.c:617
ext2_empty_dir          → fs/ext2/dir.c:659
ext2_mkdir              → fs/ext2/namei.c:228
ext2_rmdir              → fs/ext2/namei.c:294
ext2_create             → fs/ext2/namei.c:102
ext2_add_nondir         → fs/ext2/namei.c:177
ext2_link               → fs/ext2/namei.c:204
ext2_unlink              → fs/ext2/namei.c:273

[RELY]
```rust
use super::prelude::*;
use super::fs::Ext2;
```

```rust
pub(super) struct InodeInner { /* from spec 11 */ }
```

```rust
impl InodeInner {
    // Helper APIs from spec 12.
    pub(super) fn grow_dir_block(&self, meta: &mut InodeMeta, fs: &Ext2) -> Result<DirSlotInfo>;
    pub(super) fn release_dir_data_blocks_for_cleanup(&self, meta: &mut InodeMeta, fs: &Ext2) -> Result<()>;

    // Persistence API from spec 11.
    pub(super) fn persist_inode_locked(meta: &mut InodeMeta, mapping: &mut InodeMapping, ino: u32, type_: InodeType, fs: &Ext2) -> Result<()>;
}
```

[GUARANTEE]

```rust
impl Inode {
    pub(super) fn add_entry(&self, name: &str, ino: u32, file_type: DirEntryFileType) -> Result<()>;
    pub(super) fn delete_entry(&self, name: &str) -> Result<()>;
    pub(super) fn make_empty(&self, parent_ino: u32) -> Result<()>;

    pub(super) fn mkdir(&self, name: &str, perm: FilePerm) -> Result<Arc<Inode>>;
    pub(super) fn rmdir(&self, name: &str) -> Result<()>;
    pub(super) fn create(&self, name: &str, type_: InodeType, perm: FilePerm) -> Result<Arc<Inode>>;

    pub(super) fn link(&self, old: &Inode, name: &str) -> Result<()>;
    pub(super) fn unlink(&self, name: &str) -> Result<()>;
}
```

[SPECIFICATION]

## Common directory mutation locking

For `add_entry`, `delete_entry`, `make_empty`, and the parent-directory part of
`mkdir/rmdir/create`:

- Hold directory `meta.write()` for the duration.
- Take `mapping.write()` only around:
  - block allocation (`get_or_alloc_block` via mapping)
  - truncation rollback
  - final persistence (`persist_inode_locked`)
- Never hold `mapping.write()` while calling PageCache/VMO methods.

Directory mutation must explicitly update directory timestamps and flags:

- Update `mtime` + `ctime`.
- Clear `INDEX_DIR`.

Then persist via `persist_inode_locked`.

## add_entry

Behavior:
- Validate directory inode type and name bounds.
- Scan directory blocks through PageCache to find a reusable slot or detect
  duplicate name (EEXIST).
- If no slot exists, grow directory by one block (exactly one block growth).
- Write the new entry into the selected slot using PageCache.
- Update directory timestamps/flags and persist.

Error handling:
- Duplicate name → Err(EEXIST).
- Allocation failure → Err(ENOSPC) or appropriate mapping error.
- PageCache I/O errors → Err(EIO).

## delete_entry

Behavior:
- Locate the target entry by name; if not found, return Err(EIO) (Linux uses
  directory corruption semantics).
- Modify the directory block via PageCache (rec_len merge semantics) to delete
  the entry.
- Update directory timestamps/flags and persist.

## make_empty

Behavior:
- Precondition: directory has no existing data blocks.
- Allocate first data block (mapping) and set directory size to 1 block.
- Resize PageCache to 1 block.
- Write `.` and `..` entries into block 0 via PageCache.
- Persist.

Rollback:
- On any PageCache or persist failure, discard cache range, restore meta/mapping,
  and free the newly allocated block(s).

## mkdir

Behavior:
- Validate parent is directory; validate child name.
- Serialize parent mutation under parent `meta.write()`.
- Increment parent link count (for `..` of child).
- Create child inode (type=Dir) via filesystem allocator.
- Initialize child directory with `make_empty(parent_ino)`.
- Add directory entry in parent.
- Persist parent changes.

Rollback:
- On any failure after creating the child inode:
  - free the child inode (and release its blocks via cleanup helper if needed)
  - roll back parent link count

## rmdir

Behavior:
- Validate parent is directory; validate name.
- Locate child inode, verify it is a directory and empty.
- Delete parent directory entry.
- Mark child as freed (nlink -2, dtime set, size 0) and persist child.
- Decrement parent link count and persist parent.

Lock:
- This touches both parent and child. Acquire meta locks in ascending inode
  number order to avoid deadlock.

## create

Behavior:
- Parent must be directory.
- If type_==Dir, delegate to mkdir.
- Else:
  - Create inode via filesystem allocator.
  - Add directory entry.
  - On failure, roll back by freeing the inode.

## link

Behavior:
- Add a hard link in this directory to an existing non-directory inode.
- Reject hard links to directories (EPERM).
- Enforce MAX_LINK_COUNT.
- Cross-filesystem check.
- Increment target inode `links_count` and set `ctime` before adding directory
  entry.
- On `add_entry` failure, roll back link count.
- Persist the target inode after directory mutation.

Lock:
- Avoid holding 2 inode `meta.write()` locks concurrently.
  - Mutate the target inode metadata under its `meta.write()`, then drop.
  - Perform directory mutation (`add_entry`) under directory serialization.
  - Persist the target inode under its own `meta.write()` + `mapping.write()`.

## unlink

Behavior:
- Remove a non-directory entry from this directory.
- Reject directories (EISDIR).
- Delete directory entry first.
- Decrement child inode link count; if it reaches 0, set `dtime` and mark freed.
- Persist child inode.

Lock:
- Avoid holding 2 inode `meta.write()` locks concurrently.
  - Perform directory mutation (`delete_entry`) first.
  - Then update/persist child inode metadata.

[TEST]

## add_entry/delete_entry
- add_entry then lookup returns correct ino.
- delete_entry removes name and lookup fails.
- Concurrent lookup/readdir are blocked during mutation (no intermediate state).

## mkdir/rmdir
- mkdir creates child dir with '.' and '..'.
- rmdir of non-empty dir → ENOTEMPTY.
- rmdir updates link counts and marks child freed.

## link/unlink
- link creates new name for same inode, nlink increments.
- unlink removes entry and decrements nlink; when nlink hits 0, inode marked freed.
