# Phase 01 Diff Report: SuperBlock & Raw Structures

## Scope

- Rust: `kernel/src/fs/ext2/super_block.rs`
- Linux references:
  - `/root/linux/fs/ext2/super.c:800+` (`ext2_fill_super`)
  - `/root/linux/fs/ext2/super.c:645` (`ext2_setup_super`)
  - `/root/linux/include/linux/ext2_fs.h`

---

## 1) SuperBlock I/O: No buffer_head Mechanism

- **Linux**: SuperBlock is read via `sb_bread()` which returns a `buffer_head`. The `buffer_head` is cached in `sbi->s_sbh` and reused for writeback via `mark_buffer_dirty(sbh)` + `sync_dirty_buffer(sbh)`. This means the superblock block is kept in the buffer cache and only written back when dirty.
  - Reference: `/root/linux/fs/ext2/super.c:877` (`sb_bread(sb, 1)`)
- **Asterinas**: SuperBlock is read via `device.read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)` — a direct raw I/O read. There is no buffer cache layer. Writeback in `sync_metadata()` does a full `write_bytes(SUPER_BLOCK_OFFSET, raw_sb.as_bytes())`.
  - Rust: `super_block.rs:280` (`load_super_block`)
- **Impact**: Every sync writes the full 1024-byte superblock to disk even if only one field changed. Linux's buffer_head approach only marks the block dirty and flushes on demand.

## 2) No `ext2_setup_super` Equivalent

- **Linux**: After reading the superblock, `ext2_setup_super()` performs:
  - Mount count increment (`s_mnt_count++`)
  - Max mount count check (force fsck warning)
  - Last check time validation
  - State flag update (`s_state &= ~EXT2_VALID_FS`)
  - Write time update (`s_wtime = ktime_get_real_seconds()`)
  - Reference: `/root/linux/fs/ext2/super.c:645`
- **Asterinas**: None of these mount-time state transitions are performed. The superblock is loaded read-only and state/mnt_count are never updated on mount.
  - Rust: `super_block.rs:279` (`load_super_block` returns immediately after validation)
- **Impact**: Filesystem state is never marked as "mounted" (no `VALID` flag clear), so crash recovery cannot detect unclean mounts.

## 3) Block Size Hardcoded to 4096

- **Linux**: Supports block sizes 1024, 2048, 4096 via `sb->s_blocksize = 1024 << le32_to_cpu(es->s_log_block_size)`.
  - Reference: `/root/linux/fs/ext2/super.c:905`
- **Asterinas**: Rejects any `log_block_size != 2` (i.e., only 4096). This is an intentional project constraint.
  - Rust: `super_block.rs:128`
- **Impact**: Cannot mount ext2 filesystems with 1024 or 2048 byte blocks.

## 4) ErrorsBehaviour Restricted to Continue

- **Linux**: Supports `Continue`, `RemountReadonly`, and `Panic` error behaviors. The behavior is used throughout the codebase via `ext2_error()`.
  - Reference: `/root/linux/fs/ext2/super.c:350` (`ext2_error`)
- **Asterinas**: Only `ErrorsBehaviour::Continue` is accepted; others are rejected at mount time.
  - Rust: `super_block.rs:143`
- **Impact**: Filesystems formatted with `errors=remount-ro` or `errors=panic` cannot be mounted.

## 5) Creator OS Restricted to Linux

- **Linux**: Accepts any creator OS and adjusts behavior accordingly (e.g., Hurd has different uid/gid handling).
- **Asterinas**: Only `OsId::Linux` is accepted.
  - Rust: `super_block.rs:149`

## 6) Feature Gating Differences

- **Linux**: Has a comprehensive feature check with separate handling for compat, incompat, and ro_compat. Unknown incompat features cause mount failure. Unknown ro_compat features force read-only mount.
  - Reference: `/root/linux/fs/ext2/super.c:920+`
- **Asterinas**: Only `FILETYPE` incompat feature is allowed. Ro_compat allows `SPARSE_SUPER` and `LARGE_FILE` only (for read-write). Other ro_compat features are allowed in read-only mode.
  - Rust: `super_block.rs:213-218`, `super_block.rs:289-293`
- **Impact**: Narrower feature support; filesystems with `EXT_ATTR`, `DIR_INDEX`, etc. cannot be mounted read-write.

## 7) Counter Overflow/Underflow Handling

- **Linux**: Uses `percpu_counter` for free blocks/inodes counters with atomic operations. Counter updates use `le16_add_cpu` with spinlock protection.
  - Reference: `/root/linux/fs/ext2/balloc.c:168` (`group_adjust_blocks`)
- **Asterinas**: `inc_free_blocks` uses `checked_add().unwrap()` (panics on overflow). `dec_free_inodes` uses `debug_assert!` + raw subtraction. `inc_free_inodes` uses `+= 1` without overflow check.
  - Rust: `super_block.rs:445-471`
- **Impact**: Inconsistent overflow protection. `checked_add().unwrap()` will panic in release mode on overflow, while `+= 1` silently wraps. Linux never panics on counter issues.

## 8) Group Descriptor Table Writeback

- **Linux**: Group descriptors are stored in `buffer_head` arrays (`sbi->s_group_desc[]`). Each descriptor block is individually dirtied via `mark_buffer_dirty(bh)`.
  - Reference: `/root/linux/fs/ext2/balloc.c:168`
- **Asterinas**: Group descriptors are stored in a `USegment`. On sync, the entire descriptor table is read from the segment, then written as a contiguous blob to all copies (primary + backups).
  - Rust: `fs.rs:714-799` (`sync_metadata`)
- **Impact**: Full table rewrite on every sync, even if only one group changed. No per-block granularity.

## 9) Backup Superblock Writeback

- **Linux**: Backup superblocks are updated during `ext2_sync_super` with per-group `block_group_nr` adjustment.
- **Asterinas**: `sync_metadata()` correctly iterates backup groups and sets `block_group_idx` per copy. This is aligned with Linux.
  - Rust: `fs.rs:776-795`
- **Status**: Aligned.

## 10) Missing `s_overhead_last` / `s_blocks_last` Calculation

- **Linux**: Computes filesystem overhead (metadata blocks) for `statfs` reporting.
  - Reference: `/root/linux/fs/ext2/super.c:1000+`
- **Asterinas**: No overhead calculation. `statfs` is not implemented.

---

## Spec-vs-Linux Supplement (from spec review)

### S1) phase-00-core-skeleton.spec: `Ext2::open` Skeleton

- **Spec**: `open()` returns `Err(ENOSYS)` in skeleton phase. `root_inode()` returns `Err(ENOSYS)`.
- **Linux**: `ext2_fill_super` is a ~300-line function that performs full mount: reads superblock, validates, loads group descriptors, reads root inode, sets up VFS superblock fields.
  - Reference: `/root/linux/fs/ext2/super.c:877`
- **Status**: Skeleton-only; no divergence to track yet. The spec explicitly defers all I/O.

### S2) phase-00-vfs-glue.spec: `Ext2Type` Registration

- **Spec**: `Ext2Type::create()` returns `Err(ENOSYS)`. `properties()` returns `NEED_DISK`.
- **Linux**: `ext2_fs_type` has `.fs_flags = FS_REQUIRES_DEV` and `.init_fs_context = ext2_init_fs_context` which wires up the full mount path.
  - Reference: `/root/linux/fs/ext2/super.c:1698`
- **Diff**: Linux uses `init_fs_context` + `get_tree_bdev` for mount orchestration. Asterinas uses a simpler `FsType::create()` trait method. This is an architectural difference in VFS integration, not an ext2-specific divergence.

### S3) phase-01-raw-structures.spec: On-Disk Layout

- **Spec**: `RawGroupDesc` (32 bytes), `RawInode` (128 bytes), `RawDirEntry` (8 bytes header).
- **Linux**: Identical layouts in `ext2.h`.
- **Status**: Aligned. Field order and sizes match exactly.

### S4) phase-01-superblock.spec: Validation Strictness

- **Spec**: Requires `groups_count * inodes_per_group == inodes_count` as an exact equality check.
- **Linux**: `ext2_fill_super` does NOT enforce this exact equality. It computes `groups_count` from `blocks_count` and trusts `inodes_count` independently. The only check is `inodes_per_group > 0` and `inodes_count > 0`.
  - Reference: `/root/linux/fs/ext2/super.c:960-980`
- **Impact**: Asterinas rejects filesystems where `inodes_count` is not an exact multiple of `inodes_per_group`. Linux accepts such filesystems (the last group may have fewer inodes).

### S5) phase-01-superblock.spec: `frag_size == block_size` Enforcement

- **Spec**: Requires `log_frag_size == log_block_size`.
- **Linux**: Also checks `s_log_frag_size == s_log_block_size` (fragment size must equal block size).
  - Reference: `/root/linux/fs/ext2/super.c:912`
- **Status**: Aligned.

### S6) phase-01-group-desc-table.spec: `check_group_desc_table`

- **Spec**: Validates `bg_block_bitmap`, `bg_inode_bitmap`, `bg_inode_table` are within `[first_block, last_block]` for each group.
- **Linux**: `ext2_check_descriptors()` performs the same range checks.
  - Reference: `/root/linux/fs/ext2/super.c:695`
- **Status**: Aligned. The spec faithfully mirrors Linux's `ext2_check_descriptors`.

### S7) phase-01-group-desc-table.spec: No `ext2_get_group_desc` Buffer Caching

- **Spec**: `BlockGroup::load()` reads from a pre-loaded `USegment` (in-memory copy). No per-block buffer_head caching.
- **Linux**: `ext2_get_group_desc()` returns a pointer into a cached `buffer_head` from `sbi->s_group_desc[block_group]`. The buffer_head is loaded once during mount and reused.
  - Reference: `/root/linux/fs/ext2/balloc.c:24`
- **Diff**: Both cache descriptors in memory. Linux caches at buffer_head granularity (one bh per descriptor block). Asterinas caches the entire table in a contiguous `USegment`. Functionally equivalent for read access; differs for write granularity (see item 8 above).
