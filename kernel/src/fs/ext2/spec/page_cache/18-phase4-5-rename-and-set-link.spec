[PROMPT]
Phase 4.5 (rename + set_link): complete the namei refactor by rewiring
`set_link` and `rename` to the split-lock layout.

These operations are the most lock-sensitive:

- They mutate directory entries via PageCache (which may call backend callbacks).
- They involve multiple inodes and require deadlock-free lock ordering.

Rules:

- Directory mutations are serialized via `meta.write()`.
- Acquire multiple inode `meta.write()` locks in ascending inode-number order.
- Never hold `mapping.write()` across PageCache/VMO operations.

Provide modifications to `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_set_link           → fs/ext2/dir.c:450
ext2_rename             → fs/ext2/namei.c:318
ext2_find_entry         → fs/ext2/dir.c:342
ext2_delete_entry       → fs/ext2/dir.c:560
ext2_add_link           → fs/ext2/dir.c:476

[RELY]
```rust
use super::prelude::*;
use super::fs::Ext2;
```

```rust
pub(super) struct InodeInner { /* from spec 11 */ }
```

```rust
// Multi-inode meta lock helpers from spec 13.
fn meta_write_lock_two_inodes<'a>(a: &'a Inode, b: &'a Inode) -> (RwMutexWriteGuard<'a, InodeMeta>, RwMutexWriteGuard<'a, InodeMeta>);
```

```rust
// Directory mutation primitives from Phase 4.4.
impl Inode {
    pub(super) fn add_entry(&self, name: &str, ino: u32, file_type: DirEntryFileType) -> Result<()>;
    pub(super) fn delete_entry(&self, name: &str) -> Result<()>;
}
```

[GUARANTEE]

```rust
impl Inode {
    pub(super) fn set_link(
        &self,
        name: &str,
        new_ino: u32,
        file_type: DirEntryFileType,
        update_times: bool,
    ) -> Result<()>;

    pub(super) fn rename(&self, old_name: &str, target: &Inode, new_name: &str) -> Result<()>;
}
```

[SPECIFICATION]

## set_link

Behavior:
- Validate directory type and inode number bounds.
- Locate the named entry and rewrite its inode number + file_type in-place via
  PageCache (read-modify-write of the containing directory block).
- If `update_times == true`, update directory timestamps/flags (mtime/ctime,
  clear INDEX_DIR).
- Else: do not update mtime/ctime, but still clear INDEX_DIR.
- Persist via `persist_inode_locked`.

Lock:
- Hold directory `meta.write()` for the full mutation.
- Take `mapping.write()` only around final persistence.
- Do not hold `mapping.write()` during PageCache I/O.

## rename

Pre:
- `self` and `target` are directories.
- `old_name` and `new_name` are valid (not '.' or '..', bounds checked).
- Cross-filesystem rename is rejected.

Locking:
- If `self == target`:
  - Acquire one directory `meta.write()`.
- Else:
  - Acquire both directories `meta.write()` in ascending inode-number order.

Implementation constraints:
- Rename must NOT call public `add_entry/delete_entry/set_link` if those
  functions acquire `meta.write()` internally (would self-deadlock).
  Use one of the following patterns:
  - Provide internal `*_locked(meta: &mut InodeMeta, ...)` helpers that assume
    `meta.write()` is already held.
  - Or implement the mutation steps inline under the already-held meta guards.

Behavior (Linux-compatible):
- If destination exists:
  - Enforce type compatibility (dir vs non-dir).
  - If replacing a directory, require it to be empty.
  - Replace destination entry by `set_link(new_name, old_ino, ..., update_times=true)`.
  - Decrement replaced inode link counts; if nlink reaches 0, set dtime and mark freed.
- If destination does not exist:
  - Add new entry in target directory for the moved inode.
  - If the moved inode is a directory and parent changes, increment new parent link count.
- Always delete the old entry from the source directory.
- If moving a directory across parents:
  - Update moved directory's `..` entry to point to the new parent using
    `set_link("..", target_ino, Dir, update_times=false)`.
  - Decrement old parent link count.

Persistence:
- Any directory metadata changes must be persisted via `persist_inode_locked`.
- Any affected inodes with updated link counts / ctime must be persisted.

[TEST]

## set_link
- set_link on existing entry updates inode number.
- update_times=false does not change mtime/ctime but clears INDEX_DIR.

## rename
- Same-dir rename without replacement.
- Cross-dir rename without replacement.
- Rename with replacement of file.
- Rename with replacement of empty directory.
- Rename moving a directory updates '..' and link counts.
