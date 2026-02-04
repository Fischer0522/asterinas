[PROMPT]
Provide additions to `kernel/src/fs/ext2/block_group.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
read_inode_bitmap          → fs/ext2/ialloc.c:40

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
    pub fn load_inode_bitmap(&self, fs: &Ext2, sb: &SuperBlock) -> Result<IdBitmap>;
}

[SPECIFICATION]
Pre (BlockGroup::load_inode_bitmap):
- `fs.block_device` is readable and `sb` is validated.
- `self` is a valid block group belonging to this filesystem.

Post (BlockGroup::load_inode_bitmap: success):
- Reads one block from the inode bitmap location:
  - `bitmap_bid = self.desc.inode_bitmap`.
  - `offset = bitmap_bid.to_offset()`.
  - Reads exactly `BLOCK_SIZE` bytes into a buffer.
- Builds `IdBitmap` from the buffer using:
  - `capacity = sb.inodes_per_group()`.
  - If `capacity > IdBitmap::capacity()`, returns `Err(Errno::EINVAL)`.
- Returns `IdBitmap` initialized with `len = capacity as u16`.

Post (BlockGroup::load_inode_bitmap: failure):
- Returns `Err(Errno::EIO)` for device I/O errors.
- Returns `Err(Errno::EINVAL)` if capacity exceeds `IdBitmap::capacity()`.

Invariant:
- Bitmap bit numbering uses little-endian (Lsb0) order, matching Linux `ext2_test_bit`.
