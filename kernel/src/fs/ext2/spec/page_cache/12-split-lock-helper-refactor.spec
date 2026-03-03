[PROMPT]
Phase 3.1 (helper refactor): after the split-lock layout in spec 11, refactor
Ext2 inode helper routines so that:

- Block mapping mutations are isolated to `mapping.write()` sections.
- No helper holds `mapping.write()` while calling into PageCache/VMO operations
  that may trigger PageCacheBackend callbacks.
- Error rollback (write failure / growth failure) remains Linux-compatible.

This phase may still be non-compiling until Phase 4 rewires all inode
operations. Focus on helper correctness and lock safety.

Provide modifications to `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_write_begin         → fs/ext2/inode.c:928
ext2_write_end           → fs/ext2/inode.c:939
ext2_write_failed        → fs/ext2/inode.c:59
ext2_get_block           → fs/ext2/inode.c:783
ext2_get_blocks          → fs/ext2/inode.c:624
__ext2_truncate_blocks   → fs/ext2/inode.c:1172
ext2_add_link            → fs/ext2/dir.c:476
ext2_find_entry          → fs/ext2/dir.c:342
ext2_readdir             → fs/ext2/dir.c:257
ext2_delete_entry        → fs/ext2/dir.c:560
ext2_make_empty          → fs/ext2/dir.c:617
ext2_empty_dir           → fs/ext2/dir.c:659
ext2_set_link            → fs/ext2/dir.c:450

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
/// Split-lock inode state container (from spec 11).
#[derive(Debug)]
pub(super) struct InodeInner {
    pub(super) meta: RwMutex<InodeMeta>,
    pub(super) mapping: RwMutex<InodeMapping>,
    pub(super) page_cache: PageCache,
}
```

```rust
/// PageCache infrastructure.
pub struct PageCache { /* ... */ }
impl PageCache {
    pub fn pages(&self) -> &Arc<Vmo>;
    pub fn resize(&self, new_size: usize) -> Result<()>;
    pub fn discard_range(&self, range: Range<usize>);
    pub fn evict_range(&self, range: Range<usize>) -> Result<()>;
    pub fn fill_zeros(&self, range: Range<usize>) -> Result<()>;
}
```

[GUARANTEE]

```rust
impl InodeInner {
    /// Prepares block mappings and cache capacity for a buffered write.
    ///
    /// # Lock
    /// - Caller must hold `meta.write()` for the inode.
    /// - This helper will take `mapping.write()` internally, but must not hold it
    ///   across PageCache/VMO operations.
    pub(super) fn prepare_continuous_blocks(
        &self,
        // Inode metadata (size/times/flags).
        meta: &mut InodeMeta,
        // Filesystem for block allocation and indirect I/O.
        fs: &Ext2,
        // Write start offset in bytes.
        offset: usize,
        // Write end offset in bytes (exclusive).
        end: usize,
        // Filesystem block size in bytes.
        block_size: usize,
        // Whether to discard existing cached bytes in [offset, end) (direct I/O).
        discard_page_cache: bool,
    ) -> Result<()>;

    /// Best-effort rollback for failed write paths.
    ///
    /// Linux analogue: `ext2_write_failed`.
    ///
    /// # Lock
    /// - Caller must hold `meta.write()`.
    /// - Must not hold `mapping.write()` across PageCache operations.
    pub(super) fn write_failed_cleanup(
        &self,
        meta: &mut InodeMeta,
        fs: &Ext2,
        // File size before the operation.
        old_size: usize,
        // Byte end offset that was being written.
        end: usize,
        block_size: usize,
    );

    /// Grows a directory by exactly one data block and returns the new free slot.
    ///
    /// # Lock
    /// - Caller must hold directory `meta.write()` for the duration of the outer
    ///   mutation (serialization).
    /// - Takes `mapping.write()` internally only around allocation/truncation.
    pub(super) fn grow_dir_block(&self, meta: &mut InodeMeta, fs: &Ext2) -> Result<DirSlotInfo>;

    /// Cleanup helper used by mkdir/rename rollback paths.
    ///
    /// Frees all directory data blocks and clears cached pages.
    ///
    /// # Lock
    /// Caller must hold the child's `meta.write()`.
    pub(super) fn release_dir_data_blocks_for_cleanup(
        &self,
        meta: &mut InodeMeta,
        fs: &Ext2,
    ) -> Result<()>;

    /// Flushes dirty data pages for this inode.
    ///
    /// Linux analogue: file_write_and_wait_range in generic_buffers_fsync.
    ///
    /// # Lock
    /// - Caller holds `meta.read()` or `meta.write()`.
    /// - Must not take `mapping.write()`.
    pub(super) fn sync_data_pages(&self, meta: &InodeMeta) -> Result<()>;

    // -------------------------
    // Directory PageCache helpers
    // -------------------------

    /// Reads directory entries starting at `offset`.
    ///
    /// # Lock
    /// - Caller holds directory `meta.read()` (or `meta.write()` during a mutation).
    /// - Must not take `mapping.write()`.
    pub(super) fn readdir_at(
        &self,
        meta: &InodeMeta,
        fs: &Ext2,
        offset: usize,
        visitor: &mut dyn DirentVisitor,
    ) -> Result<usize>;

    /// Checks whether this directory contains only `.` and `..` as live entries.
    ///
    /// Linux: `ext2_empty_dir`.
    ///
    /// # Lock
    /// Caller holds directory `meta.read()`.
    pub(super) fn empty_dir(&self, meta: &InodeMeta, fs: &Ext2, self_ino: u32) -> bool;

    /// Finds a directory entry by name and returns its inode number.
    ///
    /// Linux: `ext2_find_entry`.
    ///
    /// # Lock
    /// Caller holds directory `meta.read()`.
    pub(super) fn find_entry(&self, meta: &InodeMeta, fs: &Ext2, name: &str) -> Result<u32>;

    /// Locates a directory entry by name and returns its on-disk position.
    ///
    /// Used by delete_entry and set_link.
    ///
    /// # Lock
    /// Caller holds directory `meta.read()`.
    pub(super) fn find_entry_target(
        &self,
        meta: &InodeMeta,
        fs: &Ext2,
        name: &str,
    ) -> Result<DirEntryTarget>;

    /// Scan directory blocks for a reusable slot or duplicate.
    ///
    /// Linux: scan loop in `ext2_add_link`.
    ///
    /// # Lock
    /// Caller holds directory `meta.read()`.
    pub(super) fn scan_dir_for_slot(
        &self,
        meta: &InodeMeta,
        fs: &Ext2,
        name: &str,
    ) -> Result<DirScanResult>;

    /// Writes a new directory entry into a selected slot via PageCache.
    ///
    /// Linux: commit phase of `ext2_add_link`.
    ///
    /// # Lock
    /// Caller holds directory `meta.read()` or `meta.write()`.
    pub(super) fn write_dir_entry(
        &self,
        meta: &InodeMeta,
        fs: &Ext2,
        slot: &DirSlotInfo,
        name: &str,
        ino: u32,
        ft: u8,
    ) -> Result<()>;

    /// Deletes a located entry by rewriting directory bytes in PageCache.
    ///
    /// Linux: `ext2_delete_entry`.
    ///
    /// # Lock
    /// Caller holds directory `meta.read()` or `meta.write()`.
    pub(super) fn delete_entry_in_cache(
        &self,
        meta: &InodeMeta,
        fs: &Ext2,
        target: &DirEntryTarget,
    ) -> Result<()>;

    /// Rewrites a located entry's inode/type via PageCache.
    ///
    /// Linux: `ext2_set_link`.
    ///
    /// # Lock
    /// Caller holds directory `meta.read()` or `meta.write()`.
    pub(super) fn set_link_in_cache(
        &self,
        meta: &InodeMeta,
        fs: &Ext2,
        target: &DirEntryTarget,
        new_ino: u32,
        ft: u8,
    ) -> Result<()>;

    // -------------------------
    // Direct I/O helpers (mapping-read only)
    // -------------------------

    /// Reads file data directly from already-allocated data blocks.
    ///
    /// # Lock
    /// Caller holds `mapping.read()` (passed in), and must not hold `mapping.write()`.
    pub(super) fn read_direct_at(
        &self,
        mapping: &InodeMapping,
        fs: &Ext2,
        offset: usize,
        end: usize,
        writer: &mut VmWriter,
    ) -> Result<()>;

    /// Writes file data directly to already-allocated data blocks.
    ///
    /// # Lock
    /// Caller holds `mapping.read()` (passed in), and must not hold `mapping.write()`.
    pub(super) fn write_direct_at(
        &self,
        mapping: &InodeMapping,
        fs: &Ext2,
        offset: usize,
        reader: &mut VmReader,
    ) -> Result<()>;
}
```

[SPECIFICATION]

## Shared lock rules

- Inode lock order: `meta` then `mapping`.
- No helper may hold `mapping.write()` while calling any PageCache/VMO method
  (`pages().read/write*`, `resize`, `evict_range`, `fill_zeros`, `discard_range`).

## Helper layering

- "In-cache" directory helpers (`find_entry*`, `scan_dir_for_slot`,
  `write_dir_entry`, `delete_entry_in_cache`, `set_link_in_cache`, `readdir_at`)
  MUST NOT:
  - allocate/free blocks
  - persist inode descriptors
  - mutate metadata other than through the explicit `meta: &mut InodeMeta` args

  They operate purely on PageCache/VMO directory bytes under caller-held `meta`
  locks.

## InodeInner::prepare_continuous_blocks

Pre:
- `block_size > 0`.
- Caller holds `meta.write()`.

Post (success):
- All logical blocks covering `[offset, end)` are mapped (allocated if needed).
- If `end > old_size`, then:
  - `meta.desc.size` is updated to `end`.
  - PageCache capacity is resized to `end.align_up(block_size)`.
- If `discard_page_cache == true`, discards cached bytes in
  `[min(offset, old_size), min(end, old_size))`.

Post (failure):
- Returns the first allocation or cache error.
- Must not leave `mapping` with partially-allocated branches visible without a
  corresponding `meta.size` change.
  - If partial allocation is unavoidable, it must be handled by callers via
    `write_failed_cleanup`.

## InodeInner::write_failed_cleanup

Pre:
- Caller holds `meta.write()`.

Post:
- If `end <= old_size`: no-op.
- Else (best-effort cleanup):
  - Discards speculative cached range `[old_size.align_up(block_size), end.align_up(block_size))`.
  - Resizes PageCache down to `old_size.align_up(block_size)`.
  - Truncates mapping back to `old_size` via `InodeMapping::truncate_blocks`.
  - Restores `meta.desc.size = old_size`.
- Any cleanup failure must be logged (no panic); the original error from the
  write path is returned by the caller.

## InodeInner::grow_dir_block

Pre:
- Caller holds directory `meta.write()`.
- Directory size is `old_size = meta.desc.size`.

Post (success):
- Allocates exactly one new directory data block at logical index
  `old_size.div_ceil(block_size)`.
- Updates `meta.desc.size = old_size + block_size`.
- Resizes PageCache to the new directory size.
- Returns `DirSlotInfo { dir_offset: old_size, slot_rec_len: block_size, used_rec_len: 0 }`.

Post (failure):
- Rolls back `meta.size` and any newly allocated blocks.
- Discards any newly-created cache range for the growth block.

## InodeInner::release_dir_data_blocks_for_cleanup

Pre:
- Caller holds child directory `meta.write()`.

Post (success):
- Mapping is truncated to size 0 (all blocks freed).
- `meta.desc.size = 0`.
- PageCache is cleared to size 0 and/or discards all cached directory bytes.

## InodeInner::sync_data_pages

Pre:
- Caller holds `meta.read()` or `meta.write()`.

Post:
- If file size is 0: Ok(())
- Else: writes back dirty pages in `[0, file_size)` and waits for completion.

[DIFF]

Linux: failure cleanup is orchestrated by pagecache + buffer-head layers.
  → Asterinas: VMO-backed PageCache requires explicit ordering to avoid
    PageCacheBackend callback deadlocks.

[TEST]

## prepare_continuous_blocks
- Allocate for a range that crosses indirect boundaries → returns Ok and mapping is complete.
- Allocation fails mid-range (ENOSPC) → returns Err and later `write_failed_cleanup` can restore old size.
- discard_page_cache=true and range overlaps old data → cached bytes in range are discarded.

## write_failed_cleanup
- end <= old_size → no-op.
- After speculative extension, cleanup restores meta.size and truncates mapping.

## grow_dir_block
- Growth succeeds → size increases by block_size and returned slot offset equals old_size.
- PageCache resize fails → mapping and meta.size rolled back; cache growth discarded.

## release_dir_data_blocks_for_cleanup
- After mkdir rollback → mapping freed, size 0, cache empty.

## sync_data_pages
- file_size==0 → no-op.
- file_size>0 and dirty pages exist → eviction writes back and returns Ok.

## directory in-cache helpers
- scan_dir_for_slot returns EEXIST for duplicates.
- find_entry returns ino for existing entry; returns ENOENT for missing.
- delete_entry_in_cache updates rec_len merge semantics.
- set_link_in_cache rewrites inode number + file_type in-place.
