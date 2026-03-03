[PROMPT]
Phase 4.2 (file I/O + symlink): rewire buffered/direct file I/O and symlink
read/write to the split-lock layout.

Covered ops (from the task breakdown):

- read_link / write_link
- read_at / write_at
- read_direct_at / write_direct_at

Rules:

- Must follow split-lock constraints from spec 11.
- Use helper routines from spec 12 for allocation and rollback.
- No `upread/upgrade/downgrade` lock choreography.

Provide modifications to `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_file_read_iter      → fs/ext2/file.c:283
ext2_file_write_iter     → fs/ext2/file.c:295
ext2_write_begin         → fs/ext2/inode.c:928
ext2_write_end           → fs/ext2/inode.c:939
ext2_write_failed        → fs/ext2/inode.c:59
ext2_dio_read_iter       → fs/ext2/file.c:168
ext2_dio_write_iter      → fs/ext2/file.c:214
ext2_setsize             → fs/ext2/inode.c:1275
block_truncate_page      → fs/buffer.c:2654
page_get_link            → fs/namei.c:6227
page_symlink             → fs/namei.c:6273

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
    pub(super) fn prepare_continuous_blocks(&self, meta: &mut InodeMeta, fs: &Ext2, offset: usize, end: usize, block_size: usize, discard_page_cache: bool) -> Result<()>;
    pub(super) fn write_failed_cleanup(&self, meta: &mut InodeMeta, fs: &Ext2, old_size: usize, end: usize, block_size: usize);
}
```

[GUARANTEE]

```rust
impl Inode {
    pub(super) fn read_link(&self) -> Result<String>;
    pub(super) fn write_link(&self, target: &str) -> Result<()>;

    pub(super) fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize>;
    pub(super) fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize>;

    pub(super) fn read_direct_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize>;
    pub(super) fn write_direct_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize>;
}
```

[SPECIFICATION]

## Buffered read_at

Pre:
- `self.type_ != Dir`.

Lock:
- Take `meta.read()` to obtain file size and compute EOF.
- Do not take `mapping.write()`.

Post (success):
- Reads min(avail, size-offset) bytes through PageCache VMO.
- Updates atime after the read.

## Buffered write_at

Pre:
- `self.type_ != Dir`.

Lock:
- Hold `meta.write()` for the whole operation (size/timestamp policy).
- Allocate blocks under `mapping.write()` only inside `prepare_continuous_blocks`.
- Never hold `mapping.write()` while calling PageCache/VMO write.

Algorithm:
1. Compute `end = offset + write_len` (overflow checked).
2. Record `old_size = meta.size`.
3. Call `inner.prepare_continuous_blocks(meta, fs, offset, end, block_size, false)`.
4. Write user bytes via `inner.page_cache.pages().write(offset, reader)`.
5. On error after allocation:
   - call `inner.write_failed_cleanup(meta, fs, old_size, end, block_size)`
   - return the write error.
6. Update `mtime/ctime`.
7. Persist via `persist_inode_locked` (take `mapping.write()` only around persist).

## Direct read_direct_at

Pre:
- block-aligned offset and length.

Lock:
- `meta.read()` for size gate.
- Use PageCache eviction (`evict_range`) without holding `mapping.write()`.
- Mapping reads for direct I/O use `mapping.read()` (not write).

Post:
- Evicts cached pages in range then reads blocks directly.
- Updates atime.

## Direct write_direct_at

Pre:
- block-aligned offset and length.

Lock:
- Hold `meta.write()`.
- Block allocation uses `prepare_continuous_blocks(..., discard_page_cache=true)`.
- Direct block writes must not hold `mapping.write()` across PageCache eviction.

Post:
- On success, direct I/O bytes are written to disk blocks and timestamps updated.
- On failure after allocation, rollback via `write_failed_cleanup`.

## Symlink read_link

Pre:
- `self.type_ == SymLink`.

Lock:
- Determine fast vs slow symlink using `meta` + `mapping` (order: meta then mapping).
- Slow symlink content read uses PageCache and may trigger backend mapping.read.

Post:
- Fast symlink reads bytes from `mapping.block_ptrs` encoding.
- Slow symlink reads bytes from PageCache.

## Symlink write_link

Pre:
- `self.type_ == SymLink`.
- `strlen(target)+1 <= block_size` else ENAMETOOLONG.

Lock:
- Hold `meta.write()`.
- Fast symlink path takes `mapping.write()` and persists immediately.
- Slow symlink path follows the same allocate → PageCache write → persist pattern
  as buffered write_at, with rollback on error.

[DIFF]

Linux: write_begin/write_end interact with buffer heads.
  → Asterinas: PageCache VMO uses backend callbacks; split locks preserve the
    ordering constraints without unsafe.

[TEST]

## Buffered IO
- write_at extends file then read_at returns same data.
- write_at ENOSPC → size/mapping/pagecache rolled back to old state.

## Direct IO
- write_direct_at then read_direct_at returns same data (aligned).
- direct write error path triggers rollback helper.

## Symlink
- Fast symlink: small target encoded in i_block, read_link returns it.
- Slow symlink: target stored in blocks/pagecache, read_link returns it.
