# Phase 02 Diff Report: Block Groups & Bitmaps

## Scope

- Rust: `kernel/src/fs/ext2/block_group.rs`
- Linux references:
  - `/root/linux/fs/ext2/balloc.c:71` (`ext2_valid_block_bitmap`)
  - `/root/linux/fs/ext2/balloc.c:128` (`read_block_bitmap`)
  - `/root/linux/fs/ext2/ialloc.c:46` (`read_inode_bitmap`)
  - `/root/linux/fs/ext2/balloc.c:168` (`group_adjust_blocks`)

---

## 1) Bitmap Always Read from Disk (No In-Memory Cache)

- **Linux**: Bitmaps are read via `sb_bread()` / `sb_getblk()` which returns a `buffer_head`. The buffer_head is cached by the block layer. Subsequent accesses to the same bitmap block hit the buffer cache — no disk I/O needed. Modifications use `mark_buffer_dirty(bh)` for deferred writeback.
  - Reference: `/root/linux/fs/ext2/balloc.c:128` (`read_block_bitmap`)
- **Asterinas**: Every call to `load_block_bitmap()` or `load_inode_bitmap()` performs a fresh `read_bytes()` from the block device. After modification (alloc/free), the entire bitmap is written back immediately via `write_bytes()`. There is no in-memory bitmap cache.
  - Rust: `block_group.rs:174` (`load_block_bitmap`), `block_group.rs:250` (`load_inode_bitmap`)
- **Impact**:
  - Every allocation/free operation requires 1 read + 1 write I/O for the bitmap block.
  - Concurrent allocations in the same group will each read their own copy, creating race conditions (last writer wins).
  - No dirty tracking for bitmaps — always full block write.

## 2) Error Handling Divergence in Bitmap Validation

- **Linux**: `ext2_valid_block_bitmap()` checks that metadata block bits are set in the bitmap. On failure, it calls `ext2_error()` (which logs the error) and returns 0. The caller `read_block_bitmap()` **still returns the bitmap** (with comment: "file system mounted not to panic on error, continue with corrupt bitmap").
  - Reference: `/root/linux/fs/ext2/balloc.c:114-166`
- **Asterinas**: `load_block_bitmap()` contains an inline `valid_block_bitmap` closure that returns `Err(EINVAL)` on any validation failure. This error propagates up and **aborts the operation**.
  - Rust: `block_group.rs:200-242`
- **Impact**: Asterinas is stricter — a corrupt bitmap prevents all operations on that group. Linux continues with the corrupt bitmap (errors=continue semantics). This changes the error path behavior significantly.

## 3) Extra Validation Not in Linux

- **Linux**: `ext2_valid_block_bitmap` does NOT check:
  - Whether `last_block < first_block`
  - Whether `capacity > IdBitmap::capacity()`
- **Asterinas**: Adds these defensive checks:
  - `if last_block < first_block { return_errno!(Errno::EINVAL); }` (line 189)
  - `if capacity > IdBitmap::capacity() as usize { return_errno!(Errno::EINVAL); }` (line 194)
- **Impact**: These are extra safety guards not present in Linux. For strict Linux alignment, they should be removed or converted to warnings.

## 4) Inode Table Validation: Bit-by-Bit vs find_next_zero_bit

- **Linux**: Uses `ext2_find_next_zero_bit()` to efficiently check that all inode table bits are set in one scan. If a zero bit is found within the inode table range, validation fails.
  - Reference: `/root/linux/fs/ext2/balloc.c:107-112`
- **Asterinas**: Iterates bit-by-bit in a `while` loop checking `bitmap.is_allocated(bit)` for each inode table block.
  - Rust: `block_group.rs:232-238`
- **Impact**: Functionally equivalent but less efficient. Not a correctness issue.

## 5) Counter Update Atomicity

- **Linux**: Group descriptor counter updates use `spin_lock(sb_bgl_lock())` for per-group locking. This allows concurrent updates to different groups without contention.
  - Reference: `/root/linux/fs/ext2/balloc.c:175-178` (`group_adjust_blocks`)
  - Reference: `/root/linux/fs/ext2/ialloc.c:78-82` (`ext2_release_inode`)
- **Asterinas**: Uses `RwMutex` on the entire `GroupDesc`. Counter updates acquire a write lock on the descriptor.
  - Rust: `block_group.rs:115-148`
- **Impact**: `RwMutex` is coarser-grained than Linux's per-group spinlock. Multiple concurrent readers of the same group's counters will not block each other (RwMutex allows concurrent reads), but any write blocks all access. This is acceptable for current single-threaded usage but may become a bottleneck under concurrent I/O.

## 6) `mark_buffer_dirty` vs Immediate Write

- **Linux**: After modifying a bitmap or group descriptor, Linux calls `mark_buffer_dirty(bh)`. The actual disk write is deferred to the writeback thread or explicit sync.
- **Asterinas**: After modifying a bitmap, the entire bitmap block is immediately written to disk via `write_bytes()`. Group descriptor changes are tracked via `Dirty<GroupDesc>` and flushed in `sync_metadata()`.
- **Impact**: Asterinas has synchronous bitmap I/O (slower but simpler). Group descriptor writeback is deferred (aligned with Linux intent).

## 7) No `percpu_counter` Equivalent

- **Linux**: Uses `percpu_counter` for superblock-level free block/inode counts, providing scalable concurrent updates.
  - Reference: `/root/linux/fs/ext2/ialloc.c:83` (`percpu_counter_inc`)
- **Asterinas**: Uses a single `RwMutex<Dirty<SuperBlock>>` with direct field mutation.
- **Impact**: No per-CPU scaling. All counter updates contend on the same lock.

---

## Spec-vs-Linux Supplement (from spec review)

### S1) phase-02-block-bitmap-read.spec: `ext2_valid_block_bitmap` Alignment

- **Spec**: Specifies Linux-equivalent validation constraints including `find_next_zero_bit` check for inode table range. The spec explicitly requires checking that block bitmap, inode bitmap, and inode table bits are all set in the bitmap.
- **Linux**: `ext2_valid_block_bitmap()` performs the same three checks plus the `find_next_zero_bit` scan.
  - Reference: `/root/linux/fs/ext2/balloc.c:71`
- **Status**: Spec is aligned with Linux. Implementation uses bit-by-bit loop instead of `find_next_zero_bit` (see item 4 above), but the spec itself is correct.

### S2) phase-02-inode-bitmap-read.spec: No Validation

- **Spec**: `load_inode_bitmap` reads one block and wraps it as `IdBitmap` with `capacity = inodes_per_group`. No validation of bitmap contents.
- **Linux**: `read_inode_bitmap()` in `ialloc.c:40` also performs no validation of inode bitmap contents — it just calls `sb_bread()` to read the block. Unlike block bitmaps, Linux does NOT have an `ext2_valid_inode_bitmap` function.
  - Reference: `/root/linux/fs/ext2/ialloc.c:40`
- **Status**: Aligned. Both Linux and Asterinas skip inode bitmap validation.

### S3) phase-02-block-group-cache.spec: `GroupDesc` Field Set

- **Spec**: `GroupDesc` contains `block_bitmap`, `inode_bitmap`, `inode_table`, `free_blocks_count`, `free_inodes_count`, `used_dirs_count`.
- **Linux**: `ext2_group_desc` has the same fields plus `bg_pad` and `bg_reserved[3]`.
- **Status**: Aligned for functional fields. Padding/reserved fields are correctly zeroed in `RawGroupDesc`.
