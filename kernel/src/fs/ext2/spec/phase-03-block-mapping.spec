[PROMPT]
Provide additions to `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_block_to_path          → fs/ext2/inode.c:163
ext2_get_block              → fs/ext2/inode.c:783

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::fs::Ext2;
```

```rust
use crate::fs::utils::InodeType;
```

```rust
/// The Ext2 inode (in-memory).
#[derive(Debug)]
pub struct Inode {
    ino: u32,
    type_: InodeType,
    block_ptrs: [u32; 15],
    fs: Weak<Ext2>,
}
```

```rust
/// Block path offsets for direct/indirect traversal.
#[derive(Clone, Copy, Debug)]
pub(super) struct BlockPath {
    pub depth: usize,
    pub offsets: [u32; 4],
    pub boundary: u32,
}
```

[GUARANTEE]
impl Inode {
    pub(super) fn block_to_path(&self, iblock: u32) -> Result<BlockPath>;
    pub(super) fn get_block(&self, iblock: u32) -> Result<Option<Bid>>;
}

[SPECIFICATION]
Pre (block_to_path):
- `iblock` is a logical block number (0-based).

Post (block_to_path: success):
- Computes `ptrs = sb.block_size() / size_of::<u32>()`.
- Computes `ptrs_bits = log2(ptrs)` and constants:
  - `direct_blocks = 12` (EXT2_NDIR_BLOCKS).
  - `indirect_blocks = ptrs`.
  - `double_blocks = 1 << (ptrs_bits * 2)`.
- Follows Linux `ext2_block_to_path` decision tree:
  - If `iblock < direct_blocks`:
    - `offsets[0] = iblock`, `depth = 1`, `boundary = direct_blocks - 1 - iblock`.
  - Else if `(iblock - direct_blocks) < indirect_blocks`:
    - `offsets[0] = 12` (EXT2_IND_BLOCK), `offsets[1] = iblock - direct_blocks`,
      `depth = 2`, `boundary = ptrs - 1 - (iblock - direct_blocks) % ptrs`.
  - Else if `(iblock - direct_blocks - indirect_blocks) < double_blocks`:
    - `offsets[0] = 13` (EXT2_DIND_BLOCK),
      `offsets[1] = (iblock - direct_blocks - indirect_blocks) >> ptrs_bits`,
      `offsets[2] = (iblock - direct_blocks - indirect_blocks) & (ptrs - 1)`,
      `depth = 3`, `boundary = ptrs - 1 - (iblock - direct_blocks - indirect_blocks) % ptrs`.
  - Else if remaining value fits in triple indirect:
    - `offsets[0] = 14` (EXT2_TIND_BLOCK),
      `offsets[1] = (rem >> (ptrs_bits * 2))`,
      `offsets[2] = (rem >> ptrs_bits) & (ptrs - 1)`,
      `offsets[3] = rem & (ptrs - 1)`,
      `depth = 4`, `boundary = ptrs - 1 - (rem % ptrs)`.
- Returns `BlockPath { depth, offsets, boundary }`.

Post (block_to_path: failure):
- Returns `Err(Errno::EINVAL)` if `iblock` exceeds triple-indirect capacity.

Pre (get_block):
- `iblock` is a logical block number (0-based).
- Inode `block_ptrs` reflect on-disk `i_block` (little-endian values).

Post (get_block: success):
- Calls `block_to_path(iblock)` to obtain traversal offsets.
- Traverses the block pointer tree without allocation (Linux `ext2_get_block` with `create = 0`):
  - If `depth == 1`, uses `block_ptrs[offsets[0]]` as data block.
  - If `depth > 1`, resolves indirect blocks iteratively:
    - `block_ptrs[offsets[0]]` is the first-level indirect block pointer.
    - For each level, if pointer is 0 → returns `Ok(None)`.
    - Otherwise read the indirect block via `fs.block_device().read_bytes()` into `BLOCK_SIZE` buffer.
    - Interpret the block as `u32` array; pick entry `offsets[level]`.
- On success, returns `Ok(Some(Bid::new(block_id as u64)))`.
- If any pointer along the path is 0, returns `Ok(None)` (hole/unmapped).

Post (get_block: failure):
- Returns `Err(Errno::EIO)` for device I/O failures or if `fs` cannot be upgraded.
- Returns `Err(Errno::EINVAL)` if `block_to_path` fails.

Invariant:
- Pointer tree traversal and offsets follow Linux `ext2_block_to_path` + read-only path of `ext2_get_block`.
- No allocation is performed in this module.
