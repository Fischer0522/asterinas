[PROMPT]
Provide additions to `kernel/src/fs/ext2/fs.rs` and `kernel/src/fs/ext2/block_group.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_new_inode          → /root/linux/fs/ext2/ialloc.c:419
ext2_free_inode         → /root/linux/fs/ext2/ialloc.c:79
read_inode_bitmap       → /root/linux/fs/ext2/ialloc.c:31
find_group_orlov        → /root/linux/fs/ext2/ialloc.c:160
find_group_other        → /root/linux/fs/ext2/ialloc.c:260

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::block_group::BlockGroup;
```

```rust
use super::inode::InodeDesc;
```

```rust
use super::super_block::SuperBlock;
```

```rust
use crate::fs::utils::IdBitmap;
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
/// The root inode number (Linux EXT2_ROOT_INO).
pub const ROOT_INO: u32 = 2;
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

```rust
impl Ext2 {
    pub(super) fn read_inode_desc(&self, ino: u32) -> Result<InodeDesc>;
}
```

[GUARANTEE]
impl BlockGroup {
    /// Decreases the free-inode counter for this group.
    ///
    /// # Arguments
    /// * `count` - Number of inodes to subtract.
    pub(super) fn dec_free_inodes(&self, count: u16);

    /// Increases the free-inode counter for this group.
    ///
    /// # Arguments
    /// * `count` - Number of inodes to add.
    pub(super) fn inc_free_inodes(&self, count: u16);

    /// Increases the used-dirs counter for this group.
    pub(super) fn inc_used_dirs(&self);

    /// Decreases the used-dirs counter for this group.
    pub(super) fn dec_used_dirs(&self);
}

impl Ext2 {
    /// Allocates a new inode number.
    ///
    /// # Arguments
    /// * `parent_ino` - Parent directory inode number (1-based).
    /// * `inode_type` - Type of inode to allocate.
    ///
    /// # Returns
    /// * `Ok(u32)` - Allocated inode number (1-based).
    /// * `Err(ENOSPC)` - No free inode available.
    /// * `Err(EIO)` - I/O failure or metadata inconsistency.
    pub(super) fn alloc_inode(&self, parent_ino: u32, inode_type: InodeType) -> Result<u32>;

    /// Frees an inode by number.
    ///
    /// # Arguments
    /// * `ino` - Inode number to free (1-based).
    ///
    /// # Returns
    /// * `Ok(())` - Inode freed.
    /// * `Err(EIO)` - I/O failure or invalid inode number.
    pub(super) fn free_inode(&self, ino: u32) -> Result<()>;
}

[SPECIFICATION]
Pre (BlockGroup::dec_free_inodes / inc_free_inodes):
- `count` fits within `u16` and does not underflow/overflow the group counter.

Post (BlockGroup::dec_free_inodes):
- Decreases `free_inodes_count` by `count` and marks the descriptor dirty.

Post (BlockGroup::inc_free_inodes):
- Increases `free_inodes_count` by `count` and marks the descriptor dirty.

Post (BlockGroup::inc_used_dirs / dec_used_dirs):
- Adjusts `used_dirs_count` by +/-1 and marks the descriptor dirty.

Pre (alloc_inode):
- `parent_ino` is within `[ROOT_INO, sb.total_inodes()]`.
- `self.super_block` is validated and `self.block_groups` is loaded.
- If `sb.free_inodes_count() == 0`, return `Err(ENOSPC)` without bitmap I/O.

Post (alloc_inode: success):
- Returns `Ok(ino)` where:
  - `ino` is within `[sb.first_ino(), sb.total_inodes()]`.
  - The inode bit for `ino` is set in the owning group bitmap.
- Allocation search order (simplified from Linux):
  1. Compute `parent_group = (parent_ino - 1) / inodes_per_group`.
  2. Scan groups in cyclic order starting at `parent_group` for `groups_count` iterations.
  3. Skip groups with `free_inodes_count == 0`.
  4. For each candidate group:
     - Load bitmap via `BlockGroup::load_inode_bitmap(self, sb)`.
     - Allocate one inode with `IdBitmap::alloc()`.
     - If allocation fails but `free_inodes_count > 0`, mark metadata inconsistency
       and continue scanning other groups.
     - If allocation succeeds, compute filesystem inode number:
       `ino = group_idx * inodes_per_group + inode_idx + 1`.
     - Return Err(EIO) if `ino` is not within `[sb.first_ino(), sb.total_inodes()]`.
     - Persist bitmap via `BlockDevice::write_bytes(group.inode_bitmap_bid())`.
     - Update counters:
       - `BlockGroup::dec_free_inodes(1)`.
       - `SuperBlock::dec_free_inodes()`.
       - If `inode_type.is_directory()`, `BlockGroup::inc_used_dirs()`.
- If any bitmap read/write fails, return `Err(EIO)`.

Post (alloc_inode: failure):
- Returns `Err(ENOSPC)` if no free inode exists in any group.
- Returns `Err(EIO)` on I/O error or if metadata inconsistency was detected during scanning.
- On failure, on-disk and in-memory counters remain unchanged.

Pre (free_inode):
- `ino` is within `[sb.first_ino(), sb.total_inodes()]`.

Post (free_inode: success):
- Determines `is_dir` by reading inode descriptor via `read_inode_desc(ino)` and parsing
  `raw.mode` with `InodeType::from_raw_mode`.
- Computes `group_idx = (ino - 1) / inodes_per_group`, `bit = (ino - 1) % inodes_per_group`.
- Loads inode bitmap via `BlockGroup::load_inode_bitmap(self, sb)`.
- Asserts the bit is set; clears it via `IdBitmap::free(bit)`.
- Persists bitmap via `BlockDevice::write_bytes(group.inode_bitmap_bid())`.
- Updates counters:
  - `BlockGroup::inc_free_inodes(1)`.
  - `SuperBlock::inc_free_inodes()`.
  - If `is_dir`, `BlockGroup::dec_used_dirs()`.
- Returns `Ok(())`.

Post (free_inode: failure):
- Returns `Err(EIO)` on bitmap I/O failure or invalid inode number.

Invariant:
- Inode numbers are 1-based; group/bit are derived from `(ino - 1)`.
- Bitmap bit 0 means free, bit 1 means allocated (LSB0).

[DIFF]
Linux: Uses Orlov allocator for directories and multiple heuristics in `find_group_orlov`/`find_group_other`.
  → Asterinas: Cyclic scan starting from parent group, based only on `free_inodes_count`.
  Reason: Simplified allocator for initial bring-up; preserves locality without Orlov debt tracking.

Linux: Uses quota, security, ACL, and VFS inode initialization in `ext2_new_inode`.
  → Asterinas: Only returns inode number and updates bitmap/counters.
  Reason: VFS inode construction and security hooks are out of scope for this phase.

Linux: On freeing an already-free inode, logs an error and continues.
  → Asterinas: Asserts (panics) on this inconsistent state.
  Reason: Current skills allow panic on detected metadata inconsistency.
