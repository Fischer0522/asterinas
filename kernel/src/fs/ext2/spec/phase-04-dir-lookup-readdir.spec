[PROMPT]
Provide additions to `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_find_entry             → fs/ext2/dir.c:342
ext2_readdir                → fs/ext2/dir.c:257

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::dir::{DirEntry, DirEntryIter};
```

```rust
use super::fs::Ext2;
```

```rust
use crate::fs::utils::DirentVisitor;
```

```rust
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
#[derive(Debug)]
pub struct InodeInner {
    desc: Dirty<InodeDesc>,
    is_freed: bool,
    weak_self: Weak<Inode>,
    fs: Weak<Ext2>,
}
```

```rust
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeDesc {
    type_: InodeType,
    size: u64,
    blocks: u32,
    block_ptrs: [u32; 15],
}
```

[GUARANTEE]
impl InodeInner {
    pub(super) fn find_entry(&self, name: &str) -> Result<u32>;
    pub(super) fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize>;
}

[SPECIFICATION]
Pre (find_entry):
- `self.desc.type_` is `InodeType::Dir`.

Post (find_entry: success):
- Iterates directory blocks in ascending order.
- For each block:
  - `block_size = fs.block_size()`.
  - `block_offset = block_idx * block_size`.
  - `limit = min(block_size, self.desc.size - block_offset)`.
  - `max_inumber = sb.total_inodes()`.
  - `max_blocks = self.desc.blocks >> 3` (512-byte sectors to fs-block bound).
  - If `block_idx > max_blocks`, returns `Err(ENOENT)`.
  - Resolves data block by `self.get_block(block_idx)`; `None` => `Err(EIO)`.
  - Parses entries with `DirEntryIter` in `[0, limit)`.
  - Skips entries with `inode == 0`.
  - Exact name match returns `Ok(inode)`.
- No match returns `Err(ENOENT)`.

Post (find_entry: failure):
- Returns `Err(ENOTDIR)` if not directory.
- Returns `Err(EIO)` for invalid entry layout or I/O failures.

Pre (readdir_at):
- `self.desc.type_` is `InodeType::Dir`.

Post (readdir_at: success):
- If directory too small for one minimal entry or `offset` beyond valid scan range, returns `Ok(0)`.
- Computes `start_block = offset / block_size`.
- Scans blocks from `start_block`, each with `limit = min(block_size, size - block_offset)`.
- For each parsed entry:
  - Skip invalid window and `inode == 0` entries.
  - Convert ext2 `file_type` to `InodeType` mapping (0=>Unknown, 1..7=>typed).
  - Calls `visitor.visit(name, inode, type, global_offset)`.
  - If visitor stops, returns bytes advanced so far.
- Returns total bytes advanced from `offset`.

Post (readdir_at: failure):
- Returns `Err(ENOTDIR)` if not directory.
- Returns `Err(EIO)` for parse/layout or I/O failure.

Invariant:
- Parsing follows module 4.1 layout checks (`DirEntryIter` / `ext2_check_folio`-compatible constraints).

[DIFF]
Linux: Uses folio/pagecache and `dir_context` (`ctx->pos`) helpers.
  → Asterinas: Uses direct block reads and `DirentVisitor` byte offsets.
  Reason: Current ext2 path does not use folio abstraction.

Linux: Maintains `i_dir_start_lookup` hint and wrap-around lookup.
  → Asterinas: Linear scan from block 0/start block without wrap-around hint.
  Reason: Lookup hint optimization deferred.

Linux: Methods conceptually belong to inode private state in VFS inode.
  → Asterinas: Methods are placed on `InodeInner` guarded by inode lock.
  Reason: Current split-handle inode abstraction.
