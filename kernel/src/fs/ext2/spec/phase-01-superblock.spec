[PROMPT]
Provide `kernel/src/fs/ext2/super_block.rs`. Output Rust code only. No unsafe. Follow Linux
`fs/ext2/super.c` and `fs/ext2/ext2.h` superblock logic, adapted to Asterinas:
- Block size is fixed to 4096 bytes.
- Only ERRORS_CONTINUE is supported.
- Supported feature flags are limited to: COMPAT=EXT_ATTR, RO_COMPAT=SPARSE_SUPER|LARGE_FILE,
  INCOMPAT=FILETYPE. Unknown COMPAT bits must not reject mount.
- Support both EXT2_GOOD_OLD_REV and EXT2_DYNAMIC_REV.

[RELY]
use core::mem::size_of;
pub trait Pod: Copy + 'static {}
pub trait VmIo { fn read_val<T: Pod>(&self, offset: usize) -> Result<T>; }
pub trait BlockDevice: VmIo { fn metadata(&self) -> BlockDeviceMeta; }
pub struct BlockDeviceMeta { pub nr_sectors: usize; }
pub type Result<T> = core::result::Result<T, Error>;
pub struct Error;
pub enum Errno { EINVAL, EOPNOTSUPP }
pub const BLOCK_SIZE: usize = 4096;
pub const SECTOR_SIZE: usize = 512;

[GUARANTEE]
pub fn load_super_block(device: &dyn BlockDevice, read_only: bool) -> Result<SuperBlock>;

[SPECIFICATION]
Pre:
- `device` is readable for at least 1024 bytes at offset 1024.
- `read_only` reflects the intended mount mode.
- feature_compat, feature_incompat, feature_ro_compat in memory are bitflags (bitflags! + struct, see ext2.old).
- The SuperBlock and RawSuperBlock can convert from each other via try_from/from.

Post (success):
- Reads a `RawSuperBlock` from byte offset 1024 (the main superblock location).
- Validates the following (Linux-equivalent checks, plus Asterinas constraints):
  - `raw.magic == EXT2_SUPER_MAGIC (0xEF53)`.
  - `raw.log_block_size == 2` and computed `block_size == 4096`.
  - `raw.log_frag_size == raw.log_block_size`.
  - `raw.creator_os == EXT2_OS_LINUX (0)`.
  - `raw.errors == EXT2_ERRORS_CONTINUE (1)`.
  - `raw.rev_level` is either `EXT2_GOOD_OLD_REV (0)` or `EXT2_DYNAMIC_REV (1)`.
  - Feature checks:
    - If `raw.feature_incompat` has any bits outside FILETYPE (0x0002), return `Err(EINVAL)`.
    - If `read_only == false` and `raw.feature_ro_compat` has any bits outside
      SPARSE_SUPER (0x0001) | LARGE_FILE (0x0002), return `Err(EINVAL)`.
    - Unknown COMPAT bits do not cause failure.
  - Revision handling:
    - If rev is GOOD_OLD_REV: `inode_size = 128`, `first_ino = 11`.
    - If rev is DYNAMIC_REV: `inode_size = raw.inode_size`, `first_ino = raw.first_ino`, and
      `inode_size >= 128`, `inode_size` is power of two, `inode_size <= 4096`.
  - Derived values (computed exactly as Linux does, with fixed block size):
    - `inodes_per_block = 4096 / inode_size`, and `inodes_per_block > 0`.
    - `inodes_per_group = raw.inodes_per_group`, `blocks_per_group = raw.blocks_per_group`.
    - `inodes_per_group != 0` and `blocks_per_group != 0`.
    - `itb_per_group = inodes_per_group / inodes_per_block`.
    - `desc_per_block = 4096 / 32` (ext2_group_desc is 32 bytes).
    - `addr_per_block_bits = ilog2(4096 / 4)`, `desc_per_block_bits = ilog2(desc_per_block)`.
  - Geometry checks (Linux `ext2_fill_super` equivalents):
    - `blocks_per_group <= 4096 * 8`.
    - `blocks_per_group > itb_per_group + 3`.
    - `inodes_per_group >= inodes_per_block`.
    - `inodes_per_group <= 4096 * 8`.
    - `device_blocks = (device.metadata().nr_sectors * 512) / 4096`.
      `device_blocks >= raw.blocks_count`.
    - `groups_count = ((raw.blocks_count - raw.first_data_block - 1) / blocks_per_group) + 1`.
      `groups_count * inodes_per_group == raw.inodes_count`.
- Returns `SuperBlock` with:
  - The on-disk fields preserved (e.g., counts, timestamps, UUID, volume name).
  - `block_size == frag_size == 4096`.
  - Derived fields: `inodes_per_block`, `itb_per_group`, `desc_per_block`,
    `addr_per_block_bits`, `desc_per_block_bits`, `groups_count`.

Post (failure):
- Returns `Err(Errno::EINVAL)` for any validation failure above; no other side effects.

Invariant (SuperBlock):
- `block_size == frag_size == 4096`.
- `inode_size` is power of two and in `[128, 4096]`.
- `inodes_per_block > 0`, `groups_count > 0`.
