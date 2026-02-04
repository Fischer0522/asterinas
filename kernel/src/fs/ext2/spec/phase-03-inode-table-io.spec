[PROMPT]
Provide additions to `kernel/src/fs/ext2/fs.rs` and `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_get_inode             → fs/ext2/inode.c:1314

[RELY]
```rust
use core::mem::size_of;
```

```rust
use super::prelude::*;
```

```rust
use super::block_group::BlockGroup;
```

```rust
use super::inode::RawInode;
```

```rust
use super::super_block::SuperBlock;
```

```rust
use super::utils::Dirty;
```

```rust
use crate::fs::utils::FsEventSubscriberStats;
```

```rust
/// The Ext2 filesystem (core state holder).
#[derive(Debug)]
pub struct Ext2 {
    block_device: Arc<dyn BlockDevice>,
    super_block: RwMutex<Dirty<SuperBlock>>,
    block_groups: Vec<BlockGroup>,
    inodes_per_group: u32,
    blocks_per_group: u32,
    inode_size: usize,
    block_size: usize,
    group_descriptors_segment: USegment,
    fs_event_subscriber_stats: FsEventSubscriberStats,
    self_ref: Weak<Ext2>,
}
```

```rust
/// The root inode number (Linux EXT2_ROOT_INO).
pub const ROOT_INO: u32 = 2;
```

```rust
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeDesc {
    pub raw: RawInode,
}
```

[GUARANTEE]
impl Ext2 {
    pub(super) fn inode_table_block(&self, group_idx: usize, table_block_index: u32) -> Result<Bid>;
    pub(super) fn read_inode_desc(&self, ino: u32) -> Result<InodeDesc>;
}

[SPECIFICATION]
Pre (inode_table_block):
- `group_idx < self.block_groups.len()`.

Post (inode_table_block: success):
- Let `base = self.block_groups[group_idx].inode_table_bid()`.
- Returns `Ok(base + table_block_index as u64)`.

Post (inode_table_block: failure):
- Returns `Err(Errno::EIO)` if `group_idx` is out of range.

Pre (read_inode_desc):
- `self.super_block` has been validated by `load_super_block`.
- `ino` is a 1-based inode number.

Post (read_inode_desc: success):
- Reads `sb` from `self.super_block`.
- Validates inode number (Linux `ext2_get_inode`):
  - If `(ino != ROOT_INO && ino < sb.first_ino())` → `Err(Errno::EINVAL)`.
  - If `ino > sb.total_inodes()` → `Err(Errno::EINVAL)`.
- Computes inode table position:
  - `group_idx = (ino - 1) / sb.inodes_per_group()`.
  - `index_in_group = (ino - 1) % sb.inodes_per_group()`.
  - `offset_bytes = index_in_group * sb.inode_size()`.
  - `block_index = offset_bytes / sb.block_size()`.
  - `offset_in_block = offset_bytes % sb.block_size()`.
- Computes inode table block:
  - `block_bid = self.inode_table_block(group_idx as usize, block_index as u32)`.
- Reads one full block from `block_bid` into a `BLOCK_SIZE` buffer.
- Extracts a `RawInode` at `offset_in_block` and returns `Ok(InodeDesc { raw })`.

Post (read_inode_desc: failure):
- Returns `Err(Errno::EIO)` if `inode_table_block` fails or the block read fails.
- Returns `Err(Errno::EINVAL)` if inode number validation fails.

Invariant:
- Block address and offsets follow Linux `ext2_get_inode` formulas using
  `inode_size` and `block_size` (no alternative indexing).
