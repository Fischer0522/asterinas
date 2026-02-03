[PROMPT]
Provide additions to `kernel/src/fs/ext2/block_group.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
read_block_bitmap           → fs/ext2/balloc.c:129
ext2_valid_block_bitmap     → fs/ext2/balloc.c:71
ext2_group_first_block_no   → fs/ext2/ext2.h:798
ext2_group_last_block_no    → fs/ext2/ext2.h:804

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::super_block::SuperBlock;
```

```rust
use super::block_group::{BlockGroup, GroupDesc};
```

```rust
use crate::fs::utils::IdBitmap;
```

```rust
/// The Ext2 filesystem (core state holder).
#[derive(Debug)]
pub struct Ext2 {
    block_device: Arc<dyn BlockDevice>,
}
```

[GUARANTEE]
impl BlockGroup {
    pub fn load_block_bitmap(&self, fs: &Ext2, sb: &SuperBlock) -> Result<IdBitmap>;
}

[SPECIFICATION]
Pre (BlockGroup::load_block_bitmap):
- `fs.block_device` is readable and `sb` is validated.
- `self` is a valid block group belonging to this filesystem.

Post (BlockGroup::load_block_bitmap: success):
- Reads one block from the block bitmap location:
  - `bitmap_bid = self.desc.block_bitmap`.
  - `offset = bitmap_bid.to_offset()`.
  - Reads exactly `BLOCK_SIZE` bytes into a buffer.
- Computes group bounds:
  - `first_block = sb.group_first_block_no(self.idx())`.
  - `last_block = sb.group_last_block_no(self.idx())`.
-  - `max_bit = last_block - first_block`.
- Uses `itb_per_group` derived during superblock validation (Linux `s_itb_per_group`).
- Validates Linux-equivalent constraints (ext2_valid_block_bitmap):
  - `offset = (block_bitmap - first_block)` and
    if `offset < 0` or `offset > max_bit` or bit `offset` is 0 → Err(EINVAL).
  - `offset = (inode_bitmap - first_block)` and
    if `offset < 0` or `offset > max_bit` or bit `offset` is 0 → Err(EINVAL).
  - `offset = (inode_table - first_block)` and
    if `offset < 0` or `offset > max_bit` or
    `offset + itb_per_group - 1 > max_bit` → Err(EINVAL).
  - `next_zero = find_next_zero_bit(bitmap, offset + itb_per_group, offset)` and
    if `next_zero < offset + itb_per_group` → Err(EINVAL).
- Builds `IdBitmap` from the buffer using:
  - `capacity = (last_block - first_block + 1)` (i.e., group block count).
  - If `capacity > IdBitmap::capacity()`, returns `Err(Errno::EINVAL)`.
- Returns `IdBitmap` initialized with `len = capacity as u16`.

Post (BlockGroup::load_block_bitmap: failure):
- Returns `Err(Errno::EIO)` for device I/O errors.
- Returns `Err(Errno::EINVAL)` if any validation fails.

Invariant:
- Bitmap bit numbering uses little-endian (Lsb0) order, matching Linux `ext2_test_bit`.
