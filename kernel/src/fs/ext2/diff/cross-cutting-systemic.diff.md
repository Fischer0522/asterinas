# Cross-Cutting Diff Report: Systemic Architectural Differences

## Scope

This document covers differences that span multiple modules and represent fundamental architectural divergences between the Linux Ext2 implementation and the current Asterinas Ext2 implementation.

---

## 1) No buffer_head Mechanism

### Linux
All metadata I/O (superblock, group descriptors, bitmaps, inode table blocks, indirect blocks, directory data blocks) goes through `buffer_head`. Key properties:
- **Caching**: `sb_bread()` returns a cached `buffer_head`. Repeated reads of the same block hit the buffer cache.
- **Deferred writeback**: `mark_buffer_dirty(bh)` flags the buffer for later writeback. Actual disk I/O is batched.
- **Reference counting**: `brelse(bh)` / `bget()` manage buffer lifetime.
- **Synchronous option**: `sync_dirty_buffer(bh)` for critical metadata.

### Asterinas
All metadata I/O uses direct `BlockDevice::read_bytes()` / `write_bytes()` or `BioSegment`-based reads. Key properties:
- **No caching**: Every read goes to disk. Every write goes to disk immediately.
- **No dirty tracking per block**: Only `Dirty<T>` wrapper on in-memory structs (SuperBlock, GroupDesc).
- **Full block read-modify-write**: Inode writes read the entire table block, patch one inode, write the entire block back.

### Impact
- Redundant I/O on repeated access to the same metadata block.
- No batching of writes — each operation is synchronous.
- The `PageCache` integration mentioned in the module docs is not yet wired for metadata.

---

## 2) Inode Cache: Global vs None

### Linux
The VFS maintains a global inode hash table. `iget_locked(sb, ino)` either returns a cached inode or allocates a new slot. Key properties:
- **Deduplication**: Only one in-memory `struct inode` per (sb, ino) pair.
- **Lifecycle**: `iput()` decrements refcount; when it reaches zero, `evict_inode()` is called.
- **Dirty tracking**: `mark_inode_dirty()` queues the inode for writeback.

### Asterinas
No inode cache. `read_inode(ino)` always reads from disk and returns a new `Arc<Inode>`.
- Rust: `fs.rs:143`

### Impact
- Multiple `Arc<Inode>` objects can exist for the same inode number simultaneously.
- Modifications to one copy are invisible to others.
- No lifecycle management — dropping `Arc<Inode>` does not trigger disk cleanup.

---

## 3) Bitmap: Always Read from Disk, No In-Memory Maintenance

### Linux
Bitmaps (block and inode) are read via `sb_bread()` into `buffer_head`. The buffer is cached by the block layer. Modifications use atomic bit operations (`ext2_set_bit_atomic`, `ext2_clear_bit_atomic`) directly on the cached buffer, then `mark_buffer_dirty(bh)`.
- Reference: `/root/linux/fs/ext2/balloc.c:542-549`

### Asterinas
Every `load_block_bitmap()` / `load_inode_bitmap()` call reads the full bitmap block from disk into a fresh `Vec<u8>`. After modification, the entire bitmap is written back immediately.
- Rust: `block_group.rs:174`, `block_group.rs:250`

### Impact
- No concurrent bitmap access safety — two threads loading the same bitmap get independent copies.
- Last writer wins on bitmap writeback.
- No atomic bit operations — entire bitmap is replaced on write.

---

## 4) Counter Accounting: checked_add/checked_sub vs Atomic/Spinlock

### Linux
Group descriptor counters use `le16_add_cpu()` under `spin_lock(sb_bgl_lock())`. Superblock counters use `percpu_counter`. These provide:
- Per-group spinlock granularity
- Per-CPU scaling for superblock counters
- No overflow/underflow panics — counters are trusted
- Reference: `/root/linux/fs/ext2/balloc.c:168-181`

### Asterinas
Mixed approaches:
- `SuperBlock::inc_free_blocks()`: `checked_add().unwrap()` — **panics on overflow**
- `SuperBlock::dec_free_blocks()`: `checked_sub().unwrap()` — **panics on underflow**
- `SuperBlock::dec_free_inodes()`: `debug_assert!` + raw `-= 1` — **wraps in release**
- `SuperBlock::inc_free_inodes()`: `+= 1` — **wraps silently**
- `BlockGroup::dec_free_blocks()`: `saturating_sub` — **silently clamps to 0**
- `BlockGroup::inc_free_blocks()`: `saturating_add` — **silently clamps to max**
- Rust: `super_block.rs:444-471`, `block_group.rs:115-148`

### Impact
- Inconsistent overflow/underflow behavior across different counters.
- `unwrap()` on checked arithmetic will panic the kernel on counter corruption.
- `saturating_*` silently masks counter bugs.
- Linux never panics on counter issues — it logs errors and continues.
- **Recommendation**: Unify to a single strategy. Either all `checked_*` with error return (no panic), or all `saturating_*` with warning logs.

---

## 5) Inode Lifecycle: No evict_inode / Orphan Management

### Linux
When an inode's VFS refcount drops to zero, `ext2_evict_inode()` is called:
- If `nlink == 0`: sets `i_dtime`, writes inode, truncates all data blocks, frees xattrs, frees the inode bitmap bit.
- If `nlink > 0`: just syncs metadata.
- Orphan list (`s_last_orphan`) tracks unlinked-but-open files for crash recovery.
- Reference: `/root/linux/fs/ext2/inode.c:72-112`

### Asterinas
No eviction mechanism. `Arc<Inode>` drop does nothing to disk state.
- `rmdir` explicitly calls `release_dir_data_blocks_for_cleanup()` and `free_inode()`.
- `mkdir` rollback does the same explicit cleanup.
- Rust: `inode.rs:1082` (`release_dir_data_blocks_for_cleanup`)

### Impact
- Block/inode leaks when files are deleted but still open.
- No crash recovery for interrupted operations.
- Each call site must manually handle cleanup instead of relying on a unified eviction path.

---

## 6) Block Group Traversal: Always from Group 0

### Linux
Block allocation uses goal-directed group selection:
- `ext2_new_blocks()` computes `goal_group` from the goal block, then iterates from there.
- Inode allocation uses `find_group_orlov()` / `find_group_dir()` / `find_group_other()` with parent-relative starting points and quadratic probing.
- Reference: `/root/linux/fs/ext2/balloc.c:1208`, `/root/linux/fs/ext2/ialloc.c:199-418`

### Asterinas
- `alloc_blocks()`: Always iterates `for group_idx in 0..groups_count` — starts from group 0.
  - Rust: `fs.rs:381`
- `alloc_inode()`: Iterates from parent group cyclically, which is closer to Linux.
  - Rust: `fs.rs:543`

### Impact
Block allocation always favors low-numbered groups, causing uneven wear and fragmentation.

---

## 7) Timestamp Updates: Deferred / Missing

### Linux
Directory mutations (`ext2_add_link`, `ext2_delete_entry`) update `ctime` and `mtime` via `inode_set_ctime_current()` and `inode->i_mtime = inode_set_ctime_current()`. The `dirsync` flag triggers synchronous writeback.
- Reference: `/root/linux/fs/ext2/dir.c:554`, `/root/linux/fs/ext2/dir.c:608`

### Asterinas
`update_dir_timestamps_and_flags()` has timestamp code commented out with a TODO. Only the `INDEX_DIR` flag removal is active.
- Rust: `inode.rs:1102-1116`

### Impact
All directory modifications leave `ctime`/`mtime` unchanged. `ls -l` will show stale timestamps.

---

## 8) Error Handling Divergence: Fail-Fast vs errors=continue

### Linux
`ext2_error()` is the central error reporting function. Its behavior depends on the mount option:
- `errors=continue`: Log error, continue operation.
- `errors=remount-ro`: Log error, remount filesystem read-only.
- `errors=panic`: Kernel panic.
Most bitmap/metadata validation errors use `ext2_error()` and continue.
- Reference: `/root/linux/fs/ext2/super.c:350`

### Asterinas
All validation errors return `Err(Errno)` immediately, aborting the current operation. There is no `ext2_error()` equivalent and no `errors=continue` semantics.
- Example: `load_block_bitmap` returns `Err(EINVAL)` on corrupt bitmap (Linux would continue).
- Rust: `block_group.rs:200-242`

### Impact
Asterinas is stricter — any metadata corruption aborts the operation. Linux tolerates more corruption under `errors=continue`.

---

## 9) PageCache: Not Wired for Data or Metadata

### Linux
All file data I/O goes through the page cache (`address_space_operations`). Directory data also uses the page cache via `ext2_get_folio()`. Metadata (indirect blocks) uses `buffer_head` cache.

### Asterinas
No PageCache integration for any path. All reads use `BlockDevice::read_bytes()` into stack/heap buffers. All writes use `BlockDevice::write_bytes()` from stack/heap buffers.

### Impact
- No read caching for any data type.
- No write coalescing or deferred writeback.
- Every operation is synchronous disk I/O.

---

## Spec-vs-Linux Supplement (from spec review)

### S1) phase-05-counter-accounting.spec: `sync_metadata` Write Time Update

- **Spec**: Before serialization, updates `sb.wtime = UnixTime::now()` (Linux `ext2_sync_super` `s_wtime` update).
- **Linux**: `ext2_sync_super()` sets `es->s_wtime = ktime_get_real_seconds()` before writing.
  - Reference: `/root/linux/fs/ext2/super.c:1283`
- **Status**: Aligned.

### S2) phase-05-counter-accounting.spec: No Free Count Recomputation on Sync

- **Spec**: `sync_metadata` writes existing in-memory counters directly. No recomputation from bitmaps.
- **Linux**: `ext2_sync_super()` calls `ext2_count_free_blocks()` and `ext2_count_free_inodes()` which scan all group descriptors to recompute totals before writing.
  - Reference: `/root/linux/fs/ext2/super.c:1290-1295`
- **Diff**: Asterinas trusts in-memory counters. Linux recomputes from group descriptors on every sync. If Asterinas counters drift due to bugs, the drift persists across syncs.

### S3) phase-05-counter-accounting.spec: Backup Descriptor Table Writeback

- **Spec**: `sync_metadata` writes primary descriptor table plus all backup groups per `is_backup_group`. Each backup superblock gets `block_group_idx` adjusted.
- **Linux**: `ext2_sync_super` also writes backup superblocks. Group descriptor backup writeback is handled separately in `ext2_commit_super`.
  - Reference: `/root/linux/fs/ext2/super.c:1359`
- **Status**: Aligned in intent. Asterinas writes all backups in one pass; Linux may defer some.
