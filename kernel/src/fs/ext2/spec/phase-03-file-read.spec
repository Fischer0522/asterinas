[PROMPT]
Provide additions to `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_file_read_iter        → fs/ext2/file.c:283
ext2_read_folio            → fs/ext2/inode.c:917
ext2_get_block             → fs/ext2/inode.c:783

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::fs::Ext2;
```

```rust
use crate::fs::utils::CachePage;
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

[GUARANTEE]
impl InodeInner {
    /// Reads file data at the given byte offset.
    pub(super) fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize>;

    /// Reads a full block-sized page for page cache.
    pub(super) fn read_page(&self, page_idx: usize, page: &CachePage) -> Result<()>;
}

[SPECIFICATION]
Pre (read_at):
- `offset` is a byte offset within the file.

Post (read_at: success):
- If `self.desc.type_` is `InodeType::Dir`, returns `Err(EISDIR)`.
- If `offset >= self.desc.size`, returns `Ok(0)`.
- Computes `read_len = min(writer.avail(), self.desc.size - offset)`.
- Iterates blocks covering `[offset, offset + read_len)`:
  - `block_size = fs.block_size()`.
  - `block_idx = cur_off / block_size`, `block_off = cur_off % block_size`.
  - `chunk = min(block_size - block_off, remaining)`.
  - Calls `self.get_block(block_idx)`.
    - `None` → writes `chunk` zero bytes.
    - `Some(bid)` → reads full block then copies `[block_off, block_off + chunk)`.
- Returns `Ok(read_len)`.

Post (read_at: failure):
- Returns `Err(EIO)` if block I/O fails or `self.fs` cannot be upgraded.

Pre (read_page):
- `page_idx` is a block index (page size == `BLOCK_SIZE`).

Post (read_page: success):
- If `self.desc.type_` is `InodeType::Dir`, returns `Err(EISDIR)`.
- Computes `page_offset = page_idx * block_size`.
- If `page_offset >= self.desc.size`, zero-fills `page` and returns `Ok(())`.
- Otherwise uses `self.get_block(page_idx as u32)`:
  - `None` → zero-fill page.
  - `Some(bid)` → read full block into page.
- If page crosses EOF, zero-fills tail beyond `self.desc.size`.

Post (read_page: failure):
- Returns `Err(EIO)` for I/O failures or invalid mapping states.

Invariant:
- Sparse regions are read as zeroes.

[DIFF]
Linux: Uses `generic_file_read_iter` and folio readahead plumbing.
  → Asterinas: Uses direct block reads in `InodeInner` and page-cache callback wrappers.
  Reason: Current architecture has not wired ext2 into generic folio helpers.
