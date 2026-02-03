[PROMPT]
Provide `kernel/src/fs/ext2/block_group.rs`. Output Rust code only. No unsafe. Follow Linux
`fs/ext2/ext2.h`, `fs/ext2/super.c:ext2_check_descriptors`, and
`fs/ext2/balloc.c:ext2_get_group_desc`, adapted to Asterinas:
- Block size is fixed to 4096 bytes.
- META_BG is not supported (already rejected by feature flags).
- Group descriptor table is read from the primary (group 0) descriptor location.
- Use `Vec<RwMutex<BlockGroupDesc>>` as descriptor cache.

[RELY]
use core::cmp::min;
use core::mem::size_of;
pub trait Pod: Copy + 'static {}
pub trait VmIo { fn read_val<T: Pod>(&self, offset: usize) -> Result<T>; }
pub trait BlockDevice: VmIo { fn metadata(&self) -> BlockDeviceMeta; }
pub struct BlockDeviceMeta { pub nr_sectors: usize; }
pub type Result<T> = core::result::Result<T, Error>;
pub struct Error;
pub enum Errno { EINVAL }
pub const BLOCK_SIZE: usize = 4096;

pub struct Bid;
impl Bid { pub fn to_offset(self) -> usize; }

pub struct RwMutex<T>(T);
pub struct RwMutexReadGuard<'a, T>(&'a T);
pub struct RwMutexWriteGuard<'a, T>(&'a mut T);

pub struct SuperBlock;
impl SuperBlock {
    pub fn blocks_per_group(&self) -> u32;
    pub fn inodes_per_group(&self) -> u32;
    pub fn inode_size(&self) -> usize;
    pub fn total_blocks(&self) -> u32;
    pub fn first_data_block(&self) -> u32;
    pub fn block_groups_count(&self) -> u32;
    pub fn group_descriptors_bid(&self, block_group_idx: usize) -> Bid;
}

[GUARANTEE]
pub fn load_group_desc_table(
    device: &dyn BlockDevice,
    sb: &SuperBlock,
) -> Result<BlockGroupDescTable>;

impl BlockGroupDescTable {
    pub fn groups_count(&self) -> u32;
    pub fn desc_per_block(&self) -> u32;
    pub fn group_desc(&self, idx: usize) -> Result<RwMutexReadGuard<BlockGroupDesc>>;
    pub fn group_desc_mut(&self, idx: usize) -> Result<RwMutexWriteGuard<BlockGroupDesc>>;
}

[SPECIFICATION]
Pre:
- `device` is readable for `groups_count * 32` bytes starting at
  `sb.group_descriptors_bid(0).to_offset()`.
- `sb` has been validated by Phase 1 (magic, feature flags, fixed block size).

Post (success):
- Defines `RawGroupDescriptor` as a 32-byte Pod matching Linux `struct ext2_group_desc`.
- Defines `BlockGroupDesc` as the in-memory descriptor containing:
  - `block_bitmap` (u32), `inode_bitmap` (u32), `inode_table` (u32)
  - `free_blocks_count` (u16), `free_inodes_count` (u16), `dirs_count` (u16)
- `BlockGroupDescTable` contains a `Vec<RwMutex<BlockGroupDesc>>` with
  `groups_count = sb.block_groups_count()` entries.
- `desc_per_block = BLOCK_SIZE / size_of::<RawGroupDescriptor>()`.
- Reads the descriptor table from the primary location:
  `table_offset = sb.group_descriptors_bid(0).to_offset()`,
  and for each group `i` (0..groups_count), reads a `RawGroupDescriptor` from
  `table_offset + i * size_of::<RawGroupDescriptor>()` and converts it to
  `BlockGroupDesc` by field-wise copy.
- Validates each descriptor exactly as Linux `ext2_check_descriptors`:
  - `inodes_per_block = BLOCK_SIZE / sb.inode_size()`; `inodes_per_block > 0`.
  - `itb_per_group = sb.inodes_per_group() / inodes_per_block`.
  - `first_block = sb.first_data_block() + i * sb.blocks_per_group()`.
  - `last_block = min(first_block + sb.blocks_per_group() - 1,
                      sb.total_blocks() - 1)`.
  - `block_bitmap` and `inode_bitmap` are within `[first_block, last_block]`.
  - `inode_table` is within `[first_block, last_block]` and
    `inode_table + itb_per_group - 1 <= last_block`.
- `group_desc(i)` returns a read guard for group `i`.
- `group_desc_mut(i)` returns a write guard for group `i`.

Post (failure):
- Returns `Err(Errno::EINVAL)` for any validation failure above or if `idx` is
  out of range in `group_desc`/`group_desc_mut`.
- I/O errors from `read_val` are propagated as `Err`.
- No side effects beyond partial in-memory allocations.

Invariant (BlockGroupDescTable):
- `groups_count > 0` and `descs.len() == groups_count`.
- Each cached descriptor satisfies the block-range constraints above.
