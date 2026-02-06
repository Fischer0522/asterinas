[PROMPT]
Provide additions to `kernel/src/fs/ext2/fs.rs` and `kernel/src/fs/ext2/block_group.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_new_blocks             → /root/linux/fs/ext2/balloc.c:1208
ext2_free_blocks            → /root/linux/fs/ext2/balloc.c:482
ext2_try_to_allocate        → /root/linux/fs/ext2/balloc.c:682
find_next_usable_block      → /root/linux/fs/ext2/balloc.c:692
ext2_has_free_blocks        → /root/linux/fs/ext2/balloc.c:1158
ext2_data_block_valid       → /root/linux/fs/ext2/balloc.c:1177
ext2_group_first_block_no   → /root/linux/fs/ext2/ext2.h:798
ext2_group_last_block_no    → /root/linux/fs/ext2/ext2.h:804

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::block_group::BlockGroup;
```

```rust
use super::super_block::SuperBlock;
```

```rust
use crate::fs::utils::FsEventSubscriberStats;
```

```rust
/// The Ext2 filesystem (core state holder).
#[derive(Debug)]
pub struct Ext2 {
    /// Backing block device.
    block_device: Arc<dyn BlockDevice>,
    /// Superblock with dirty tracking.
    super_block: RwMutex<Dirty<SuperBlock>>,
    /// Block group descriptors and caches.
    block_groups: Vec<BlockGroup>,
    /// Inodes per group.
    inodes_per_group: u32,
    /// Blocks per group.
    blocks_per_group: u32,
    /// Inode size in bytes.
    inode_size: usize,
    /// Block size in bytes.
    block_size: usize,
    /// Group descriptor table segment.
    group_descriptors_segment: USegment,
    /// FS event stats for VFS.
    fs_event_subscriber_stats: FsEventSubscriberStats,
    /// Weak self reference for inode back-pointers.
    self_ref: Weak<Ext2>,
}
```

```rust
#[derive(Debug)]
pub struct BlockGroup {
    idx: usize,
    desc: RwMutex<Dirty<GroupDesc>>,
}
```

```rust
#[derive(Clone, Copy, Debug)]
pub(super) struct GroupDesc {
    pub block_bitmap: Bid,
    pub inode_bitmap: Bid,
    pub inode_table: Bid,
    pub free_blocks_count: u16,
    pub free_inodes_count: u16,
    pub used_dirs_count: u16,
}
```

[GUARANTEE]
impl BlockGroup {
    /// Decreases the free-block counter for this group.
    ///
    /// # Arguments
    /// * `count` - Number of blocks to subtract.
    pub(super) fn dec_free_blocks(&self, count: u16);

    /// Increases the free-block counter for this group.
    ///
    /// # Arguments
    /// * `count` - Number of blocks to add.
    pub(super) fn inc_free_blocks(&self, count: u16);
}

impl Ext2 {
    /// Allocates up to `count` contiguous blocks.
    ///
    /// # Arguments
    /// * `count` - Requested number of contiguous blocks.
    ///
    /// # Returns
    /// * `Ok(Range<u32>)` - Allocated block range `[start, end)` with length >= 1.
    /// * `Err(ENOSPC)` - No free blocks available.
    /// * `Err(EIO)` - I/O failure or metadata inconsistency.
    pub(super) fn alloc_blocks(&self, count: u32) -> Result<Range<u32>>;

    /// Frees a range of blocks starting at `start`.
    ///
    /// # Arguments
    /// * `start` - Filesystem-wide start block number.
    /// * `count` - Number of blocks to free.
    ///
    /// # Returns
    /// * `Ok(())` - Blocks freed (best effort for already-free blocks).
    /// * `Err(EIO)` - I/O failure or invalid data zone range.
    pub(super) fn free_blocks(&self, start: u32, count: u32) -> Result<()>;
}

[SPECIFICATION]
Pre (BlockGroup::dec_free_blocks / inc_free_blocks):
- `count` fits within `u16` and does not underflow/overflow the group counter.

Post (BlockGroup::dec_free_blocks):
- Decreases `free_blocks_count` by `count` and marks the descriptor dirty.

Post (BlockGroup::inc_free_blocks):
- Increases `free_blocks_count` by `count` and marks the descriptor dirty.

Pre (alloc_blocks):
- `count > 0`.
- `self.super_block` is validated and `self.block_groups` is loaded.

Post (alloc_blocks: success):
- Returns `Ok(range)` where:
  - `range.start` is a filesystem-wide block number.
  - `range.end - range.start` is the actual allocated count `n`, with `1 <= n <= count`.
  - All blocks in `range` are in the data zone (`sb.data_block_valid(range.start, n)` is true).
  - `range` is contiguous and contained in a single block group.
- Allocation search order (per Linux `ext2_new_blocks` + `ext2_try_to_allocate` + `find_next_usable_block`):
  1. Scan block groups from index 0 to `groups_count - 1` in order.
  2. For each group with `free_blocks_count > 0`, read its bitmap into a buffer and
     wrap it as an `IdBitmap` with length equal to the group size.
  3. Attempt to allocate a consecutive range using `IdBitmap::alloc_consecutive`:
     - Start with `req = min(count, group_size)`.
     - If allocation fails, halve `req` and retry until `req == 0`.
  4. Once a range is obtained, convert it to filesystem-wide block numbers and proceed.
- After allocation, if the chosen range overlaps any system zone block
  (block bitmap, inode bitmap, inode table range for that group), treat as
  metadata inconsistency and retry allocation within the same group using the
  next free region; if no valid region exists in any group, return `Err(EIO)`.
- On success, update counters:
  - `BlockGroup::dec_free_blocks(n as u16)` for the chosen group.
  - `SuperBlock::dec_free_blocks(n)`.
- Persist bitmap changes by writing the modified bitmap block back to
  `block_bitmap_bid` via `BlockDevice::write_bytes`.

Post (alloc_blocks: failure):
- Returns `Err(ENOSPC)` if no free blocks exist in any group.
- Returns `Err(EIO)` if any bitmap read/write fails or if metadata inconsistency
  prevents allocation.
- If allocation fails, on-disk and in-memory counters remain unchanged.

Pre (free_blocks):
- If `count == 0`, return `Ok(())` without I/O.
- `self.super_block` is validated and `self.block_groups` is loaded.

Post (free_blocks: success):
- Validates the data range via `sb.data_block_valid(start, count)`; if false, returns `Err(EIO)`.
- Frees blocks across group boundaries as needed:
  - For each affected group, read its block bitmap into a buffer.
  - Compute group-relative `bit` and per-group `group_count` to free.
  - If the range overlaps group system zones (block bitmap, inode bitmap, inode table),
    return `Err(EIO)` and stop.
  - For each bit in the per-group range:
    - If the bit is set, clear it and increment `freed`.
    - If the bit is already clear, log metadata inconsistency and continue.
  - Write the bitmap back to disk.
  - Update counters with the actual number of bits cleared:
    - `BlockGroup::inc_free_blocks(freed as u16)`.
    - `SuperBlock::inc_free_blocks(freed)`.
- Returns `Ok(())` after all groups are processed.

Post (free_blocks: failure):
- Returns `Err(EIO)` for bitmap I/O failures or invalid ranges.

Invariant:
- Bitmap bit 0 means free, bit 1 means allocated (LSB0).
- Group boundaries are computed using `group_first_block_no`/`group_last_block_no`.

[DIFF]
Linux: Uses quota accounting, reservation windows, per-cpu counters, and buffer_head dirtying.
  → Asterinas: No quota/reservation/per-cpu counters; direct bitmap read/write via `BlockDevice`.
  Reason: Asterinas lacks quota/reservation infrastructure and buffer_head layer.

Linux: Uses `goal`-based allocation with `find_next_usable_block` heuristics.
  → Asterinas: Omits the `goal` parameter and uses `IdBitmap::alloc_consecutive` with halving fallback.
  Reason: Simpler allocator for initial bring-up; preserves contiguous allocation but not Linux placement.

Linux: Enforces reserved-block policy in `ext2_has_free_blocks` using `s_r_blocks_count`,
resuid/resgid, and caller capabilities.
  → Asterinas: Reserved-block policy not implemented in this phase.
  Reason: Credential/resuid/resgid plumbing for Ext2 is not wired yet; deferred to a later module.

Linux: On system-zone overlap during allocation, logs and retries with possibly corrupted bitmaps.
  → Asterinas: Treats overlap as metadata inconsistency and retries; if no valid region, returns `EIO`.
  Reason: Provide explicit error reporting in absence of kernel error logger.

Linux: On freeing an already-free block, logs an error and continues.
  → Asterinas: Logs metadata inconsistency and continues freeing remaining bits.
  Reason: Keep allocator progress while preserving corruption visibility.
