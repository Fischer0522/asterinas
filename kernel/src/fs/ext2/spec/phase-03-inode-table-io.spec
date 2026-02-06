[PROMPT]
Provide additions to `kernel/src/fs/ext2/fs.rs` and `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_get_inode             → fs/ext2/inode.c:1314
ext2_iget                  → fs/ext2/inode.c:1387

[RELY]
```rust
use core::mem::size_of;
```

```rust
use super::prelude::*;
```

```rust
use super::block_group::BlockGroup;
```

```rust
use super::inode::{FileFlags, FilePerm, InodeDesc, RawInode};
```

```rust
use super::super_block::SuperBlock;
```

```rust
use super::utils::Dirty;
```

```rust
use crate::fs::utils::FsEventSubscriberStats;
```

```rust
/// The Ext2 filesystem (core state holder).
#[derive(Debug)]
pub struct Ext2 {
    block_device: Arc<dyn BlockDevice>,
    super_block: RwMutex<Dirty<SuperBlock>>,
    block_groups: Vec<BlockGroup>,
    inodes_per_group: u32,
    blocks_per_group: u32,
    inode_size: usize,
    block_size: usize,
    group_descriptors_segment: USegment,
    fs_event_subscriber_stats: FsEventSubscriberStats,
    self_ref: Weak<Ext2>,
}
```

```rust
/// The root inode number (Linux EXT2_ROOT_INO).
pub const ROOT_INO: u32 = 2;
```

```rust
/// On-disk inode struct.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct RawInode { /* ext2 on-disk layout */ }
```

```rust
/// In-memory inode descriptor parsed from RawInode.
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeDesc {
    type_: InodeType,
    perm: FilePerm,
    uid: u32,
    gid: u32,
    size: u64,
    atime: UnixTime,
    ctime: UnixTime,
    mtime: UnixTime,
    dtime: UnixTime,
    links_count: u16,
    blocks: u32,
    flags: FileFlags,
    file_acl: u32,
    block_ptrs: [u32; 15],
}
```

[GUARANTEE]
impl Ext2 {
    pub(super) fn inode_table_block(&self, group_idx: usize, table_block_index: u32) -> Result<Bid>;
    pub(super) fn read_inode_desc(&self, ino: u32) -> Result<InodeDesc>;
}

impl TryFrom<&RawInode> for InodeDesc {
    type Error = Error;
    fn try_from(raw: &RawInode) -> Result<Self>;
}

[SPECIFICATION]
Pre (inode_table_block):
- `group_idx < self.block_groups.len()`.

Post (inode_table_block: success):
- Let `base = self.block_groups[group_idx].inode_table_bid()`.
- Returns `Ok(base + table_block_index as u64)`.

Post (inode_table_block: failure):
- Returns `Err(EIO)` if `group_idx` is out of range.

Pre (read_inode_desc):
- `self.super_block` has been validated.
- `ino` is a 1-based inode number.

Post (read_inode_desc: success):
- Validates inode number as Linux `ext2_get_inode`:
  - `(ino != ROOT_INO && ino < sb.first_ino())` → `Err(EINVAL)`.
  - `ino > sb.total_inodes()` → `Err(EINVAL)`.
- Computes inode table address:
  - `group_idx = (ino - 1) / sb.inodes_per_group()`.
  - `index_in_group = (ino - 1) % sb.inodes_per_group()`.
  - `offset_bytes = index_in_group * sb.inode_size()`.
  - `block_index = offset_bytes / sb.block_size()`.
  - `offset_in_block = offset_bytes % sb.block_size()`.
- Reads table block from `inode_table_block(group_idx, block_index)`.
- Extracts `RawInode` at `offset_in_block`.
- Returns `InodeDesc::try_from(&raw)`.

Post (read_inode_desc: failure):
- Returns `Err(EIO)` on block I/O failure.
- Returns `Err(EINVAL)` on invalid inode number.
- Propagates parse errors from `InodeDesc::try_from` (`ESTALE`, `EUCLEAN`/`EIO`, etc).

Pre (InodeDesc::try_from):
- `raw` bytes came from inode table and match ext2 inode layout.

Post (InodeDesc::try_from: success):
- Decodes Linux inode fields (`mode`, uid/gid high bits, timestamps, `i_blocks`, `i_flags`, `i_block`).
- Computes file size as Linux ext2:
  - regular file: `(size_high << 32) | size_lo`
  - non-regular: `size_lo`.
- Performs deleted inode check: `links_count == 0 && (mode == 0 || dtime != 0)` → `Err(ESTALE)`.

Invariant:
- Address arithmetic matches Linux `ext2_get_inode` exactly.
- In-memory descriptor is authoritative parsed state used by `InodeInner`.
