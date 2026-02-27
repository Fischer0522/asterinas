# Spec: VFS Contract Verification — FileSystem Trait
#
# Formal contracts for FileSystem trait methods implemented by Ext2.
# Each method is one Actor message with REQUIRE/ENSURE/ENSURE_ERR.
#
# Reference: kernel/src/fs/ext2/impl_for_vfs/fs.rs
# Reference: kernel/src/fs/utils/fs.rs (trait definition)

## [SOURCE]

ext2_sops          → /root/linux/fs/ext2/super.c:366
ext2_sync_fs       → /root/linux/fs/ext2/super.c:1308
ext2_statfs        → /root/linux/fs/ext2/super.c:1446
ext2_fill_super    → /root/linux/fs/ext2/super.c:877
ext2_fs_type       → /root/linux/fs/ext2/super.c:1698

## [CONTRACTS]

### MSG-FS-01: name() -> &'static str

```
REQUIRE:
    ⊤  -- always callable

ENSURE:
    result = "ext2"

ENSURE_ERR:
    ⊥  -- infallible

INVARIANT:
    S' = S

FRAME:
    no fields modified
```

### MSG-FS-02: sync() -> Result<()>

```
REQUIRE:
    FS.mounted = true
    FS.block_device is accessible

ENSURE (Ok):
    -- Phase 1: sync_all_inodes()
    --   ∀ inode ∈ FS.inode_cache:
    --     dirty pages flushed to block device
    --     inode descriptor persisted to inode table
    --     xattr block flushed if dirty

    -- Phase 2: sync_metadata()
    --   superblock persisted to disk (if dirty)
    --   block group descriptors persisted to disk (if dirty)

    -- Phase 3: block_device.sync()
    --   device-level flush/barrier issued

    -- After return: all in-memory state durable on disk

ENSURE_ERR:
    Err(EIO)  -- any phase fails (inode sync, metadata sync, or device sync)
    -- partial sync: earlier phases may have completed before failure

FRAME:
    modified: all dirty flags cleared across inodes, superblock, block groups
    -- no logical state change, only durability guarantee
```

### MSG-FS-03: root_inode() -> Arc<dyn Inode>

```
REQUIRE:
    FS.mounted = true

ENSURE:
    result.ino() = 2                        -- EXT2_ROOT_INO
    result.type_() = Dir
    result.desc.links_count ≥ 2             -- "." + parent ref
    Arc::ptr_eq(&result, &FS.root_inode)    -- same cached instance

ENSURE_ERR:
    ⊥  -- infallible (root_inode cached at mount time)

INVARIANT:
    S' = S

FRAME:
    no fields modified
```

### MSG-FS-04: sb() -> SuperBlock

```
REQUIRE:
    FS.mounted = true

ENSURE:
    result.magic  = MAGIC_NUM as u64        -- 0xEF53
    result.bsize  = FS.super_block.block_size()
    result.blocks = FS.super_block.total_blocks() as usize
    result.bfree  = FS.super_block.free_blocks_count() as usize
    result.bavail = FS.super_block.free_blocks_count()
                      .saturating_sub(FS.super_block.reserved_blocks_count()) as usize
    result.files  = FS.super_block.total_inodes() as usize
    result.ffree  = FS.super_block.free_inodes_count() as usize
    result.fsid   = 0
    result.namelen = NAME_MAX               -- 255
    result.frsize = FS.super_block.fragment_size()
    result.flags  = 0

ENSURE_ERR:
    ⊥  -- infallible

INVARIANT:
    S' = S

FRAME:
    no fields modified
```

### MSG-FS-05: fs_event_subscriber_stats() -> &FsEventSubscriberStats

```
REQUIRE:
    ⊤

ENSURE:
    result = &FS.fs_event_subscriber_stats
    -- returns reference to the per-filesystem event subscriber counter

ENSURE_ERR:
    ⊥  -- infallible

INVARIANT:
    S' = S

FRAME:
    no fields modified
```
