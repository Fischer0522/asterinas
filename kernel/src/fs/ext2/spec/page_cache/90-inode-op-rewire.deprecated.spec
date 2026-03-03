[PROMPT]
DEPRECATED: Phase 4 has been split into the following specs (use these instead):

- 14-phase4-1-metadata-xattr-dir-read.spec
- 15-phase4-2-io-and-symlink.spec
- 16-phase4-3-resize-fallocate.spec
- 17-phase4-4-dir-mutations.spec
- 18-phase4-5-rename-and-set-link.spec

Phase 4 (rewire operations): update `Inode` high-level operations to use split
meta/mapping locks and remove the upread/upgrade choreography that previously
existed to avoid PageCacheBackend callback self-deadlock.

Directory mutations are serialized: they may block concurrent directory reads.

Provide modifications to `kernel/src/fs/ext2/inode.rs` and
`kernel/src/fs/ext2/impl_for_vfs/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_readpage            → fs/ext2/inode.c (address_space ops)
ext2_write_begin         → fs/ext2/inode.c:928
ext2_write_end           → fs/ext2/inode.c:939
ext2_write_failed        → fs/ext2/inode.c:59
ext2_setsize             → fs/ext2/inode.c:1275
block_truncate_page      → fs/buffer.c:2654
generic_buffers_fsync    → fs/buffer.c:646
ext2_add_link            → fs/ext2/dir.c:476
ext2_delete_entry        → fs/ext2/dir.c:560
ext2_set_link            → fs/ext2/dir.c:519

[RELY]
```rust
use super::prelude::*;
use super::fs::Ext2;
```

```rust
pub(super) struct InodeInner { /* from spec 11 */ }
```

[GUARANTEE]

```rust
impl Inode {
    pub(super) fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize>;
    pub(super) fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize>;
    pub(super) fn resize(&self, new_size: usize) -> Result<()>;

    pub(super) fn add_entry(
        &self,
        name: &str,
        ino: u32,
        file_type: DirEntryFileType,
    ) -> Result<()>;
    pub(super) fn delete_entry(&self, name: &str) -> Result<()>;

    pub(super) fn sync_data(&self) -> Result<()>;
    pub(super) fn sync_all(&self) -> Result<()>;
    pub(super) fn prepare_for_evict(&self) -> Result<bool>;

    pub(super) fn metadata(&self) -> Metadata;
}
```

[SPECIFICATION]

## Buffered file IO

- `read_at`:
  - Acquire `meta.read()` to obtain file size and enforce EOF.
  - Perform VMO read via PageCache.
  - Update atime after read.

- `write_at`:
  - Acquire `meta.write()` for duration (size/times policy).
  - Under `mapping.write()`, allocate all logical blocks touched by the write
    range (`get_or_alloc_block(create=true)`), returning allocation errors from
    `write_at`.
  - Drop `mapping.write()` before touching VMO/PageCache.
  - Perform VMO write.
  - Update `mtime/ctime`, then persist via `persist_inode_locked`.
  - On failure after allocation, rollback:
    - discard speculative cache range
    - truncate newly allocated blocks
    - restore old size

## Directory operations

- Directory mutation serialization (accepted):
  - `add_entry`/`delete_entry`/`set_link` and rename-related helpers MUST hold
    `meta.write()` for the duration.
  - `lookup`/`readdir_at` may be blocked during mutations.

## Sync

- `sync_data` flushes dirty data pages first (PageCache eviction), then persists
  metadata if needed, then issues device flush.
- `sync_all` includes metadata persistence and metadata sync at FS level.

## Locking

- Must follow split-lock rules from spec 11.

[DIFF]

Linux: buffer-head based write_begin/write_end integrates block mapping with page
  cache update.
  → Asterinas: VMO-backed PageCache uses backend callbacks for mapping and I/O.
  The split-lock refactor preserves semantics while avoiding callback deadlocks.

[TEST]

## Buffered write/read regression
- Write extends file and allocates blocks → read returns the same data.
- Write ENOSPC → state rolled back (size/mapping/pages).

## Directory mutation
- add_entry then lookup returns ino.
- delete_entry removes name and lookup fails.
- Concurrent readdir/lookup blocked during mutation (no mid-state observed).

## Deadlock regression
- Previously-deadlocking pattern (meta write held while page commit triggers
  backend mapping) completes.
