[PROMPT]
Provide `kernel/src/fs/ext2/impl_for_vfs/fs.rs` and `kernel/src/fs/ext2/impl_for_vfs/inode.rs`.
Also update integration wiring in `kernel/src/fs/ext2/mod.rs` and `kernel/src/fs/mod.rs` if needed.
Output Rust code only. No unsafe. No panic/assert/unimplemented.
Implementation logic MUST follow [SOURCE] Linux code.
TODO: finalize exact implementation scope and phase split for this spec.

[SOURCE]
## FileSystem trait mapping (super_operations equivalent)
TODO: verify final source mapping set.
ext2_sops                    → /root/linux/fs/ext2/super.c:366
ext2_sync_fs                 → /root/linux/fs/ext2/super.c:1308
ext2_statfs                  → /root/linux/fs/ext2/super.c:1446
ext2_fill_super (root inode) → /root/linux/fs/ext2/super.c:877

## Inode trait mapping (inode_operations + file_operations equivalent)
TODO: verify final source mapping set.
ext2_file_inode_operations   → /root/linux/fs/ext2/file.c:330
ext2_dir_inode_operations    → /root/linux/fs/ext2/namei.c:407
ext2_file_operations         → /root/linux/fs/ext2/file.c:313
ext2_dir_operations          → /root/linux/fs/ext2/dir.c:726
ext2_aops                    → /root/linux/fs/ext2/inode.c:971

## Read/write/readdir dispatch
TODO: verify final source mapping set.
ext2_file_read_iter          → /root/linux/fs/ext2/file.c:176
ext2_file_write_iter         → /root/linux/fs/ext2/file.c:189
ext2_readdir                 → /root/linux/fs/ext2/dir.c:257

## Namei operations dispatched by inode type
TODO: verify final source mapping set.
ext2_iget (type dispatch)    → /root/linux/fs/ext2/inode.c:1379-1491
ext2_lookup                  → /root/linux/fs/ext2/namei.c:67
ext2_create                  → /root/linux/fs/ext2/namei.c:102
ext2_symlink                 → /root/linux/fs/ext2/namei.c:143
ext2_mknod                   → /root/linux/fs/ext2/namei.c:189
ext2_link                    → /root/linux/fs/ext2/namei.c:204
ext2_mkdir                   → /root/linux/fs/ext2/namei.c:228
ext2_unlink                  → /root/linux/fs/ext2/namei.c:273
ext2_rmdir                   → /root/linux/fs/ext2/namei.c:283
ext2_rename                  → /root/linux/fs/ext2/namei.c:318
ext2_fsync                   → /root/linux/fs/ext2/file.c:195

## FsType registration/mount entry
TODO: verify final source mapping set.
ext2_fs_type                 → /root/linux/fs/ext2/super.c:1698
ext2_get_tree                → /root/linux/fs/ext2/super.c:1665
ext2_mount                   → /root/linux/fs/ext2/super.c:1677

[RELY]
```rust
use super::prelude::*;
// TODO: finalize imports for impl_for_vfs/fs.rs and impl_for_vfs/inode.rs.
```

```rust
use crate::fs::{
    registry::{FsProperties, FsType},
    utils::{
        AccessMode, DirentVisitor, Extension, FallocMode, FileSystem, FsEventSubscriberStats,
        FsFlags, Inode as VfsInode, InodeIo, InodeMode, InodeType, Metadata, MknodType,
        StatusFlags, SuperBlock, SymbolicLink, XattrName, XattrNamespace, XattrSetFlags,
    },
};
// TODO: trim to exact symbol set used by implementation.
```

```rust
use crate::fs::ext2::{Ext2, Inode};
// TODO: confirm whether VFS impl target type should be `ext2::Inode` directly.
```

```rust
// TODO: decide whether to keep/extend `kernel/src/fs/ext2/fs_type.rs`
// or move FsType glue into `impl_for_vfs/fs.rs`.
```

[GUARANTEE]
TODO: finalize exact function set and signatures for this phase.

impl FileSystem for Ext2 {
    fn name(&self) -> &'static str;
    fn sync(&self) -> Result<()>;
    fn root_inode(&self) -> Arc<dyn VfsInode>;
    fn sb(&self) -> SuperBlock;
    fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats;
    // TODO: decide whether `flags` / `set_fs_flags` need explicit overrides.
}

impl InodeIo for Inode {
    fn read_at(&self, offset: usize, writer: &mut VmWriter, status_flags: StatusFlags)
        -> Result<usize>;
    fn write_at(&self, offset: usize, reader: &mut VmReader, status_flags: StatusFlags)
        -> Result<usize>;
}

impl VfsInode for Inode {
    // TODO: complete full trait coverage from `kernel/src/fs/utils/inode.rs`.
    fn size(&self) -> usize;
    fn resize(&self, new_size: usize) -> Result<()>;
    fn metadata(&self) -> Metadata;
    fn ino(&self) -> u64;
    fn type_(&self) -> InodeType;
    fn mode(&self) -> Result<InodeMode>;
    fn set_mode(&self, mode: InodeMode) -> Result<()>;
    fn owner(&self) -> Result<Uid>;
    fn set_owner(&self, uid: Uid) -> Result<()>;
    fn group(&self) -> Result<Gid>;
    fn set_group(&self, gid: Gid) -> Result<()>;
    fn atime(&self) -> Duration;
    fn set_atime(&self, time: Duration);
    fn mtime(&self) -> Duration;
    fn set_mtime(&self, time: Duration);
    fn ctime(&self) -> Duration;
    fn set_ctime(&self, time: Duration);
    fn page_cache(&self) -> Option<Arc<Vmo>>;
    fn open(
        &self,
        access_mode: AccessMode,
        status_flags: StatusFlags,
    ) -> Option<Result<Box<dyn FileIo>>>;
    fn create(&self, name: &str, type_: InodeType, mode: InodeMode) -> Result<Arc<dyn VfsInode>>;
    fn mknod(&self, name: &str, mode: InodeMode, type_: MknodType) -> Result<Arc<dyn VfsInode>>;
    fn lookup(&self, name: &str) -> Result<Arc<dyn VfsInode>>;
    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize>;
    fn link(&self, old: &Arc<dyn VfsInode>, name: &str) -> Result<()>;
    fn unlink(&self, name: &str) -> Result<()>;
    fn rmdir(&self, name: &str) -> Result<()>;
    fn rename(&self, old_name: &str, target: &Arc<dyn VfsInode>, new_name: &str) -> Result<()>;
    fn read_link(&self) -> Result<SymbolicLink>;
    fn write_link(&self, target: &str) -> Result<()>;
    fn sync_all(&self) -> Result<()>;
    fn sync_data(&self) -> Result<()>;
    fn fallocate(&self, mode: FallocMode, offset: usize, len: usize) -> Result<()>;
    fn fs(&self) -> Arc<dyn FileSystem>;
    fn extension(&self) -> &Extension;
    fn set_xattr(
        &self,
        name: XattrName,
        value_reader: &mut VmReader,
        flags: XattrSetFlags,
    ) -> Result<()>;
    fn get_xattr(&self, name: XattrName, value_writer: &mut VmWriter) -> Result<usize>;
    fn list_xattr(&self, namespace: XattrNamespace, list_writer: &mut VmWriter) -> Result<usize>;
    fn remove_xattr(&self, name: XattrName) -> Result<()>;
}

[SPECIFICATION]
TODO: fill complete Hoare-style contracts for each guaranteed function.

Pre (global):
- TODO: define mount state and object validity requirements.
- TODO: define locking preconditions and lock order constraints.

Post (registration and mount wiring):
- TODO: define `FsType::create` behavior and error mapping.
- TODO: define how module `init()` path registers ext2 into registry.
- TODO: define whether `kernel/src/fs/mod.rs::init()` must call `ext2::init()`.

Post (FileSystem trait):
- TODO: define sync semantics and durability boundaries.
- TODO: define root inode acquisition semantics and failure behavior.
- TODO: define statfs/superblock projection fields and mapping rules.

Post (InodeIo trait):
- TODO: define O_DIRECT/non-O_DIRECT dispatch behavior.
- TODO: define read/write partial I/O, sparse-hole, and EOF behaviors.

Post (Inode trait):
- TODO: define metadata and ownership semantics.
- TODO: define lookup/create/link/unlink/rename state transitions.
- TODO: define symlink, xattr, and fallocate behavior.

Error model:
- TODO: enumerate all `Err(Errno::*)` cases per function.

Invariant:
- TODO: define inode lifecycle, cache coherence, and dirty/writeback invariants.
- TODO: define VFS-visible consistency invariants under concurrent operations.

[DIFF]
TODO: complete all Linux vs Asterinas adaptation notes.

Linux: TODO
  → Asterinas: TODO (reason)

Linux: TODO
  → Asterinas: TODO (reason)

[TEST]
TODO: add scenario matrix after contracts are finalized.

## FileSystem integration
- TODO: registration happy path.
- TODO: duplicate registration (`EEXIST`).
- TODO: mount with/without disk based on FsProperties.
- TODO: sync failure propagation.

## InodeIo integration
- TODO: read path happy case.
- TODO: write path happy case.
- TODO: O_DIRECT path dispatch.
- TODO: I/O failure propagation.

## Inode integration
- TODO: create/lookup/link/unlink/rmdir/rename state transitions.
- TODO: metadata updates (size, timestamps, mode, uid, gid).
- TODO: symlink read/write.
- TODO: xattr set/get/list/remove.
- TODO: sync_all/sync_data durability checks.
