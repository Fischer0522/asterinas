[PROMPT]
Provide additions to `kernel/src/fs/ext2/fs.rs` and `kernel/src/fs/ext2/block_group.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
struct ext2_group_desc   → /root/linux/fs/ext2/ext2.h:191
ext2_sync_super          → /root/linux/fs/ext2/super.c:1283
ext2_write_super         → /root/linux/fs/ext2/super.c:1359
ext2_group_sparse        → /root/linux/fs/ext2/balloc.c:1498

[RELY]
```rust
use core::mem::size_of;
```

```rust
use super::prelude::*;
```

```rust
use super::block_group::{BlockGroup, RawGroupDesc};
```

```rust
use super::super_block::{RawSuperBlock, SuperBlock, SUPER_BLOCK_OFFSET};
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
impl From<GroupDesc> for RawGroupDesc;

impl BlockGroup {
    /// Writes this group descriptor into the in-memory descriptor table.
    ///
    /// # Arguments
    /// * `group_descs` - Group descriptor table segment.
    pub(super) fn sync_metadata(&self, group_descs: &USegment) -> Result<()>;
}

impl Ext2 {
    /// Writes back superblock and group descriptor table if dirty.
    ///
    /// # Returns
    /// * `Ok(())` - Metadata flushed (or already clean).
    /// * `Err(EIO)` - I/O failure while writing metadata.
    pub fn sync_metadata(&self) -> Result<()>;
}

[SPECIFICATION]
Pre (From<GroupDesc> for RawGroupDesc):
- `desc` contains valid on-disk block IDs and counters.

Post (From<GroupDesc> for RawGroupDesc):
- Converts `Bid` fields to raw `u32` block numbers.
- Copies `free_blocks_count`, `free_inodes_count`, and `used_dirs_count`.
- Sets padding/reserved fields to zero.

Pre (BlockGroup::sync_metadata):
- `group_descs` covers at least `groups_count * size_of::<RawGroupDesc>()` bytes.

Post (BlockGroup::sync_metadata):
- If descriptor is clean, returns `Ok(())` without modifying `group_descs`.
- If descriptor is dirty:
  - Writes `RawGroupDesc::from(*desc)` to offset `idx * size_of::<RawGroupDesc>()` in `group_descs`.
  - Clears the dirty flag on the descriptor.

Pre (sync_metadata):
- `self.super_block` is validated and `self.block_groups` is loaded.
- `group_descriptors_segment` contains the descriptor table for all groups.

Post (sync_metadata: success):
- If neither superblock nor any group descriptor is dirty, returns `Ok(())` and performs no I/O.
- Otherwise:
  1. For each block group, call `BlockGroup::sync_metadata(&group_descriptors_segment)`.
  2. Serialize the descriptor table by reading `desc_bytes = groups_count * size_of::<RawGroupDesc>()`
     bytes from `group_descriptors_segment` into a buffer.
  3. Write the primary descriptor table to disk at
     `sb.group_descriptors_bid(0).to_offset()` using `BlockDevice::write_bytes`.
  4. Serialize the superblock via `RawSuperBlock::from(&*sb_guard)` and write it to
     `SUPER_BLOCK_OFFSET` using `BlockDevice::write_bytes`.
     - Before serialization, update superblock write time:
       `sb.wtime = UnixTime::now()` (Linux `ext2_sync_super` `s_wtime` update).
  5. For each backup group `i` where `sb.is_backup_group(i)` is true:
     - Clone the raw superblock and set `block_group_idx = i as u16`.
     - Write the backup superblock to `sb.bid(i).to_offset()`.
     - Write the descriptor table to `sb.group_descriptors_bid(i).to_offset()`.
  6. Clear the superblock dirty flag.
- Returns `Ok(())` after all writes succeed.

Post (sync_metadata: failure):
- Returns `Err(EIO)` if any descriptor table read or disk write fails.
- On error, dirty flags may remain set.

Invariant:
- Descriptor table layout is `groups_count * size_of::<RawGroupDesc>()` bytes.
- Backup superblock placement follows `SuperBlock::is_backup_group` policy.

[DIFF]
Linux: `ext2_sync_super` recomputes free counts by scanning bitmaps and uses buffer_head dirtying.
  → Asterinas: Writes existing in-memory counters and uses direct `BlockDevice::write_bytes`.
  Reason: No buffer_head layer or bitmap recounting in this phase.

Linux: Superblock sync is driven by VFS sync/umount hooks and may skip backup copies under policy.
  → Asterinas: Explicit `sync_metadata` writes primary plus all backup groups per `is_backup_group`.
  Reason: Simpler flush path without VFS callback integration.

Linux: Group descriptor updates are mediated by buffer_head + spinlock ordering.
  → Asterinas: Writes `RawGroupDesc` into `USegment` directly when dirty.
  Reason: Descriptor table is kept in memory; no buffer_head layer yet.
