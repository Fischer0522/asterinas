# Ext2 Fix-List

Grounded in diff reports, spec review, and actual code inspection.
Each item includes: severity, file:line, Linux reference, and fix description.

---

## P0: Correctness — Will Cause Panic / Data Corruption

### F01. Counter arithmetic panics kernel + inconsistent across paths
- **File**: `super_block.rs:446-472`, `block_group.rs:399-432`
- **Symptom**: `SuperBlock` uses `checked_add().unwrap()` (panics), raw `+= 1` (wraps), `debug_assert!` + `-= 1` (wraps in release). `BlockGroup` uses `saturating_*` (silently clamps). Three different strategies across the same subsystem.
- **Linux**: `le16_add_cpu()` under spinlock, never panics; trusts callers (`balloc.c:168`). Counter is redundant cache of bitmap — bitmap is source of truth.
- **Fix**: Unify all counter methods (SuperBlock + BlockGroup) to `checked_*` + `log::warn!` + clamp. No panic, no silent wrap, corruption is logged.

### F02. `create_inode` missing uid/gid
- **File**: `fs.rs:503-527` (RawInode literal has `uid: 0, gid: 0`)
- **Linux**: `ext2_new_inode` sets uid from `current_fsuid()`, gid from parent dir or `current_fsgid()` with SGID inheritance (`ialloc.c:539+`)
- **Impact**: All new files owned by root:root, permission model broken for non-root users
- **Fix**: Fill `uid`/`gid` from current credentials; handle SGID inheritance from parent dir

### F03. `inodes_count` validation too strict — rejects valid filesystems
- **File**: `super_block.rs:208-210`
- **Symptom**: `groups_count * inodes_per_group != inodes_count` → EINVAL
- **Linux**: Does NOT enforce exact equality; last group may have fewer inodes (`super.c:960-980`)
- **Fix**: Change to `inodes_count <= groups_count * inodes_per_group && inodes_count > (groups_count - 1) * inodes_per_group`

### F04. No `ext2_setup_super` on mount
- **File**: `super_block.rs:280-297` (`load_super_block`)
- **Linux**: `ext2_setup_super` increments `mnt_count`, clears `VALID_FS` state, sets `wtime` (`super.c:645`)
- **Fix**: After validation in `load_super_block` (for rw mount): `mnt_count += 1`, `state &= ~VALID_FS`, `wtime = now()`, write back to disk

---

## P1: Functional — Wrong Behavior Under Normal Use

### F05. `create_inode` missing timestamps
- **File**: `fs.rs:503-527` (RawInode literal has `atime/ctime/mtime: 0`)
- **Linux**: `ext2_new_inode` sets all three from `current_time()` (`ialloc.c:539+`)
- **Impact**: `ls -l` shows 1970, `make` dependency tracking breaks
- **Fix**: Fill `atime`/`ctime`/`mtime` from `UnixTime::now()`

### F06. Timestamps not updated on directory mutations
- **File**: `inode.rs` — `add_entry`/`delete_entry`/`mkdir`/`rmdir` paths
- **Linux**: `ext2_add_link` sets `dir->i_mtime = dir->i_ctime = current_time()` (`dir.c:554`); `ext2_delete_entry` does same (`dir.c:608`)
- **Fix**: After successful dir mutation, set `ctime`/`mtime` = `UnixTime::now()` on parent inode, mark dirty

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

### F11. No `verify_chain` in `get_block` — stale data under concurrent modification
- **File**: `inode.rs` — `get_block` reads indirect chain without re-verification
- **Linux**: `ext2_get_branch` + `verify_chain` with `i_meta_lock` (`inode.c:234`)
- **Fix**: Add read lock on block tree; after reading chain, verify first pointer unchanged; retry on mismatch

---

## P2: Performance / Robustness — Correct but Suboptimal

### F12. No `i_dir_start_lookup` hint for directory lookups
- **File**: `inode.rs` — `find_entry` always scans from block 0
- **Linux**: Starts from cached hint, wraps around (`dir.c:356`)
- **Fix**: Add `dir_start_lookup: AtomicU32` to `InodeInner`, update on successful find

### F13. No Orlov / quadratic probing for inode allocation
- **File**: `fs.rs:440-481` — simple cyclic scan from parent group
- **Linux**: `find_group_orlov` for dirs, `find_group_other` with quadratic probing for files (`ialloc.c:199-418`)
- **Fix**: Implement `find_group_other` with quadratic probing as minimum; Orlov optional

### F14. `sync_metadata` does not recompute free counts from group descriptors
- **File**: `fs.rs:593-670`
- **Linux**: `ext2_sync_super` calls `ext2_count_free_blocks/inodes` to recompute from groups (`super.c:1290-1295`)
- **Fix**: Before writing superblock, sum `free_blocks_count`/`free_inodes_count` across all groups

### F15. No `statfs` / overhead calculation
- **File**: not implemented
- **Linux**: `ext2_statfs` computes overhead for reporting (`super.c:1000+`)
- **Fix**: Implement `FileSystem::statfs` with overhead calculation

---

## P3: Deferred — Requires New Subsystem

### F16. No `evict_inode` / orphan management
- **Linux**: `ext2_evict_inode` truncates+frees on nlink=0 (`inode.c:72-112`); orphan list via `s_last_orphan`
- **Phase**: 11 (orphan lifecycle)
- **Note**: `free_inode` clear-before-reuse ordering (`ialloc.c:89-103`) is also deferred here. Current Asterinas uses `Weak<Inode>` in inode cache, so the Linux race (stale strong-ref in VFS hash) cannot occur. The ordering guarantee becomes necessary only when evict_inode is implemented.

### F17. No symlink support (fast/slow)
- **Linux**: Fast symlink in `i_block[]`, slow via data blocks (`inode.c`, `namei.c`)
- **Phase**: 8

### F18. No special file support (device nodes, fifo, socket)
- **Linux**: `init_special_inode` (`inode.c`)
- **Phase**: 8

### F19. No extended attributes / ioctl
- **Linux**: `ext2_xattr_get/set` (`xattr.c`), `ext2_ioctl` (`ioctl.c`)
- **Phase**: 10

### F20. `create_inode` missing generation
- **Linux**: `ext2_new_inode` sets `i_generation = sbi->s_next_generation++` (`ialloc.c:565`)
- **Impact**: Only affects NFS file handle validation and `EXT2_IOC_GETVERSION` ioctl — no local correctness impact
- **Phase**: 10 (ioctl) or when NFS export is needed

---

## Suggested Fix Order

```
Stage 1 (P0 — must fix before any testing):
  F01 → F02 → F03 → F04

Stage 2 (P1 — functional correctness):
  F05 → F06 → F10 → F09 → F07 → F08 → F11

Stage 3 (P2 — performance/robustness):
  F12 → F14 → F13 → F15

Stage 4 (P3 — new phases):
  F16 → F17 → F18 → F19 → F20
```
