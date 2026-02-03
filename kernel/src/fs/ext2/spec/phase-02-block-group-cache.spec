[PROMPT]
Provide additions to `kernel/src/fs/ext2/block_group.rs` and `kernel/src/fs/ext2/fs.rs`.
Output Rust code only. No unsafe. All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
struct ext2_group_desc    → fs/ext2/ext2.h:191
ext2_get_group_desc       → fs/ext2/balloc.c:24
ext2_fill_super           → fs/ext2/super.c:877

[RELY]
```rust
use core::mem::size_of;
```

```rust
use super::prelude::*;
```

```rust
use super::block_group::RawGroupDesc;
```

```rust
use super::super_block::SuperBlock;
```

```rust
use crate::fs::utils::IdBitmap;
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
impl From<RawGroupDesc> for GroupDesc {
    fn from(raw: RawGroupDesc) -> Self;
}

impl BlockGroup {
    pub fn load(group_descs: &USegment, idx: usize) -> Result<Self>;
    pub fn idx(&self) -> usize;
    pub fn block_bitmap_bid(&self) -> Bid;
    pub fn inode_bitmap_bid(&self) -> Bid;
    pub fn inode_table_bid(&self) -> Bid;
    pub fn free_blocks_count(&self) -> u16;
    pub fn free_inodes_count(&self) -> u16;
    pub fn used_dirs_count(&self) -> u16;
}

impl Ext2 {
    pub(super) fn load_block_groups(
        &self,
        sb: &SuperBlock,
        group_descs: &USegment,
    ) -> Result<Vec<BlockGroup>>;
}

[SPECIFICATION]
Pre (From<RawGroupDesc> for GroupDesc):
- `raw` is a `RawGroupDesc` read from the on-disk descriptor table.

Post (From<RawGroupDesc> for GroupDesc):
- Returns a `GroupDesc` with fields copied from `raw`.
- `block_bitmap`, `inode_bitmap`, `inode_table` are converted to `Bid` via `Bid::new`.

Pre (BlockGroup::load):
- `group_descs` contains the descriptor table loaded by Ext2.
- `idx` is a valid block group index.

Post (BlockGroup::load: success):
- Reads `RawGroupDesc` from offset `idx * size_of::<RawGroupDesc>()` via `group_descs.read_val`.
- Converts it using `From<RawGroupDesc> for GroupDesc`.
- Returns `BlockGroup { idx, desc: RwMutex::new(Dirty::new(desc)) }`.
- No device I/O is performed.

Post (BlockGroup::load: failure):
- Returns `Err(Errno::EIO)` if the descriptor cannot be read from memory.

Post (idx / *_bid / *_count):
- `idx()` returns the group index.
- `block_bitmap_bid()`, `inode_bitmap_bid()`, `inode_table_bid()` return the `Bid` values from `desc`.
- `free_blocks_count()`, `free_inodes_count()`, `used_dirs_count()` return corresponding counters.
- These accessors do not mutate state.

Pre (Ext2::load_block_groups):
- `sb` is validated and `group_descs` contains the descriptor table.

Post (Ext2::load_block_groups: success):
- Iterates `group_idx` from `0..sb.block_groups_count()`.
- For each, calls `BlockGroup::load(group_descs, group_idx)` and collects into a `Vec<BlockGroup>`.
- Returns the vector with length `sb.block_groups_count()`.
- No device I/O is performed here (all reads are from `group_descs`).

Post (Ext2::load_block_groups: failure):
- Returns `Err(Errno::EIO)` if any `BlockGroup::load` fails.

Invariant:
- `BlockGroup` caches only descriptor data and counters at this stage (no bitmap data).
- Group descriptor indexing matches Linux `ext2_get_group_desc` logic.
