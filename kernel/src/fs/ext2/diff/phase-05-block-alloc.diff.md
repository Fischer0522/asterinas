# Phase 05 Diff Report: Block Allocation

## Scope

- Rust: `kernel/src/fs/ext2/fs.rs`
- Linux references:
  - `/root/linux/fs/ext2/balloc.c:1208` (`ext2_new_blocks`)
  - `/root/linux/fs/ext2/balloc.c:482` (`ext2_free_blocks`)
  - `/root/linux/fs/ext2/balloc.c:1158` (`ext2_has_free_blocks`)

---

## 1) No Goal-Based Allocation

- **Linux**: `ext2_new_blocks()` takes a `goal` parameter — a hint block number near which allocation should occur. It first checks if the goal block itself is free, then searches within 32 blocks of the goal, then falls back to byte-aligned search within the group.
  - Reference: `/root/linux/fs/ext2/balloc.c:1208`
- **Asterinas**: `alloc_blocks()` takes only a `count` parameter. It scans groups sequentially from group 0 and uses `bitmap.alloc_consecutive(req)` with halving fallback. No goal hint.
  - Rust: `fs.rs:350`
- **Impact**: No locality optimization. Blocks for the same file may be scattered across groups.

## 2) No Reservation Window

- **Linux**: Uses a per-inode reservation window (`ext2_reserve_window_node`) backed by a red-black tree. The window reserves a contiguous range of blocks for an inode, reducing fragmentation for sequential writes.
  - Reference: `/root/linux/fs/ext2/balloc.c:183-474`
- **Asterinas**: No reservation window mechanism. Each allocation is a standalone bitmap scan.
  - Rust: `fs.rs:802` (`try_alloc_in_group`)
- **Impact**: Sequential writes to the same file will not benefit from pre-reserved contiguous ranges.

## 3) No `ext2_has_free_blocks` Reserved Block Policy

- **Linux**: Before allocation, `ext2_has_free_blocks()` checks whether the caller has permission to use reserved blocks. Non-privileged users cannot allocate when free blocks fall below `s_r_blocks_count`, unless they have `CAP_SYS_RESOURCE` or match `s_resuid`/`s_resgid`.
  - Reference: `/root/linux/fs/ext2/balloc.c:1158`
- **Asterinas**: Only checks `sb_free_blocks == 0` for ENOSPC. No reserved block policy.
  - Rust: `fs.rs:376`
- **Impact**: Root-reserved blocks can be consumed by unprivileged users.

---

## Spec-vs-Linux Supplement (from spec review)

### S1) phase-05-block-alloc.spec: Group Scan Order

- **Spec**: Scans block groups from index 0 to `groups_count - 1` in order.
- **Linux**: `ext2_new_blocks()` computes `goal_group = (goal - le32_to_cpu(es->s_first_data_block)) / EXT2_BLOCKS_PER_GROUP(sb)` and starts scanning from that group.
  - Reference: `/root/linux/fs/ext2/balloc.c:1260-1280`
- **Diff**: Asterinas always starts from group 0. Linux starts from the goal group. This causes Asterinas to favor low-numbered groups, leading to uneven wear and fragmentation.

### S2) phase-05-block-alloc.spec: Halving Fallback vs Linux Retry Strategy

- **Spec**: Uses `IdBitmap::alloc_consecutive` with halving fallback: start with `req = min(count, group_size)`, halve on failure until `req == 0`.
- **Linux**: `ext2_try_to_allocate()` first checks the goal block, then searches nearby (within 32 blocks), then uses `find_next_usable_block()` for byte-aligned search. Falls back to `ext2_try_to_allocate_with_rsv()` with reservation window.
  - Reference: `/root/linux/fs/ext2/balloc.c:682-750`
- **Diff**: Completely different allocation strategies. Linux's approach is locality-aware and fine-grained. Asterinas's halving approach may allocate fewer blocks than available (e.g., 8 free blocks scattered → halving from 8 to 4 to 2 to 1).

### S3) phase-05-block-alloc.spec: System Zone Overlap Check

- **Spec**: After allocation, checks if the chosen range overlaps any system zone block (block bitmap, inode bitmap, inode table). Treats overlap as metadata inconsistency and retries.
- **Linux**: `ext2_data_block_valid()` performs the same check. On overlap, logs error via `ext2_error()` and returns 0 (invalid).
  - Reference: `/root/linux/fs/ext2/balloc.c:1177`
- **Status**: Aligned in intent. Asterinas retries; Linux logs and fails the allocation.
