[PROMPT]
Provide additions to `kernel/src/fs/ext2/fs.rs` and `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_iget                  → fs/ext2/inode.c:1387
ext2_set_inode_flags       → fs/ext2/inode.c:1357

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::inode::{Inode, InodeDesc, InodeInner};
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

[GUARANTEE]
impl Ext2 {
    pub(super) fn read_inode(&self, ino: u32) -> Result<Arc<Inode>>;
}

impl InodeInner {
    pub fn new(desc: Dirty<InodeDesc>, weak_self: Weak<Inode>, fs: Weak<Ext2>) -> Self;
}

[SPECIFICATION]
Pre (read_inode):
- `self.super_block` has been validated.
- `ino` is a 1-based inode number.

Post (read_inode: success):
- Calls `self.read_inode_desc(ino)` and obtains decoded `InodeDesc`.
- Computes `block_group_idx = (ino - 1) / sb.inodes_per_group()`.
- Constructs inode using two-layer abstraction:
  - outer immutable handle `Inode` (identity/type/group/fs link),
  - inner mutable state `InodeInner { desc, is_freed, weak_self, fs }`.
- `inner.desc` is initialized dirty-state wrapper over decoded descriptor.
- Returns `Ok(Arc<Inode>)`.

Post (read_inode: failure):
- Propagates `Err(EINVAL|EIO|ESTALE|EUCLEAN)` from `read_inode_desc`.
- Returns `Err(EIO)` if self-reference wiring fails.

Pre (InodeInner::new):
- `desc` is a valid decoded descriptor for one inode.
- `weak_self` and `fs` correspond to the same inode/filesystem object graph.

Post (InodeInner::new):
- Stores `desc` without altering decoded metadata.
- Initializes `is_freed = false`.
- Preserves `weak_self` and `fs` for callbacks/lookups.

Invariant:
- Public `Inode` identity and mutable inode metadata are separated.
- All mutable metadata transitions happen via `inner.desc` under lock.
- `Inode.type_` is consistent with `inner.desc.type_` at construction time.

[DIFF]
Linux: Ext2 inode private state is embedded in VFS inode (`EXT2_I(inode)`).
  → Asterinas: Uses explicit split between `Inode` and `InodeInner` guarded by `RwMutex`.
  Reason: Rust ownership and lock-based mutability model.

Linux: Global inode cache (`iget_locked`) is VFS-provided.
  → Asterinas: Cache infrastructure is deferred; this phase defines object construction contract only.
  Reason: Incremental bring-up of Ext2 stack.
