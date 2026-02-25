[PROMPT]
Audit and unify arithmetic safety across the entire ext2 module (T01).
Output: code changes for `super_block.rs`, `block_group.rs`, `inode.rs`.
Implementation logic MUST follow [SOURCE] Linux code.

Principle:
- Arithmetic that CANNOT overflow/underflow → plain `+` / `-` (remove unnecessary saturating).
- Arithmetic that CAN underflow from corrupted on-disk data → `saturating_sub` + `warn!`.
- Arithmetic on untrusted disk values that CAN overflow → `checked_*` returning `Err`.
- `checked_*.unwrap()` → NEVER (kernel must not panic on corrupt data).

[SOURCE]
percpu_counter_sub (dec free blocks)  → /root/linux/fs/ext2/balloc.c:1407
percpu_counter_add (inc free blocks)  → /root/linux/fs/ext2/balloc.c:567
group_adjust_blocks                   → /root/linux/fs/ext2/balloc.c:166
percpu_counter_dec (dec free inodes)  → /root/linux/fs/ext2/ialloc.c:523
percpu_counter_inc (inc free inodes)  → /root/linux/fs/ext2/ialloc.c:83
le16_add_cpu (group desc counters)    → /root/linux/fs/ext2/ialloc.c:79,528
inode_dec_link_count                  → /root/linux/fs/ext2/namei.c:312
ext2_inc_count / ext2_dec_count       → /root/linux/fs/ext2/namei.c:42-52
ext2_block_to_path                    → /root/linux/fs/ext2/inode.c:176
ext2_splice_branch (i_blocks update)  → /root/linux/fs/ext2/inode.c:595
ext2_free_branches (i_blocks update)  → /root/linux/fs/ext2/inode.c:1003

Note: Linux uses `percpu_counter_add/sub` (wrapping s64) for SB-level counters,
`le16_add_cpu` (wrapping u16) for group desc counters. Both are plain add/sub
with no overflow check — counters are hints, bitmap is source of truth.

[RELY]
/// SuperBlock in-memory representation (relevant fields).
pub struct SuperBlock {
    blocks_count: u32,        // total blocks on filesystem
    inodes_count: u32,        // total inodes on filesystem
    free_blocks_count: u32,   // free block counter — bitmap is truth
    free_inodes_count: u32,   // free inode counter — bitmap is truth
    mnt_count: u16,           // mount count
}

/// BlockGroup descriptor (relevant fields, all u16).
pub struct GroupDesc {
    free_blocks_count: u16,   // free blocks in this group — bitmap is truth
    free_inodes_count: u16,   // free inodes in this group — bitmap is truth
    used_dirs_count: u16,     // directories in this group
}

/// Inode descriptor (relevant fields).
pub struct InodeDesc {
    links_count: u16,         // hard link count
    blocks: u32,              // 512-byte sector count
    size: u64,                // file size in bytes
}

[GUARANTEE]

## ── A. super_block.rs: SuperBlock free counters (u32) ──

/// Increase free-block counter after releasing `count` blocks.
/// Arithmetic: plain `+=`. Result bounded by blocks_count (u32).
pub(super) fn inc_free_blocks(&mut self, count: u32);

/// Decrease free-block counter after allocating `count` blocks.
/// Arithmetic: `saturating_sub`. Corrupted on-disk counter may be < count.
pub(super) fn dec_free_blocks(&mut self, count: u32);

/// Increase free-inode counter after releasing one inode.
/// Arithmetic: plain `+= 1`. Result bounded by inodes_count (u32).
pub(super) fn inc_free_inodes(&mut self);

/// Decrease free-inode counter after allocating one inode.
/// Arithmetic: `saturating_sub`. Corrupted on-disk counter may be 0.
pub(super) fn dec_free_inodes(&mut self);

## ── B. block_group.rs: GroupDesc counters (u16) ──
## Status: ALL ALREADY CORRECT — no changes needed.
## All 6 methods (inc/dec_free_blocks, inc/dec_free_inodes, inc/dec_used_dirs)
## already use saturating_add/sub. This is appropriate because u16 counters
## loaded from disk are untrusted and can be inconsistent with bitmap state.

## ── C. inode.rs: links_count (u16) ──

## C1. mkdir — parent links_count += 1 (inode.rs:960)
##   Current: saturating_add(1). KEEP saturating_add.
##   Reason: links_count is u16 (max 65535). A directory with many subdirs
##   could theoretically approach the limit. Linux checks EXT2_LINK_MAX (65000)
##   before incrementing, but we don't have that check yet (T04). Until T04 is
##   done, saturating_add is the safety net. After T04, this becomes unreachable
##   but saturating_add costs nothing — keep it.

## C2. mkdir rollback / rmdir / unlink / rename — links_count -= N
##   Sites: inode.rs:914,920,967,976,992,1004,3491,3537,3656,3658,3691,3756,3758,3792
##   Current: ALL saturating_sub. KEEP saturating_sub.
##   Reason: links_count comes from disk. A corrupted inode could have
##   links_count=0 when we try to decrement. saturating_sub prevents wrap to 65535.

## C3. link — target links_count += 1 (inode.rs:3485)
##   Current: saturating_add(1). KEEP saturating_add.
##   Reason: same as C1 — no EMLINK guard yet (T04).

## ── D. inode.rs: desc.blocks (u32, 512-byte sector count) ──

## D1. alloc_branch — blocks = new_block_count (inode.rs:2715)
##   Current: direct assignment from checked_add result (inode.rs:2566).
##   The checked_add at :2566 returns Err on overflow. CORRECT — keep as-is.

## D2. truncate_blocks — blocks saturating_sub (inode.rs:1550,1818,1871)
##   Current: saturating_sub(sectors_per_block). KEEP saturating_sub.
##   Reason: during truncate, blocks comes from the inode on disk. A corrupted
##   inode could have blocks=0 while still having allocated block pointers.
##   saturating_sub prevents wrap to u32::MAX.

## ── E. Address/offset/geometry arithmetic ──
## Invariants from load_super_block validation:
##   blocks_per_group ≤ block_size*8 = 32768 (4K block)
##   inodes_per_group ≤ 32768
##   itb_per_group ≤ inodes_per_group ≤ 32768
##   blocks_count: u32;  first_data_block: 0 or 1
##   groups_count = blocks_count.div_ceil(blocks_per_group), fits u32
##   All callers of group_first/last_block_no pass group_idx < groups_count

## E1. group_first_block_no (super_block.rs:360-361)
##   group_idx * blocks_per_group + first_data_block
##   Result = first block of group, < blocks_count (u32). 100% safe.
##   → CHANGE to plain `*` / `+`.

## E2. group_last_block_no (super_block.rs:369-374)
##   Branch 1: total_blocks - 1. total_blocks ≥ 2 (load validates). Safe.
##   Branch 2: group_first_block_no(idx) + blocks_per_group - 1. < blocks_count. Safe.
##   groups_count - 1: groups_count ≥ 1. Safe.
##   → CHANGE to plain arithmetic.

## E3. data_block_valid (super_block.rs:389)
##   start_blk.checked_add(count.saturating_sub(1))
##   start_blk and count are EXTERNAL u32 — can be arbitrary.
##   → KEEP checked_add (必须).
##   count - 1: count == 0 already returned false above. Safe.
##   → CHANGE saturating_sub(1) to plain `- 1`.

## E4. mnt_count (super_block.rs:308)
##   u16 mount counter. CAN reach 65535 over many mounts.
##   Saturating is intentional semantic (cap at max, don't wrap).
##   → KEEP saturating_add.

## E5. load_super_block inodes validation (super_block.rs:212-215)
##   groups_count (u64, max ~4G) * inodes_per_group (u32, max 32768)
##   Max product = 2^32 * 2^15 = 2^47. u64 safe.
##   groups_count - 1: groups_count ≥ 1. Safe.
##   → CHANGE to plain `*` / `-`.

## E6a. bitmap validation offsets (block_group.rs:564,569)
##   offset + itb_per_group: both ≤ blocks_per_group ≤ 32768. u32 safe.
##   itb_per_group - 1: itb_per_group ≥ 1 (load validates). Safe.
##   → CHANGE to plain arithmetic.

## E6b. raw_inodes_size (block_group.rs:197)
##   inodes_per_group * inode_size: 32768 * 128 = 4MB. usize safe.
##   → CHANGE to plain `*`.

## E6c. inode_idx (block_group.rs:284)
##   ino - 1: ino ≥ 1 (ROOT_INO), caller validates ino ≥ first_ino. Safe.
##   → CHANGE to plain `- 1`.

## E6d. read/write_inode_desc offset (block_group.rs:773,784)
##   index_in_group * inode_size: same as E6b. Safe.
##   → CHANGE to plain `*`.

## E6e. bitmap capacity (block_group.rs:523)
##   max_bit + 1: max_bit ≤ blocks_per_group - 1 ≤ 32767. +1 = 32768. Safe.
##   → CHANGE to plain `+ 1`.

## E7a. alloc_blocks ret_block (block_group.rs:652)
##   first_block + run_start: run_start < group_size ≤ blocks_per_group.
##   Result ≤ last_block < blocks_count. Safe.
##   → CHANGE to plain `+`.

## E7b. alloc_blocks range end (block_group.rs:676)
##   ret_block + alloc_len: ≤ last_block + 1 ≤ blocks_count. Safe.
##   → CHANGE to plain `+`.

## E7c. free_blocks abs_start (block_group.rs:696)
##   first_block + bit: bit < group_size. Result ≤ last_block. Safe.
##   → CHANGE to plain `+`.

## E7d. free_blocks warn log (block_group.rs:712)
##   abs_start + (idx - range_start): ≤ last_block. Safe.
##   → CHANGE to plain `+`.

## E8. overlaps_system_zone (block_group.rs:796,823)
##   start + (count-1): start is from disk data, CAN be arbitrary.
##   → KEEP checked_add (必须).
##   count - 1: callers guarantee count ≥ 1 (alloc_len > 0, group_count > 0). Safe.
##   zone_len - 1: zone_len == 0 early-returned above. Safe.
##   → CHANGE saturating_sub(1) to plain `- 1`.

## E9. inode read/write offset+len (inode.rs:375,485,566,601,1261,1301,1344,1381)
##   offset + len: offset and len are USER-SUPPLIED, can be arbitrary usize.
##   → KEEP checked_add (必须).
##   file_size - offset: guarded by `offset >= file_size → return 0`. Safe.
##   → CHANGE saturating_sub to plain `-`.

## E10. block_to_path (inode.rs:2346,2354,2363,2364)
##   block - direct_blocks / indirect_blocks / double_blocks:
##   Each guarded by preceding `block < threshold` failing → block ≥ threshold. Safe.
##   → CHANGE to plain `-`.
##   ptrs_bits * 2 (:2364): ptrs_bits = log2(block_size/4) = 10 (4K). 10*2=20. Safe.
##   → CHANGE to plain `*`.

## E11. truncate partial level (inode.rs:1580,1690,1716,1724)
##   depth - 1: depth ≥ 1 in all these paths (case 2 = indirect, depth ≥ 2). Safe.
##   path.depth - 1 - partial: partial ≤ depth - 1. Safe.
##   path.depth - 1 - level: level ≤ depth - 1. Safe.
##   k - 1 (:1580): k = path.depth, and we're in case 2 so depth ≥ 2. Safe.
##   → CHANGE to plain `-`.

## E12. directory iteration offsets (inode.rs:2275,2276,2279,2284,2300,2309)
##   block_offset + inner_off: both < file_size (usize). Safe.
##   next_offset - block_offset: next_offset ≥ block_offset (cursor moves forward). Safe.
##   current_offset - offset: current_offset ≥ offset (initialized from offset). Safe.
##   → CHANGE to plain `+` / `-`.

## E13. indirect pointer offsets (inode.rs:1604,1622,1663,1699,1727,1846,etc.)
##   index * sizeof(u32): index < ptrs_per_block = block_size/4 = 1024.
##   1024 * 4 = 4096 = block_size. usize safe.
##   ptr_offset + sizeof(u32): ≤ block_size. Safe.
##   byte + sizeof(u32) (:1622): loop cursor within block. Safe.
##   → CHANGE to plain `*` / `+`.

## E14. fs.rs free_blocks loop (fs.rs:431,435,444,445)
##   current - group_first: current ≥ group_first (loop invariant, validated at :432). Safe.
##   group_size - bit: bit < group_size (validated at :432). Safe.
##   current + group_count: current + group_count ≤ start + total_count ≤ u32. Safe.
##   remaining - group_count: group_count ≤ remaining (from .min()). Safe.
##   → CHANGE to plain `-` / `+`.

## E15. alloc_inode ino (fs.rs:492-494)
##   group_idx * inodes_per_group + inode_idx + 1
##   Result ≤ total_inodes (u32), validated at :495. Safe.
##   → CHANGE to plain `*` / `+`.

## E16. sync_all_inodes accumulator (fs.rs:730-731)
##   freed_inodes += result.freed_inodes: total ≤ total_inodes (u32). Safe.
##   freed_dirs += result.freed_dirs: total ≤ total_inodes. Safe.
##   → CHANGE to plain `+`.

## E17. alloc_branch alloc_goal (inode.rs:2602)
##   last_allocated_block + 1: block number < blocks_count (u32).
##   Edge case: last block = blocks_count - 1, +1 = blocks_count. Fits u32.
##   Actually stored as Bid (u64), so even blocks_count as u64 + 1 is fine.
##   → CHANGE to plain `+ 1`.

## E18. alloc_branch alloc_len (inode.rs:2594)
##   allocated.end - allocated.start: end > start (range from alloc_blocks). Safe.
##   → CHANGE to plain `-`.

## E19. blks_to_allocate (inode.rs:2501-2502)
##   depth - 1 - partial_level: depth ≤ 4, partial_level ≤ depth - 1. Safe.
##   → CHANGE to plain `-`.

## E20. branch chain lookup (inode.rs:2403,2687,2738)
##   path.depth - 1: depth ≥ 1 (at least direct). Safe.
##   → CHANGE to plain `- 1`.

## E21. read/write_at current_offset advance (inode.rs:1316,1404)
##   current_offset + bytes_this_block: ≤ end (loop bound). Safe.
##   → CHANGE to plain `+`.

## E22. dir entry dot/dotdot rec_len (inode.rs:844)
##   block_size - dot_len: dot_len = 12, block_size = 4096. Safe.
##   → CHANGE to plain `-`.

## E23. symlink read_len (inode.rs:334)
##   MAX_FAST_SYMLINK_LEN - 1: constant 60 - 1 = 59. Safe.
##   → CHANGE to plain `- 1`.

## E24. free_branches depth (inode.rs:1861)
##   depth - 1: depth ≥ 1 (recursive call only when depth > 0). Safe.
##   → CHANGE to plain `- 1`.

## E25. start_idx in truncate tail-free (inode.rs:1723)
##   offsets[level] + 1: offsets[level] < ptrs_per_block = 1024. +1 ≤ 1024. Safe.
##   → CHANGE to plain `+ 1`.

## E26. keep_bytes in truncate (inode.rs:1604)
##   keep_entries * sizeof(u32): keep_entries < ptrs_per_block = 1024. Same as E13. Safe.
##   → CHANGE to plain `*`.

## E27. dir block iteration (inode.rs:1993,2003,2185,2252,2361,2776)
##   block_idx * block_size: block_idx < data_blocks ≤ file_size/block_size.
##   Result < file_size (usize). Safe.
##   size - block_offset: block_offset < size (loop guard). Safe.
##   → CHANGE to plain `*` / `-`.

[SPECIFICATION]

## ── Changes required (only 2 methods in super_block.rs) ──

## A1. SuperBlock::inc_free_blocks  (super_block.rs:470-472)
Current:  self.free_blocks_count = self.free_blocks_count.checked_add(count).unwrap();
Change:   self.free_blocks_count += count;
Pre:      count is the number of blocks actually freed in bitmap. count > 0.
          free_blocks_count + count <= blocks_count <= u32::MAX.
Post:     free_blocks_count' == free_blocks_count + count.
Proof:    Caller is fs.rs:441. `freed` comes from BlockGroup::free_blocks which
          returns the number of bits actually cleared in the bitmap. The bitmap
          has at most blocks_per_group bits (~2^15 for 4K blocks). The sum
          free_blocks_count + freed <= blocks_count (u32). Cannot overflow.
Invariant: No panic. No saturation.

## A2. SuperBlock::dec_free_blocks  (super_block.rs:475-477)
Current:  self.free_blocks_count = self.free_blocks_count.checked_sub(count).unwrap();
Change:   saturating_sub + warn! on clamp.
Pre:      count is the number of blocks actually allocated in bitmap. count > 0.
Post (normal):  free_blocks_count >= count → free_blocks_count' == free_blocks_count - count.
Post (corrupt): free_blocks_count < count  → free_blocks_count' == 0, warn! emitted.
Proof:    Normal path: caller checks sb_free_blocks >= alloc_len (block_group.rs:659).
          Corrupt path: on-disk free_blocks_count was loaded at mount and may be
          smaller than bitmap's actual free count. checked_sub().unwrap() panics here.
Invariant: No panic.

## ── No changes required ──

## A3. SuperBlock::inc_free_inodes  (super_block.rs:485-487)
Current:  self.free_inodes_count += 1;
Status:   ALREADY CORRECT. Plain += 1. Cannot overflow (bounded by inodes_count).

## A4. SuperBlock::dec_free_inodes  (super_block.rs:493-497)
Current:  debug_assert!(self.free_inodes_count > 0); self.free_inodes_count -= 1;
Status:   ACCEPTABLE. Normal path has free_inodes == 0 → ENOSPC guard (fs.rs:472).
          In release mode, corrupt counter wraps to u32::MAX — same risk class as
          dec_free_blocks but single-decrement makes it less likely to cause harm
          before next sync overwrites the counter.
Option:   Change to saturating_sub(1) for consistency with dec_free_blocks.
          This is a style choice, not a correctness fix. Either way is defensible.

## B. block_group.rs — NO CHANGES. All 6 counter methods already saturating.

## C. inode.rs links_count — NO CHANGES. All sites already saturating_add/sub.

## D. inode.rs desc.blocks — NO CHANGES.
##    D1 (alloc) uses checked_add → Err. Correct.
##    D2 (truncate) uses saturating_sub. Correct.

## E. Address/offset arithmetic — ALL 100% safe, CHANGE to plain arithmetic.
##    Exception: E3, E8 checked_add KEEP (external/disk input can be arbitrary).
##    Exception: E4 mnt_count saturating KEEP (intentional cap semantic).
##    See GUARANTEE section E1-E27 for per-site proof.

[DIFF]
Linux: All counter updates (SB percpu_counter, group le16_add_cpu, inode i_blocks)
  use plain wrapping arithmetic with no overflow/underflow check.
  → Asterinas: mixed strategy per risk level.
  Reason: Linux operates on s64 (percpu) or under spinlock with wrapping semantics
  that are benign on 64-bit. Asterinas uses u32/u16 directly where wrapping to
  MAX is far more damaging (e.g. free_blocks_count wrapping to u32::MAX would
  make the FS appear to have 4 billion free blocks).

Linux: inc_free_blocks / dec_free_blocks are symmetric plain add/sub.
  → Asterinas: inc uses plain `+=` (cannot overflow), dec uses saturating_sub
  (corrupt counter could underflow).
  Reason: asymmetric risk — overflow requires free_blocks_count > blocks_count
  which is impossible after bitmap free; underflow requires free_blocks_count <
  actual bitmap free count which IS possible with corrupt on-disk data.

[TEST]

## SuperBlock::inc_free_blocks (A1 — changed: checked_add().unwrap() → plain +=)
- Free 1 block when free_blocks_count=0 → becomes 1
- Free N blocks → increases by exactly N
- Free blocks until free_blocks_count == blocks_count → no overflow, no panic

## SuperBlock::dec_free_blocks (A2 — changed: checked_sub().unwrap() → saturating_sub)
- Allocate 1 block when free_blocks_count > 0 → decreases by 1
- Allocate N blocks when free_blocks_count >= N → decreases by N
- Allocate 1 when free_blocks_count == 0 (corrupt) → stays 0, no panic
- Allocate N when free_blocks_count < N (corrupt) → clamps to 0, no panic

## SuperBlock::inc_free_inodes (A3 — no change, already plain +=)
- Free 1 inode → increases by 1

## SuperBlock::dec_free_inodes (A4 — optional: -= 1 → saturating_sub for consistency)
- Allocate 1 inode when free_inodes_count > 0 → decreases by 1
- Allocate when free_inodes_count == 0 (corrupt) → stays 0 if saturating, wraps if raw

## BlockGroup counters (B — no change, already all saturating)
- Existing tests in block_group.rs::free_block_counter_update_marks_dirty cover this.

## Inode links_count (C — no change, already all saturating)
- Covered by existing link/unlink/mkdir/rmdir tests.

## Inode desc.blocks (D — no change)
- D1 alloc: covered by existing alloc_branch tests (checked_add → Err).
- D2 truncate: covered by existing truncate tests (saturating_sub).
