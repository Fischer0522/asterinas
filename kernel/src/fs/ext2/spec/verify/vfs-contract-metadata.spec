# Spec: VFS Contract Verification — Inode Metadata Methods
#
# Formal contracts for Inode trait metadata accessor/mutator methods.
# Each method is one Actor message with REQUIRE/ENSURE/ENSURE_ERR.
#
# Reference: kernel/src/fs/ext2/impl_for_vfs/inode.rs
# Reference: kernel/src/fs/utils/inode.rs (trait definition)

## [SOURCE]

Inode metadata accessors/mutators — no direct Linux ext2 function mapping.
These are VFS-layer abstractions. Ext2 delegates to InodeDesc fields.

Relevant Linux paths:
- ext2_getattr → /root/linux/fs/ext2/inode.c:1700
- ext2_setattr → /root/linux/fs/ext2/inode.c:1710
- generic_fillattr → /root/linux/fs/stat.c:94

## [CONTRACTS]

### MSG-META-01: size() -> usize

```
REQUIRE:
    inode.fs.upgrade().is_some()    -- filesystem alive

ENSURE:
    result = inode.desc.size as usize

ENSURE_ERR:
    ⊥  -- infallible method, never fails

INVARIANT:
    S' = S  -- pure read, no state change

FRAME:
    no fields modified
```

### MSG-META-02: resize(new_size: usize) -> Result<()>

```
REQUIRE:
    inode.fs.upgrade().is_some()
    inode.type_ ∈ {Reg, Dir}       -- only regular files and dirs can resize
    fs.block_size() > 0

ENSURE (Ok):
    S'.desc.size = new_size
    -- if new_size > S.desc.size: new blocks allocated, zero-filled
    -- if new_size < S.desc.size: excess blocks freed, page cache truncated
    -- if new_size = S.desc.size: no-op
    S'.desc.ctime ≥ S.desc.ctime
    S'.desc.mtime ≥ S.desc.mtime

ENSURE_ERR:
    Err(EIO)     -- fs.upgrade() failed or block_size = 0
    Err(ENOSPC)  -- no space for new blocks (when extending)
    -- on error: S' = S (rollback)

FRAME:
    modified: desc.size, desc.blocks, desc.ctime, desc.mtime, block_ptrs, page_cache
```

### MSG-META-03: metadata() -> Metadata

```
REQUIRE:
    ⊤  -- always callable

ENSURE:
    result.dev = ⊥                          -- not tracked by ext2
    result.ino = inode.ino as u64
    result.size = inode.desc.size
    result.blk_size = fs.block_size()
    result.blocks = inode.desc.blocks
    result.atime = inode.desc.atime
    result.mtime = inode.desc.mtime
    result.ctime = inode.desc.ctime
    result.type_ = inode.type_
    result.mode = InodeMode::from_bits_truncate(inode.desc.mode)
    result.nlinks = inode.desc.links_count
    result.uid = Uid::new(inode.desc.uid)
    result.gid = Gid::new(inode.desc.gid)

ENSURE_ERR:
    ⊥  -- infallible

INVARIANT:
    S' = S

FRAME:
    no fields modified
```

### MSG-META-04: ino() -> u64

```
REQUIRE:
    ⊤

ENSURE:
    result = inode.ino as u64

ENSURE_ERR:
    ⊥

INVARIANT:
    S' = S

FRAME:
    no fields modified
```

### MSG-META-05: type_() -> InodeType

```
REQUIRE:
    ⊤

ENSURE:
    result = inode.type_
    result ∈ {Reg, Dir, SymLink, CharDevice, BlockDevice, NamedPipe, Socket}

ENSURE_ERR:
    ⊥

INVARIANT:
    S' = S

FRAME:
    no fields modified
```

### MSG-META-06: mode() -> Result<InodeMode>

```
REQUIRE:
    ⊤

ENSURE (Ok):
    result = InodeMode::from_bits_truncate(inode.desc.mode as u32)

ENSURE_ERR:
    ⊥  -- Ext2 impl wraps in Ok(), never fails

INVARIANT:
    S' = S

FRAME:
    no fields modified
```

### MSG-META-07: set_mode(mode: InodeMode) -> Result<()>

```
REQUIRE:
    inode.fs.upgrade().is_some()

ENSURE (Ok):
    S'.desc.mode = mode.bits() as u16
    S'.desc.ctime = now()
    -- inode persisted to disk

ENSURE_ERR:
    Err(EIO)  -- fs.upgrade() failed or persist failed

FRAME:
    modified: desc.mode, desc.ctime
```

### MSG-META-08: owner() -> Result<Uid>

```
REQUIRE:
    ⊤

ENSURE (Ok):
    result = Uid::new(inode.desc.uid)

ENSURE_ERR:
    ⊥  -- Ext2 impl wraps in Ok(), never fails

INVARIANT:
    S' = S

FRAME:
    no fields modified
```

### MSG-META-09: set_owner(uid: Uid) -> Result<()>

```
REQUIRE:
    inode.fs.upgrade().is_some()

ENSURE (Ok):
    S'.desc.uid = uid.as_u32()
    S'.desc.ctime = now()

ENSURE_ERR:
    Err(EIO)  -- fs.upgrade() failed or persist failed

FRAME:
    modified: desc.uid, desc.ctime
```

### MSG-META-10: group() -> Result<Gid>

```
REQUIRE:
    ⊤

ENSURE (Ok):
    result = Gid::new(inode.desc.gid)

ENSURE_ERR:
    ⊥

INVARIANT:
    S' = S

FRAME:
    no fields modified
```

### MSG-META-11: set_group(gid: Gid) -> Result<()>

```
REQUIRE:
    inode.fs.upgrade().is_some()

ENSURE (Ok):
    S'.desc.gid = gid.as_u32()
    S'.desc.ctime = now()

ENSURE_ERR:
    Err(EIO)  -- fs.upgrade() failed or persist failed

FRAME:
    modified: desc.gid, desc.ctime
```

### MSG-META-12: atime() -> Duration

```
REQUIRE:
    ⊤

ENSURE:
    result = inode.desc.atime

INVARIANT:
    S' = S

FRAME:
    no fields modified
```

### MSG-META-13: set_atime(time: Duration)

```
REQUIRE:
    ⊤  -- infallible, no disk persist

ENSURE:
    S'.desc.atime = time

FRAME:
    modified: desc.atime
    -- NOTE: no ctime update, no disk persist (VFS layer responsibility)
```

### MSG-META-14: mtime() -> Duration

```
REQUIRE:
    ⊤

ENSURE:
    result = inode.desc.mtime

INVARIANT:
    S' = S

FRAME:
    no fields modified
```

### MSG-META-15: set_mtime(time: Duration)

```
REQUIRE:
    ⊤

ENSURE:
    S'.desc.mtime = time

FRAME:
    modified: desc.mtime
```

### MSG-META-16: ctime() -> Duration

```
REQUIRE:
    ⊤

ENSURE:
    result = inode.desc.ctime

INVARIANT:
    S' = S

FRAME:
    no fields modified
```

### MSG-META-17: set_ctime(time: Duration)

```
REQUIRE:
    ⊤

ENSURE:
    S'.desc.ctime = time

FRAME:
    modified: desc.ctime
```

### MSG-META-18: page_cache() -> Option<Arc<Vmo>>

```
REQUIRE:
    ⊤

ENSURE:
    result = Some(inode.page_cache.vmo())
    -- Ext2 always returns Some for all inode types that have a page cache

ENSURE_ERR:
    ⊥  -- infallible

INVARIANT:
    S' = S

FRAME:
    no fields modified
```

### MSG-META-19: open(access_mode, status_flags) -> Option<Result<Box<dyn FileIo>>>

```
REQUIRE:
    ⊤

ENSURE:
    result = None
    -- Ext2 regular files use the default VFS path (InodeHandle),
    -- not a custom FileIo. Returns None to signal "use default".

ENSURE_ERR:
    ⊥

INVARIANT:
    S' = S

FRAME:
    no fields modified
```

### MSG-META-20: fs() -> Arc<dyn FileSystem>

```
REQUIRE:
    inode.fs.upgrade().is_some()    -- filesystem must be alive

ENSURE:
    result = inode.fs.upgrade().unwrap()
    Arc::ptr_eq(&result, &FS)       -- same filesystem instance

ENSURE_ERR:
    ⊥  -- panics if fs dropped (unwrap inside fs_arc)

INVARIANT:
    S' = S

FRAME:
    no fields modified
```

### MSG-META-21: extension() -> &Extension

```
REQUIRE:
    ⊤

ENSURE:
    result = &inode.extension
    -- Extension holds dentry-cache and inode-level metadata for VFS

ENSURE_ERR:
    ⊥

INVARIANT:
    S' = S

FRAME:
    no fields modified
```
