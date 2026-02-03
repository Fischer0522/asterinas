[PROMPT]
Provide additions to `kernel/src/fs/ext2/fs.rs` and `kernel/src/fs/ext2/super_block.rs`. Output Rust code only. No unsafe.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
struct ext2_group_desc        → fs/ext2/ext2.h:191
ext2_check_descriptors        → fs/ext2/super.c:695
ext2_group_first_block_no     → fs/ext2/ext2.h:798
ext2_group_last_block_no      → fs/ext2/ext2.h:804

[RELY]
```rust
use core::mem::size_of;
```

```rust
use super::prelude::*;
```

```rust
use super::super_block::SuperBlock;
```

```rust
use super::block_group::RawGroupDesc;
```

```rust
use crate::fs::utils::FsEventSubscriberStats;
```

```rust
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

[GUARANTEE]
impl Ext2 {
    pub(super) fn load_group_desc_table(
        &self,
        sb: &SuperBlock,
    ) -> Result<USegment>;

    pub(super) fn check_group_desc_table(
        &self,
        sb: &SuperBlock,
        group_descs: &USegment,
    ) -> Result<()>;
}

impl SuperBlock {
    pub(super) fn group_first_block_no(&self, group_idx: usize) -> u32;
    pub(super) fn group_last_block_no(&self, group_idx: usize) -> u32;
}

[SPECIFICATION]
Pre (load_group_desc_table):
- `self.block_device` is readable and supports block I/O.
- `sb` has been validated by `load_super_block`.

Post (load_group_desc_table: success):
- Computes descriptor table size:
  - `groups_count = sb.block_groups_count()`.
  - `desc_bytes = groups_count * size_of::<RawGroupDesc>()`.
  - `npages = desc_bytes.div_ceil(BLOCK_SIZE)`.
- Allocates a `USegment` with `npages` pages (non-zeroed).
- Reads `npages` contiguous blocks from `self.block_device` starting at
  `sb.group_descriptors_bid(0)` into the segment via `BioSegment`.
- If I/O status is not `BioStatus::Complete`, returns `Err(Errno::EIO)`.
- Calls `self.check_group_desc_table(sb, &segment)` and returns error if validation fails.
- Returns the populated `USegment` on success.

Post (load_group_desc_table: failure):
- Returns `Err(Errno::EIO)` for I/O failures.
- Returns `Err(Errno::EINVAL)` for validation failures.
- No other side effects.

Pre (check_group_desc_table):
- `group_descs` contains at least `groups_count * size_of::<RawGroupDesc>()` bytes.

Post (check_group_desc_table: success):
- For each `group_idx` in `[0, groups_count)`:
  - `first_block = sb.group_first_block_no(group_idx)`.
  - `last_block = sb.group_last_block_no(group_idx)`.
  - `inodes_per_block = BLOCK_SIZE / sb.inode_size()`.
  - `itb_per_group = sb.inodes_per_group() / inodes_per_block`.
  - Read `RawGroupDesc` at offset `group_idx * size_of::<RawGroupDesc>()`.
  - Validate (Linux `ext2_check_descriptors` equivalent):
    - `bg_block_bitmap` in `[first_block, last_block]`.
    - `bg_inode_bitmap` in `[first_block, last_block]`.
    - `bg_inode_table` in `[first_block, last_block]` and
      `bg_inode_table + itb_per_group - 1 <= last_block`.
- Returns `Ok(())` if all groups pass.

Post (check_group_desc_table: failure):
- Returns `Err(Errno::EINVAL)` when any descriptor is out of range.

Pre (group_first_block_no/group_last_block_no):
- `group_idx < sb.block_groups_count()`.

Post (group_first_block_no):
- Returns `group_idx * sb.blocks_per_group() + sb.first_data_block()`.

Post (group_last_block_no):
- If `group_idx == sb.block_groups_count() - 1`:
  - Returns `sb.total_blocks() - 1`.
- Else:
  - Returns `group_first_block_no(group_idx) + sb.blocks_per_group() - 1`.

Invariant:
- Validation uses Linux block range formulas from `ext2_group_first_block_no`
  and `ext2_group_last_block_no`.
