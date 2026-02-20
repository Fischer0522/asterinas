[PROMPT]
Provide additions to `kernel/src/fs/ext2/inode.rs` and `kernel/src/fs/ext2/impl_for_vfs/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_inode_is_fast_symlink → fs/ext2/inode.c:48
ext2_symlink (write path)  → fs/ext2/namei.c:157
ext2_iget (read path)      → fs/ext2/inode.c:1482-1492
page_symlink               → fs/namei.c:6273
page_get_link (slow read)  → fs/namei.c (via ext2_aops → page cache)
simple_get_link (fast read) → fs/namei.c (direct i_link pointer)

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::fs::Ext2;
```

```rust
use crate::fs::utils::SymbolicLink;
```

```rust
#[derive(Debug)]
pub struct Inode {
    ino: u32,
    type_: InodeType,
    inner: RwMutex<InodeInner>,
    block_group_idx: usize,
    fs: Weak<Ext2>,
    extension: Extension,
}
```

```rust
pub struct InodeInner {
    desc: Dirty<InodeDesc>,
    is_freed: bool,
    weak_self: Weak<Inode>,
    fs: Weak<Ext2>,
    page_cache: PageCache,
}
```

```rust
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeDesc {
    type_: InodeType,
    perm: FilePerm,
    uid: u32,
    gid: u32,
    size: u64,
    atime: Duration,
    ctime: Duration,
    mtime: Duration,
    dtime: Duration,
    links_count: u16,
    blocks: u32,
    flags: FileFlags,
    file_acl: u32,
    generation: u32,
    block_ptrs: [u32; 15],
}
```

```rust
impl Ext2 {
    pub fn block_device(&self) -> &dyn BlockDevice;
    pub fn block_size(&self) -> usize;
}
```

```rust
impl Inode {
    pub(super) fn fs_arc(&self) -> Result<Arc<Ext2>>;
    pub(super) fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize>;
    pub(super) fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize>;
    pub(super) fn resize(&self, new_size: usize) -> Result<()>;
}
```

[GUARANTEE]
```rust
/// Maximum bytes storable in the inode block pointer area (fast symlink).
/// sizeof(u32) * 15 = 60 bytes.
/// Linux: sizeof(EXT2_I(inode)->i_data) = 60.
const MAX_FAST_SYMLINK_LEN: usize = core::mem::size_of::<u32>() * 15; // 60
```

```rust
impl Inode {
    /// Reads the symlink target path.
    ///
    /// Linux: ext2_iget (inode.c:1482-1492) determines fast vs slow at iget time.
    ///        Fast: simple_get_link returns i_link = (char *)ei->i_data.
    ///        Slow: page_get_link reads target from page cache (data blocks).
    pub(super) fn read_link(&self) -> Result<String>;

    /// Writes the symlink target path into a newly created symlink inode.
    ///
    /// Linux: ext2_symlink (namei.c:157) — called once at symlink creation.
    ///        Fast: memcpy into ei->i_data, set i_size = len-1.
    ///        Slow: page_symlink writes target into data block via page cache.
    pub(super) fn write_link(&self, target: &str) -> Result<()>;
}
```

[SPECIFICATION]

## 8.1.0  is_fast_symlink — Fast Symlink Detection

Context (InodeDesc method):
- Determines whether a symlink stores its target inline in `block_ptrs`
  (fast symlink) or in data blocks (slow symlink).

Linux reference: `ext2_inode_is_fast_symlink` (inode.c:48-55):
```c
static inline int ext2_inode_is_fast_symlink(struct inode *inode)
{
    int ea_blocks = EXT2_I(inode)->i_file_acl ?
        (inode->i_sb->s_blocksize >> 9) : 0;
    return (S_ISLNK(inode->i_mode) &&
        inode->i_blocks - ea_blocks == 0);
}
```

Algorithm:
1. Compute `ea_blocks`: if `self.file_acl != 0`, then `block_size / SECTOR_SIZE`,
   else `0`.
   (Linux: `EXT2_I(inode)->i_file_acl ? (inode->i_sb->s_blocksize >> 9) : 0`)
2. Return `true` if `self.type_ == InodeType::SymLink` AND
   `self.blocks - ea_blocks == 0`.
   (Linux: `S_ISLNK(inode->i_mode) && inode->i_blocks - ea_blocks == 0`)

The logic is: a symlink with zero data blocks (after subtracting any EA block)
has its target stored inline in the `block_ptrs` array. If it has data blocks
allocated, the target is stored in those data blocks (slow symlink).

---

## 8.1.1  read_link — Read Symlink Target

Pre (read_link):
- `self` refers to a valid, non-freed `Inode`.
- `self.type_` is `InodeType::SymLink`.
- `self.fs` can be upgraded to a live `Arc<Ext2>`.

Post (read_link: success — fast symlink):
- Mirrors Linux `simple_get_link`: returns `(char *)ei->i_data` directly.
- Algorithm:
  1. Acquires read lock on `self.inner`.
  2. Calls `is_fast_symlink(block_size)` on the `InodeDesc`.
  3. If fast: reinterprets `desc.block_ptrs` (60 bytes) as a byte array.
     Reads `min(desc.size, MAX_FAST_SYMLINK_LEN - 1)` bytes (clamped to 59) from it.
     (Linux: `nd_terminate_link(ei->i_data, inode->i_size, sizeof(ei->i_data) - 1)`
      places null at `min(i_size, 59)`, so effective read length is `min(i_size, 59)`.)
     (Linux: `inode->i_link = (char *)ei->i_data`, inode.c:1484;
      `nd_terminate_link(ei->i_data, inode->i_size, sizeof(ei->i_data) - 1)`,
      inode.c:1486-1487)
  4. Converts the bytes to a UTF-8 `String` and returns it.

Post (read_link: success — slow symlink):
- Mirrors Linux `page_get_link`: reads target from data blocks via page cache.
- Algorithm:
  1. Acquires read lock on `self.inner`.
  2. Calls `is_fast_symlink(block_size)` on the `InodeDesc`.
  3. If slow: reads `desc.size` bytes from offset 0 of the inode's page cache
     using `page_cache.pages().read_bytes(0, &mut buf)`.
     (Linux: `page_get_link` → `read_mapping_folio` → `ext2_read_folio` →
      reads data block into page cache, then returns folio data as link target)
  4. Converts the bytes to a UTF-8 `String` and returns it.

Post (read_link: failure):
- `Err(EINVAL)` if `self.type_ != InodeType::SymLink`.
- `Err(EIO)` if filesystem reference is lost or page cache read fails.

Invariant:
- `read_link` is a read-only operation: no inode metadata is modified.
- The returned string length equals `desc.size` (the symlink target length
  without null terminator, matching Linux `inode->i_size` semantics).

---

## 8.1.2  write_link — Write Symlink Target

Pre (write_link):
- `self` refers to a valid, non-freed `Inode`.
- `self.type_` is `InodeType::SymLink`.
- `self.fs` can be upgraded to a live `Arc<Ext2>`.
- `target` is a non-empty string (the symlink target path).
- The inode is newly created (no existing symlink data).

Post (write_link: success — fast symlink):
- Mirrors Linux `ext2_symlink` fast path (namei.c:185-191).
- Condition: `target.len() + 1 <= MAX_FAST_SYMLINK_LEN` (i.e. `target.len() < 60`).
  (Linux: `l > sizeof(EXT2_I(inode)->i_data)` where `l = strlen(symname)+1`;
   fast when `strlen+1 <= 60`, i.e. `strlen <= 59`.)
- Algorithm:
  1. Acquires write lock on `self.inner`.
  2. Reinterprets `desc.block_ptrs` as a mutable 60-byte array.
  3. Copies `target.as_bytes()` into the byte array, followed by a null
     terminator byte (0x00).
     (Linux: `memcpy(inode->i_link, symname, l)` where `l` includes '\0',
      namei.c:189)
  4. Sets `desc.size = target.len() as u64`.
     (Linux: `inode->i_size = l-1`, namei.c:190)
  5. `desc.blocks` remains 0 (no data blocks allocated).
  6. Marks desc dirty and persists inode to disk.
     (Linux: `mark_inode_dirty(inode)`, namei.c:192)

Post (write_link: success — slow symlink):
- Mirrors Linux `ext2_symlink` slow path (namei.c:177-184).
- Condition: `target.len() + 1 > MAX_FAST_SYMLINK_LEN` (i.e. `target.len() >= 60`).
  (Linux: `l > sizeof(EXT2_I(inode)->i_data)` where `l = strlen+1`)
- Algorithm:
  1. Sets `desc.size = target.len() as u64`.
  2. Calls `self.resize(target.len())` to allocate data blocks and set up
     the page cache.
     (Linux: `page_symlink` → `aops->write_begin` allocates blocks)
  3. Writes `target.as_bytes()` into the page cache at offset 0 via
     `page_cache.pages().write_bytes(0, target.as_bytes())`.
     (Linux: `memcpy(folio_address(folio), symname, len - 1)`, namei.c:6292)
  4. Marks desc dirty and persists inode to disk.
     (Linux: `mark_inode_dirty(inode)`, namei.c:192)

Post (write_link: failure):
- `Err(EINVAL)` if `self.type_ != InodeType::SymLink`.
- `Err(ENAMETOOLONG)` if `target.len() + 1 > block_size` (i.e. `target.len() >= block_size`).
  (Linux: `if (l > sb->s_blocksize)` where `l = strlen(symname)+1`, namei.c:165-166.)
- `Err(ENOSPC)` if block allocation fails for slow symlink.
- `Err(EIO)` if page cache write or inode persistence fails.

Invariant:
- After successful `write_link`:
  - Fast: `desc.blocks == 0`, target stored in `block_ptrs` bytes.
  - Slow: `desc.blocks > 0`, target stored in data block(s) via page cache.
  - `desc.size == target.len() as u64` in both cases.
- `write_link` is idempotent for fast symlinks (overwrites block_ptrs).
  For slow symlinks, it should only be called once on a fresh inode.

---

## 8.1.3  VFS Integration — impl_for_vfs/inode.rs

The VFS trait stubs in `impl_for_vfs/inode.rs` delegate to the `Inode` methods:

```rust
fn read_link(&self) -> Result<SymbolicLink> {
    Inode::read_link(self).map(SymbolicLink::Plain)
}

fn write_link(&self, target: &str) -> Result<()> {
    Inode::write_link(self, target)
}
```

[DIFF]
Linux: Fast symlink detection happens once at `ext2_iget` time (inode.c:1483)
  and the result is baked into the inode_operations pointer
  (`ext2_fast_symlink_inode_operations` vs `ext2_symlink_inode_operations`).
  → Asterinas: Detection happens at each `read_link`/`write_link` call by
  checking `is_fast_symlink()` on the `InodeDesc`.
  Reason: Asterinas uses a single `Inode` struct for all types. There is no
  separate inode_operations dispatch; the fast/slow decision is made inline.
  The check is cheap (compare `blocks` field) and avoids storing extra state.

Linux: Slow symlink read uses `page_get_link` → `read_mapping_folio` which
  goes through the full page cache + `ext2_read_folio` → `mpage_read_folio`
  → `ext2_get_block` pipeline.
  → Asterinas: Slow symlink read uses `page_cache.pages().read_bytes()` which
  reads from the inode's PageCache. The PageCache backend calls
  `read_page_async` → `get_block` → block device read, matching the Linux
  data path.

Linux: Slow symlink write uses `page_symlink` → `aops->write_begin` /
  `aops->write_end` which allocates blocks through the address_space_operations.
  → Asterinas: Uses `resize()` to allocate blocks + set up page cache, then
  `page_cache.pages().write_bytes()` to write the target data.
  Reason: Asterinas has no `write_begin`/`write_end` aops; `resize` +
  direct page cache write achieves the same effect.

Linux: `ext2_symlink` checks `strlen(symname)+1 > sb->s_blocksize` (includes
  null terminator in the length comparison), rejecting when `strlen >= blocksize`.
  → Asterinas: Checks `target.len() + 1 > block_size` (equivalent to
  `target.len() >= block_size`), matching the Linux boundary exactly.

Linux: Fast symlink threshold is `l > sizeof(EXT2_I(inode)->i_data)` where
  `l = strlen+1` and `sizeof(i_data) = 60`. Fast when `strlen <= 59`.
  `memcpy` copies `l` bytes (including null terminator) into `i_data`.
  → Asterinas: Fast when `target.len() + 1 <= 60` (i.e. `target.len() < 60`).
  Copies target bytes plus a null terminator into `block_ptrs` byte view,
  matching the Linux on-disk layout exactly.
