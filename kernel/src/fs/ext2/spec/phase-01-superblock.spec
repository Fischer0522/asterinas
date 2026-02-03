[PROMPT]
Provide additions to `kernel/src/fs/ext2/super_block.rs`. Output Rust code only. No unsafe.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
struct ext2_super_block     → fs/ext2/ext2.h:411
ext2_fill_super             → fs/ext2/super.c:877
ext2_setup_super            → fs/ext2/super.c:645

[RELY]
```rust
use core::mem::size_of;
```

```rust
use super::prelude::*;
```

```rust
use super::super_block::{MAGIC_NUM, SUPER_BLOCK_OFFSET, RawSuperBlock, SuperBlock};
```

```rust
pub trait VmIo { fn read_val<T: Pod>(&self, offset: usize) -> Result<T>; }
```

```rust
pub trait BlockDevice: VmIo { fn metadata(&self) -> BlockDeviceMeta; }
```

```rust
pub struct BlockDeviceMeta { pub nr_sectors: usize; }
```

[GUARANTEE]
impl TryFrom<RawSuperBlock> for SuperBlock {
    fn try_from(sb: RawSuperBlock) -> Result<Self>;
}

impl SuperBlock {
    pub fn load_super_block(&self, device: &dyn BlockDevice, read_only: bool) -> Result<SuperBlock>;
}

[SPECIFICATION]
Pre (try_from):
- `sb` is a raw superblock read from disk at offset 1024.

Post (try_from: success):
- Validates Linux-equivalent checks (from ext2_fill_super) with Asterinas constraints:
  - `sb.magic == EXT2_SUPER_MAGIC (0xEF53)`.
  - `sb.log_block_size == 2` and computed `block_size == 4096`.
  - `sb.log_frag_size == sb.log_block_size`.
  - `sb.creator_os == EXT2_OS_LINUX (0)`.
  - `sb.errors == EXT2_ERRORS_CONTINUE (1)` (only supported behavior).
  - `sb.rev_level` is `EXT2_GOOD_OLD_REV (0)` or `EXT2_DYNAMIC_REV (1)`.
  - Revision handling:
    - If GOOD_OLD_REV: `inode_size = 128`, `first_ino = 11`.
    - If DYNAMIC_REV: `inode_size = sb.inode_size`, `first_ino = sb.first_ino`, and
      `inode_size >= 128`, `inode_size` is power of two, `inode_size <= 4096`.
  - Group geometry checks:
    - `inodes_per_group != 0`, `blocks_per_group != 0`.
    - `inodes_per_block = 4096 / inode_size`, `inodes_per_block > 0`.
    - `inodes_per_group >= inodes_per_block`.
    - `inodes_per_group <= 4096 * 8`.
    - `blocks_per_group <= 4096 * 8`.
    - `itb_per_group = inodes_per_group / inodes_per_block`.
    - `blocks_per_group > itb_per_group + 3`.
    - `blocks_count > first_data_block + 1`.
    - `groups_count = ((blocks_count - first_data_block - 1) / blocks_per_group) + 1` and
      `groups_count * inodes_per_group == inodes_count`.
  - Feature checks (Asterinas supported bits):
    - COMPAT: unknown bits are ignored.
    - INCOMPAT: only FILETYPE (0x0002) allowed; others → Err(EINVAL).
    - RO_COMPAT: no rejection here; `feature_ro_compat` stores only supported bits
      (SPARSE_SUPER | LARGE_FILE), unknown bits are handled in `load_super_block`.
- Returns a `SuperBlock` populated with all on-disk fields and computed values:
  - `block_size == frag_size == 4096`.
  - `feature_*` bitsets derived from raw fields.

Post (try_from: failure):
- Returns `Err(Errno::EINVAL)` for any validation failure above.
- No side effects.

Pre (load_super_block):
- `device` is readable at least 1024 bytes from offset `SUPER_BLOCK_OFFSET`.
- `read_only` reflects the intended mount mode.

Post (load_super_block: success):
- Reads `RawSuperBlock` from byte offset `SUPER_BLOCK_OFFSET`.
- Calls `SuperBlock::try_from` for structural validation.
- Performs Linux-equivalent device size check:
  - `device_blocks = (device.metadata().nr_sectors * 512) / 4096`.
  - `device_blocks >= raw.blocks_count`.
- Applies RO_COMPAT gating:
  - If `read_only == false` and raw has unsupported RO_COMPAT bits, return Err(EINVAL).
  - If `read_only == true`, allow unsupported RO_COMPAT bits (mount read-only).
- Returns the validated `SuperBlock`.

Post (load_super_block: failure):
- Returns `Err(Errno::EINVAL)` for validation or size check failures.
- No side effects.

Invariant (SuperBlock):
- `block_size == frag_size == 4096`.
- `inode_size` is power-of-two within `[128, 4096]`.
- `inodes_per_group > 0`, `blocks_per_group > 0`.
