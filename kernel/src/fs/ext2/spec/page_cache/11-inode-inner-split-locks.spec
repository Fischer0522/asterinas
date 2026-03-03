[PROMPT]
Phase 3 (lock split): refactor Ext2 inode mutable state from a single
`RwMutex<InodeInner>` to a split-lock `InodeInner` that contains independent
meta and mapping lock domains. Ensure PageCache backend callbacks never acquire
meta, eliminating the callback self-deadlock root cause.

This phase is a *structural* refactor. It is expected that the tree may not
compile until Phase 4 rewires all inode operations to the new lock layout.
Do not attempt to preserve the old upread/upgrade choreography.

Provide modifications to `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_get_block           → fs/ext2/inode.c:783
ext2_get_blocks          → fs/ext2/inode.c:624
generic_buffers_fsync    → fs/buffer.c:646
Locking concepts         → (i_rwsem, truncate/mapping locks in VFS)

[RELY]
```rust
use super::prelude::*;
use super::fs::Ext2;
```

```rust
use super::utils::Dirty;
```

```rust
pub(super) struct InodeMeta { /* from spec 10 */ }
pub(super) struct InodeMapping { /* from spec 10 */ }
```

```rust
/// PageCache infrastructure.
pub struct PageCache { /* ... */ }
pub trait PageCacheBackend: Sync + Send {
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;
    fn write_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;
    fn npages(&self) -> usize;
}
```

[GUARANTEE]

```rust
/// The Ext2 inode public handle.
///
/// NOTE: `inner` is no longer wrapped by a monolithic `RwMutex`.
#[derive(Debug)]
pub struct Inode {
    // ...
    inner: InodeInner,
    // ...
}

/// Split-lock inode state container.
#[derive(Debug)]
pub(super) struct InodeInner {
    meta: RwMutex<InodeMeta>,
    mapping: RwMutex<InodeMapping>,
    page_cache: PageCache,
}

impl InodeInner {
    pub(super) fn meta_read(&self) -> RwMutexReadGuard<'_, InodeMeta>;
    pub(super) fn meta_write(&self) -> RwMutexWriteGuard<'_, InodeMeta>;
    pub(super) fn mapping_read(&self) -> RwMutexReadGuard<'_, InodeMapping>;
    pub(super) fn mapping_write(&self) -> RwMutexWriteGuard<'_, InodeMapping>;
    pub(super) fn page_cache(&self) -> &PageCache;

    /// Persists inode meta+mapping to disk.
    ///
    /// # Lock
    /// The caller must hold both `meta.write()` and `mapping.write()`.
    pub(super) fn persist_inode_locked(
        meta: &mut InodeMeta,
        mapping: &mut InodeMapping,
        ino: u32,
        type_: InodeType,
        fs: &Ext2,
    ) -> Result<()>;
}
```

```rust
impl PageCacheBackend for Inode {
    /// # Lock
    /// Must take `mapping.read()` ONLY.
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;

    /// # Lock
    /// Must take `mapping.read()` ONLY.
    fn write_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;

    /// # Lock
    /// Must not take `meta`. Prefer using PageCache VMO size.
    fn npages(&self) -> usize;
}
```

[SPECIFICATION]

## Locking protocol

- Inode lock order within a single inode: `meta` then `mapping`.
- PageCache backend constraint:
  - Backend callbacks MUST NOT acquire `meta`.
  - Backend callbacks may acquire `mapping.read()`.
- PageCache interaction constraint:
  - No code path may hold `mapping.write()` while calling into VMO/PageCache
    operations that can trigger pager callbacks.

## Persistence

- `persist_inode_locked` assembles `RawInode` from meta+mapping split descriptors
  (see spec 9) and writes it via `Ext2::write_inode_desc`.
- On success, clears both dirty flags.
- `persist_inode_locked` is the single persistence primitive for inode table
  writes. Any convenience wrapper must delegate to it (no other direct
  `write_inode_desc` call sites).

## API cleanup

- Remove `commit_dir_metadata` (directory metadata commit helper).
  - Directory timestamp/flag updates must be expressed as explicit `meta` domain
    mutations, then persisted via `persist_inode_locked`.
  - Rationale: a dedicated "commit" helper tends to re-introduce mixed lock
    domains and hidden PageCache interactions.

[DIFF]

Linux: separate lock domains avoid get_block re-entrancy issues under writeback.
  → Asterinas: enforce backend callback restriction by construction using split
    locks.

[TEST]

## Deadlock regression
- Hold `meta.write()` and perform buffered VMO write that triggers page commit;
  must not deadlock (backend takes mapping.read()).
- Attempt to call backend while holding mapping.write() (if any path exists)
  must be prevented by design (no such code in production paths).
