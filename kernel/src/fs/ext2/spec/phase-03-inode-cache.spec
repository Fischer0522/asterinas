[PROMPT]
Provide additions to `kernel/src/fs/ext2/fs.rs` and `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_iget                  → fs/ext2/inode.c:1387
ext2_data_block_valid       → fs/ext2/balloc.c:1177

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::inode::{Inode, InodeDesc, RawInode, FilePerm};
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
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeDesc {
    pub raw: RawInode,
}
```

```rust
pub enum InodeType { Unknown, NamedPipe, CharDevice, Dir, BlockDevice, File, SymLink, Socket }
```

```rust
#[derive(Debug)]
pub struct Inode {
    ino: u32,
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
    flags: u32,
    faddr: u32,
    frag: u8,
    fsize: u8,
    file_acl: u32,
    dir_acl: u32,
    generation: u32,
    block_group_idx: usize,
    dir_start_lookup: u32,
    block_ptrs: [u32; 15],
    fs: Weak<Ext2>,
}
```

[GUARANTEE]
impl Ext2 {
    pub(super) fn read_inode(&self, ino: u32) -> Result<Arc<Inode>>;
}

impl Inode {
    pub(super) fn from_desc(ino: u32, desc: InodeDesc, fs: Weak<Ext2>) -> Result<Arc<Inode>>;
}

[SPECIFICATION]
Pre (read_inode):
- `self.super_block` has been validated by `load_super_block`.
- `ino` is a 1-based inode number.

Post (read_inode: success):
- Calls `self.read_inode_desc(ino)`.
- If `read_inode_desc` returns `Err(Errno::EINVAL|EIO)`, returns the same error.
- Uses `Inode::from_desc(ino, desc, self.self_ref.clone())` to construct the inode.
- Returns `Ok(Arc<Inode>)`.

Post (read_inode: failure):
- Returns `Err(Errno::ESTALE)` if Linux equivalent would reject deleted inode:
  - `desc.raw.links_count == 0` and (`desc.raw.mode == 0` or `desc.raw.dtime != 0`).
- Returns `Err(Errno::EFSCORRUPTED)` if `from_desc` detects corrupt on-disk metadata.
- Returns `Err(Errno::EIO)` for any I/O failure from `read_inode_desc`.
- Returns `Err(Errno::EINVAL)` for invalid inode numbers.

Pre (Inode::from_desc):
- `desc.raw` was read from disk (`RawInode`).

Post (Inode::from_desc: success):
- Decodes the following fields (Linux `ext2_iget` logic):
  - `mode = desc.raw.mode`.
  - `type_` derived from `mode` (file type bits); unknown → `Err(EINVAL)`.
  - `perm = FilePerm::from_bits_truncate(mode)`.
  - `uid = desc.raw.uid | (desc.raw.uid_high << 16)`.
  - `gid = desc.raw.gid | (desc.raw.gid_high << 16)`.
  - `links_count = desc.raw.links_count`.
  - `atime/ctime/mtime = desc.raw.atime/ctime/mtime` (UnixTime seconds).
  - `dtime = desc.raw.dtime` (used for deleted inode check).
  - `blocks = desc.raw.blocks`.
  - `flags = desc.raw.flags` (stored; Asterinas mapping of ext2_set_inode_flags is deferred).
  - `faddr = desc.raw.faddr`.
  - `frag = desc.raw.frag`.
  - `fsize = desc.raw.fsize`.
  - `file_acl = desc.raw.file_acl`.
  - `generation = desc.raw.generation`.
  - `block_group_idx = (ino - 1) / sb.inodes_per_group()`.
  - `dir_start_lookup = 0`.
  - `block_ptrs = desc.raw.block` (no byte swap).
- Deleted inode check (Linux e2fsck rule):
  - If `links_count == 0` and (`mode == 0` or `dtime != 0`) → `Err(Errno::ESTALE)`.
- Extended attribute block validity (Linux `ext2_data_block_valid`):
  - If `file_acl != 0` and `!sb.data_block_valid(file_acl, 1)` → `Err(Errno::EFSCORRUPTED)`.
- Size handling:
  - `size = desc.raw.size_lo`.
  - If `type_ == InodeType::File`, set `size |= (desc.raw.size_high as u64) << 32`.
  - Else set `dir_acl = desc.raw.size_high` and keep `size = size_lo`.
  - If `size > i64::MAX as u64` → `Err(Errno::EFSCORRUPTED)`.
- Clears in-memory deletion time after successful parse: set `dtime = 0`.
- Returns `Ok(Arc<Inode>)` with decoded metadata stored.

Post (Inode::from_desc: failure):
- Returns `Err(Errno::EINVAL)` if `mode` maps to unknown file type.
- Returns `Err(Errno::ESTALE)` for deleted inode check.
- Returns `Err(Errno::EFSCORRUPTED)` for invalid ACL block or invalid size.

Invariant:
- All decoding follows Linux `ext2_iget` field semantics.
