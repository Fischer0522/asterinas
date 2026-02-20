# Ext2 Fix-List

Grounded in diff reports, spec review, and actual code inspection.
Each item includes: severity, file:line, Linux reference, and fix description.

---

## P0: Correctness — Will Cause Panic / Data Corruption

### F01. Counter arithmetic panics kernel
- **File**: `super_block.rs:446,451`
- **Symptom**: `checked_add().unwrap()` / `checked_sub().unwrap()` → kernel panic on counter corruption
- **Linux**: `le16_add_cpu()` under spinlock, never panics (`balloc.c:168`)
- **Fix**: Replace `unwrap()` with `return Err(Errno::EIO)` on overflow/underflow

### F02. Counter arithmetic inconsistent across paths
- **File**: `super_block.rs:446-472`
- **Symptom**: `inc_free_blocks` uses `unwrap()`, `inc_free_inodes` uses raw `+= 1` (wraps silently), `dec_free_inodes` uses `debug_assert!` + raw `-= 1`
- **Fix**: Unify all to `checked_*` with `Result` return, no panic, no silent wrap

### F03. `create_inode` missing uid/gid/timestamps/generation
- **File**: `fs.rs:503-527` (RawInode literal)
- **Linux**: `ext2_new_inode` sets uid/gid from credentials, timestamps from `current_time()`, generation from random (`ialloc.c:539+`)
- **Fix**: Fill `uid`/`gid` from current credentials, `atime`/`ctime`/`mtime` from `UnixTime::now()`, `generation` from counter or random

### F04. Timestamps not updated on directory mutations
- **File**: `inode.rs` — `add_entry`/`delete_entry`/`mkdir`/`rmdir` paths
- **Linux**: `ext2_add_link` sets `dir->i_mtime = dir->i_ctime = current_time()` (`dir.c:554`); `ext2_delete_entry` does same (`dir.c:608`)
- **Fix**: After successful dir mutation, set `ctime`/`mtime` = `UnixTime::now()` on parent inode, mark dirty

### F05. `inodes_count` validation too strict — rejects valid filesystems
- **File**: `super_block.rs:208-210`
- **Symptom**: `groups_count * inodes_per_group != inodes_count` → EINVAL
- **Linux**: Does NOT enforce exact equality; last group may have fewer inodes (`super.c:960-980`)
- **Fix**: Change to `inodes_count <= groups_count * inodes_per_group && inodes_count > (groups_count - 1) * inodes_per_group`

### F06. No `ext2_setup_super` on mount
- **File**: `super_block.rs:280-297` (`load_super_block`)
- **Linux**: `ext2_setup_super` increments `mnt_count`, clears `VALID_FS` state, sets `wtime` (`super.c:645`)
- **Fix**: After validation in `load_super_block` (for rw mount): `mnt_count += 1`, `state &= ~VALID_FS`, `wtime = now()`, write back to disk

---

## P1: Functional — Wrong Behavior Under Normal Use

### F07. Block allocation always starts from group 0
- **File**: `fs.rs:349` (`for group in &self.block_groups`)
- **Linux**: Starts from `goal_group` derived from goal block (`balloc.c:1260`)
- **Fix**: Accept `goal: Bid` parameter, compute `goal_group`, iterate cyclically from there

### F08. No reserved block policy (`ext2_has_free_blocks`)
- **File**: `fs.rs:344` (only checks `sb_free_blocks == 0`)
- **Linux**: Non-root users blocked when free < `s_r_blocks_count` unless `CAP_SYS_RESOURCE` (`balloc.c:1158`)
- **Fix**: Add `has_free_blocks()` check before allocation using `reserved_blocks_count`, `def_resuid`, `def_resgid`

### F09. `find_entry` uses `i_blocks` bound instead of `i_size`
- **File**: `inode.rs` — `find_entry` block iteration bound
- **Linux**: Uses `dir_pages(dir)` from `i_size` (`dir.c:349`)
- **Fix**: Compute max blocks from `i_size.div_ceil(block_size)` instead of `i_blocks >> 3`

### F10. `free_inode` reads inode from disk just to check `is_dir`
- **File**: `fs.rs:567-569`
- **Linux**: Caller already has in-memory inode with `i_mode` (`ialloc.c:127`)
- **Fix**: Change `free_inode(ino)` signature to `free_inode(ino, is_dir)` — caller already knows the type

### F11. `free_inode` no clear-before-reuse ordering
- **File**: `fs.rs:578` (clears bitmap immediately)
- **Linux**: `clear_inode()` must happen BEFORE bitmap clear to prevent reuse race (`ialloc.c:89-103`)
- **Fix**: Ensure inode on-disk data is zeroed/invalidated before clearing bitmap bit

### F12. No `verify_chain` in `get_block` — stale data under concurrent modification
- **File**: `inode.rs` — `get_block` reads indirect chain without re-verification
- **Linux**: `ext2_get_branch` + `verify_chain` with `i_meta_lock` (`inode.c:234`)
- **Fix**: Add read lock on block tree; after reading chain, verify first pointer unchanged; retry on mismatch

---

## P2: Performance / Robustness — Correct but Suboptimal

### F13. No `i_dir_start_lookup` hint for directory lookups
- **File**: `inode.rs` — `find_entry` always scans from block 0
- **Linux**: Starts from cached hint, wraps around (`dir.c:356`)
- **Fix**: Add `dir_start_lookup: AtomicU32` to `InodeInner`, update on successful find

### F14. No Orlov / quadratic probing for inode allocation
- **File**: `fs.rs:440-481` — simple cyclic scan from parent group
- **Linux**: `find_group_orlov` for dirs, `find_group_other` with quadratic probing for files (`ialloc.c:199-418`)
- **Fix**: Implement `find_group_other` with quadratic probing as minimum; Orlov optional

### F15. `sync_metadata` does not recompute free counts from group descriptors
- **File**: `fs.rs:593-670`
- **Linux**: `ext2_sync_super` calls `ext2_count_free_blocks/inodes` to recompute from groups (`super.c:1290-1295`)
- **Fix**: Before writing superblock, sum `free_blocks_count`/`free_inodes_count` across all groups

### F16. No `statfs` / overhead calculation
- **File**: not implemented
- **Linux**: `ext2_statfs` computes overhead for reporting (`super.c:1000+`)
- **Fix**: Implement `FileSystem::statfs` with overhead calculation

---

## P3: Deferred — Requires New Subsystem

### F17. No `evict_inode` / orphan management
- **Linux**: `ext2_evict_inode` truncates+frees on nlink=0 (`inode.c:72-112`); orphan list via `s_last_orphan`
- **Phase**: 11 (orphan lifecycle)

### F18. No symlink support (fast/slow)
- **Linux**: Fast symlink in `i_block[]`, slow via data blocks (`inode.c`, `namei.c`)
- **Phase**: 8

### F19. No special file support (device nodes, fifo, socket)
- **Linux**: `init_special_inode` (`inode.c`)
- **Phase**: 8

### F20. No extended attributes / ioctl
- **Linux**: `ext2_xattr_get/set` (`xattr.c`), `ext2_ioctl` (`ioctl.c`)
- **Phase**: 10

---

## Suggested Fix Order

```
Stage 1 (P0 — must fix before any testing):
  F01 → F02 → F05 → F03 → F04 → F06

Stage 2 (P1 — functional correctness):
  F10 → F11 → F07 → F08 → F09 → F12

Stage 3 (P2 — performance/robustness):
  F13 → F15 → F14 → F16

Stage 4 (P3 — new phases):
  F17 → F18 → F19 → F20
```
