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

```rust
impl InodeDesc {
    pub fn type_(&self) -> InodeType;
}
```

[GUARANTEE]
impl BlockGroup {
    /// Decreases the free-inode counter for this group.
    pub(super) fn dec_free_inodes(&self, count: u16);

    /// Increases the free-inode counter for this group.
    pub(super) fn inc_free_inodes(&self, count: u16);

    /// Increases the used-dirs counter for this group.
    pub(super) fn inc_used_dirs(&self);

    /// Decreases the used-dirs counter for this group.
    pub(super) fn dec_used_dirs(&self);
}

impl Ext2 {
    /// Allocates a new inode number.
    pub(super) fn alloc_inode(&self, parent_ino: u32, inode_type: InodeType) -> Result<u32>;

    /// Frees an inode by number.
    pub(super) fn free_inode(&self, ino: u32) -> Result<()>;
}

[SPECIFICATION]
Pre (BlockGroup::dec_free_inodes / inc_free_inodes):
- `count` fits within `u16` and does not underflow/overflow the group counter.

Post (BlockGroup::dec_free_inodes):
- Decreases `free_inodes_count` by `count` and marks descriptor dirty.

Post (BlockGroup::inc_free_inodes):
- Increases `free_inodes_count` by `count` and marks descriptor dirty.

Post (BlockGroup::inc_used_dirs / dec_used_dirs):
- Adjusts `used_dirs_count` by +/-1 and marks descriptor dirty.

Pre (alloc_inode):
- `parent_ino` is within `[ROOT_INO, sb.total_inodes()]`.
- `self.super_block` is validated and `self.block_groups` is loaded.
- If `sb.free_inodes_count() == 0`, returns `Err(ENOSPC)` without bitmap I/O.

Post (alloc_inode: success):
- Returns `Ok(ino)` where:
  - `ino` is within `[sb.first_ino(), sb.total_inodes()]`.
  - inode bitmap bit for `ino` is set in owning group.
- Allocation order (simplified from Linux):
  1. `parent_group = (parent_ino - 1) / inodes_per_group`.
  2. Cyclic group scan from `parent_group`.
  3. Skip groups with zero free inode counter.
  4. Candidate group path:
     - `load_inode_bitmap`.
     - allocate one bit with `IdBitmap::alloc()`.
     - compute `ino = group_idx * inodes_per_group + inode_idx + 1`.
     - validate inode range.
     - persist bitmap.
     - update counters:
       - `group.dec_free_inodes(1)`
       - `sb.dec_free_inodes()`
       - if directory: `group.inc_used_dirs()`.
- Returns `Err(EIO)` for bitmap I/O failures.

Post (alloc_inode: failure):
- `Err(ENOSPC)` if no free inode is found.
- `Err(EIO)` on I/O failure.
- On failure, counters remain unchanged.

Pre (free_inode):
- `ino` is within `[sb.first_ino(), sb.total_inodes()]`.

Post (free_inode: success):
- Determines `is_dir` by `read_inode_desc(ino)?.type_().is_directory()`.
- Computes `group_idx = (ino - 1) / inodes_per_group`, `bit = (ino - 1) % inodes_per_group`.
- Loads inode bitmap.
- If bit is allocated, clears it and marks local flag for counter updates.
- If bit is already clear, logs metadata inconsistency and skips counter updates.
- Persists bitmap.
- When bit transitioned allocated→free, updates:
  - `group.inc_free_inodes(1)`
  - `sb.inc_free_inodes()`
  - if directory: `group.dec_used_dirs()`.

Post (free_inode: failure):
- Returns `Err(EIO)` on bitmap I/O failure or invalid inode number.

Invariant:
- Inode numbering is 1-based.
- Group and bit indices are derived from `(ino - 1)`.
- Bitmap bit semantics: 0 free, 1 allocated.

[DIFF]
Linux: Uses Orlov allocator (`find_group_orlov` / `find_group_other`).
  → Asterinas: Cyclic scan starting from parent group and free counter heuristics.
  Reason: Simpler allocator for current bring-up phase.

Linux: Inode free path consults inode/VFS state and broad side effects.
  → Asterinas: Uses decoded `InodeDesc` (`type_`) for dir counter decision.
  Reason: Current abstraction centralizes parsed inode metadata in `InodeDesc`.

Linux: Already-free inode free logs error and continues.
  → Asterinas: Same resilience policy, no counter mutation on already-free bit.
