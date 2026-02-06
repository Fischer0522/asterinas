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
/// The Ext2 inode public handle.
#[derive(Debug)]
pub struct Inode {
    ino: u32,
    type_: InodeType,
    inner: RwMutex<InodeInner>,
    block_group_idx: usize,
    fs: Weak<Ext2>,
}
```

```rust
/// Mutable inode state.
#[derive(Debug)]
pub struct InodeInner {
    desc: Dirty<InodeDesc>,
    is_freed: bool,
    weak_self: Weak<Inode>,
    fs: Weak<Ext2>,
}
```

```rust
/// In-memory inode descriptor.
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeDesc {
    type_: InodeType,
    size: u64,
    blocks: u32,
    block_ptrs: [u32; 15],
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
impl InodeInner {
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
    - `offsets[0] = 12`, `offsets[1] = iblock - direct_blocks`, `depth = 2`.
  - Else if `(iblock - direct_blocks - indirect_blocks) < double_blocks`:
    - `offsets[0] = 13`, `offsets[1] = rem >> ptrs_bits`, `offsets[2] = rem & (ptrs - 1)`, `depth = 3`.
  - Else if remaining value fits in triple-indirect:
    - `offsets[0] = 14`, `offsets[1..=3]` from triple-indirect decomposition, `depth = 4`.
- Returns `BlockPath { depth, offsets, boundary }`.

Post (block_to_path: failure):
- Returns `Err(Errno::EINVAL)` if `iblock` exceeds triple-indirect capacity.

Pre (get_block):
- `iblock` is a logical block number (0-based).
- `self.desc.block_ptrs` reflects on-disk `i_block` values.

Post (get_block: success):
- Calls `block_to_path(iblock)` to obtain traversal offsets.
- Traverses the block pointer tree without allocation (`create = 0` semantics):
  - First pointer from `self.desc.block_ptrs[offsets[0]]`.
  - For each indirect level, reads indirect block and selects `offsets[level]`.
  - If any pointer on path is 0, returns `Ok(None)`.
- Returns `Ok(Some(Bid::new(block_id as u64)))` when fully resolved.

Post (get_block: failure):
- Returns `Err(Errno::EIO)` for device I/O failures or if `fs` cannot be upgraded.
- Returns `Err(Errno::EINVAL)` if `block_to_path` fails.

Invariant:
- Pointer tree traversal and offsets follow Linux `ext2_block_to_path` plus read-only path of `ext2_get_block`.
- No allocation is performed in this module.

[DIFF]
Linux: Block mapping helpers are attached to `struct inode` internals.
  → Asterinas: Mapping helpers are placed in `InodeInner` and operate on `desc.block_ptrs`.
  Reason: Current abstraction separates public inode handle from mutable inode state.
