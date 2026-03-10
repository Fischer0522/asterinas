[PROMPT]
Provide additions to `kernel/src/fs/ext2/inode.rs` and `kernel/src/fs/ext2/impl_for_vfs/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_setattr          → fs/ext2/inode.c:1647-1679
ext2_setsize          → fs/ext2/inode.c:1275-1308
setattr_copy          → fs/attr.c (generic VFS helper)
Note: Linux ext2 has NO native fallocate. The old Asterinas ext2 provides
compatibility fallocate (Allocate, AllocateKeepSize, PunchHoleKeepSize)
which we replicate here.

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::fs::Ext2;
```

```rust
use crate::fs::utils::FallocMode;
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
impl Inode {
    pub(super) fn fs_arc(&self) -> Result<Arc<Ext2>>;
    pub(super) fn file_size(&self) -> usize;
    pub(super) fn resize(&self, new_size: usize) -> Result<()>;
}
```

```rust
impl InodeInner {
    fn persist_inode_and_sync(&mut self, fs: &Ext2) -> Result<()>;
}
```

```rust
/// FallocMode variants:
pub enum FallocMode {
    Allocate,
    AllocateKeepSize,
    AllocateUnshareRange,
    PunchHoleKeepSize,
    ZeroRange,
    ZeroRangeKeepSize,
    CollapseRange,
    InsertRange,
}
```

[GUARANTEE]
```rust
impl Inode {
    /// Implements fallocate operations for ext2.
    ///
    /// Linux ext2 has no native fallocate; this provides compatibility
    /// matching the old Asterinas ext2 implementation.
    pub(super) fn fallocate(&self, mode: FallocMode, offset: usize, len: usize) -> Result<()>;
}
```

[SPECIFICATION]

## 8.3.0  Existing Metadata Setters — Design Rationale

The following methods are already implemented and correct. This section
documents their design for completeness.

### Immediate-persist setters (return Result):
- `set_mode(mode)`: Updates `desc.perm`, sets `ctime = now()`, persists.
  Linux: `setattr_copy` sets `i_mode`, then `ext2_setattr` calls
  `mark_inode_dirty` (inode.c:1676).
- `set_uid(uid)`: Updates `desc.uid`, sets `ctime = now()`, persists.
  Linux: `setattr_copy` sets `i_uid`, `mark_inode_dirty`.
- `set_gid(gid)`: Updates `desc.gid`, sets `ctime = now()`, persists.
  Linux: `setattr_copy` sets `i_gid`, `mark_inode_dirty`.

These persist immediately because the VFS layer expects `set_owner`/
`set_group`/`set_mode` to be durable after return (chmod/chown semantics).

### Lazy setters (no disk I/O):
- `set_atime(time)`: Updates `desc.atime` in memory only.
- `set_mtime(time)`: Updates `desc.mtime` in memory only.
- `set_ctime(time)`: Updates `desc.ctime` in memory only.

These are lazy because the VFS layer batches time updates and relies on
`sync_all` (fsync) or inode eviction to persist them. This matches Linux
where `setattr_copy` updates in-memory times and `mark_inode_dirty` defers
the actual writeback to the journal/buffer layer.

No code changes needed for these methods.

---

## 8.3.1  fallocate — File Space Manipulation

Pre (fallocate):
- `self` refers to a valid, non-freed `Inode`.
- `self.type_` is `InodeType::File`.
- `offset` and `len` are valid (non-overflowing when added).

Note: Linux ext2 does NOT implement fallocate natively. The old Asterinas
ext2 provides compatibility support for three modes. We replicate that
behavior here.

### Post (fallocate: FallocMode::PunchHoleKeepSize):
- Zeroes data in the range `[offset, min(offset+len, file_size))` without
  changing the file size.
- Algorithm:
  1. Acquires read lock on `self.inner`.
  2. Gets `file_size = desc.size`.
  3. If `offset >= file_size`: return `Ok(())` (nothing to punch).
  4. Computes `end = min(file_size, offset + len)`.
  5. Calls `page_cache.fill_zeros(offset..end)` to zero the range.
  6. Returns `Ok(())`.
- Old ext2 ref: `ext2_old/inode.rs:806-819`.

### Post (fallocate: FallocMode::Allocate):
- Allocates real data blocks covering `[offset, offset + len)`.
- Newly allocated data blocks are zero-initialized before they become
  observable through mapped reads.
- If `offset + len > file_size`, updates `file_size = offset + len`
  after allocation and zeroing succeed.
- On allocator exhaustion, returns `Err(ENOSPC)`.

### Post (fallocate: FallocMode::AllocateKeepSize):
- Allocates real data blocks covering `[offset, offset + len)` without
  changing `file_size`.
- Newly allocated data blocks are zero-initialized on disk so that future
  reads after a later size extension observe zeros instead of stale contents.
- On allocator exhaustion, returns `Err(ENOSPC)`.

### Post (fallocate: unsupported modes):
- `ZeroRange`, `ZeroRangeKeepSize`, `CollapseRange`, `InsertRange`,
  `AllocateUnshareRange`: return `Err(EOPNOTSUPP)`.
- Old ext2 ref: `ext2_old/inode.rs:830-836`.

---

## 8.3.2  VFS Integration — impl_for_vfs/inode.rs

The `fallocate` stub currently returns `EOPNOTSUPP`. Replace with:

```rust
fn fallocate(&self, mode: FallocMode, offset: usize, len: usize) -> Result<()> {
    Inode::fallocate(self, mode, offset, len)
}
```

[DIFF]
Linux: `ext2_setattr` (inode.c:1647) is a single function that handles all
  attribute changes (size, mode, uid, gid, times) in one call, using
  `iattr->ia_valid` flags to determine which fields to update. It calls
  `setattr_copy` for the generic fields and `ext2_setsize` for truncation.
  → Asterinas: The VFS trait splits attribute updates into individual methods
  (`set_mode`, `set_owner`, `set_group`, `set_atime`, etc.) called separately
  by syscall handlers. Each immediate-persist setter does its own
  `persist_inode_and_sync`, which is slightly less efficient than Linux's
  single `mark_inode_dirty` but correct.

Linux: ext2 has NO fallocate support (`ext2_file_operations` does not set
  `.fallocate`).
  → Asterinas: Provides compatibility fallocate for `Allocate`,
  `AllocateKeepSize`, and `PunchHoleKeepSize`. Unlike the old compatibility
  behavior, allocation modes now consume real ext2 blocks and zero newly
  allocated blocks before exposure so reads remain zero-filled and ENOSPC is
  reported when free blocks run out.

Linux: Time updates in `setattr_copy` are in-memory only; writeback happens
  via `mark_inode_dirty` → periodic writeback or explicit fsync.
  → Asterinas: Time setters are lazy (in-memory only); writeback happens via
  `sync_all` → `persist_inode_and_sync`. Same logical model.
