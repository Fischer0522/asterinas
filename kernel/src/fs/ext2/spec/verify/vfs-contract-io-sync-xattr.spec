# Spec: VFS Contract Verification — InodeIo, Symlink, Sync, Xattr
#
# Formal contracts for I/O, symlink, sync, fallocate, and xattr methods.
# Each method is one Actor message with REQUIRE/ENSURE/ENSURE_ERR.
#
# Reference: kernel/src/fs/ext2/impl_for_vfs/inode.rs
# Reference: kernel/src/fs/ext2/inode.rs (internal implementations)

## [SOURCE]

ext2_file_read_iter   → /root/linux/fs/ext2/file.c:283
ext2_file_write_iter  → /root/linux/fs/ext2/file.c:295
ext2_symlink          → /root/linux/fs/ext2/namei.c:143
ext2_get_link (fast)  → /root/linux/fs/ext2/inode.c:1483
page_get_link (slow)  → /root/linux/fs/namei.c:6227
ext2_fsync            → /root/linux/fs/ext2/file.c:195
ext2_xattr_get        → /root/linux/fs/ext2/xattr.c:195
ext2_xattr_set        → /root/linux/fs/ext2/xattr.c:405
ext2_xattr_list       → /root/linux/fs/ext2/xattr.c:287
ext2_xattr_delete     → /root/linux/fs/ext2/xattr.c:816

## [CONTRACTS]

### MSG-IO-01: read_at(offset, writer, status_flags) -> Result<usize>

```
REQUIRE:
    inode.type_ = Reg                       -- only regular files
    inode.fs.upgrade().is_some()

ENSURE (Ok):
    let n = result;
    -- Buffered path (no O_DIRECT):
    --   n = min(writer.avail(), inode.desc.size - offset)
    --   writer receives bytes from page cache at [offset..offset+n]
    --   if offset ≥ inode.desc.size: n = 0
    --   holes (unmapped blocks) read as zero bytes

    -- Direct I/O path (O_DIRECT):
    --   n = min(writer.avail(), inode.desc.size - offset)
    --   data read directly from block device, bypassing page cache

ENSURE_ERR:
    Err(EIO)  -- fs.upgrade() failed or block device read error

INVARIANT:
    S' = S  -- read does not modify inode state
    -- NOTE: atime update is VFS-layer responsibility, not ext2

FRAME:
    no fields modified
```

### MSG-IO-02: write_at(offset, reader, status_flags) -> Result<usize>

```
REQUIRE:
    inode.type_ = Reg
    inode.fs.upgrade().is_some()
    fs.block_size() > 0

ENSURE (Ok):
    let n = result;
    n ≤ reader.remain()

    -- Buffered path (no O_DIRECT):
    --   data written to page cache at [offset..offset+n]
    --   if offset + n > S.desc.size:
    --     S'.desc.size = offset + n
    --     new blocks allocated as needed
    --   S'.desc.mtime = now()
    --   S'.desc.ctime = now()

    -- Direct I/O path (O_DIRECT):
    --   data written directly to block device
    --   same size/timestamp semantics

ENSURE_ERR:
    Err(EIO)    -- fs.upgrade() failed or block device error
    Err(ENOSPC) -- no space for new blocks
    -- on error: cleanup via write_failed_cleanup
    --   discard_range on page cache
    --   truncate excess blocks if size was extended

FRAME:
    modified: desc.size (if extended), desc.mtime, desc.ctime,
              desc.blocks (if new blocks allocated), page_cache
```

### MSG-IO-03: read_link() -> Result<SymbolicLink>

```
REQUIRE:
    inode.type_ = SymLink
    inode.fs.upgrade().is_some()
    fs.block_size() > 0

ENSURE (Ok):
    result = SymbolicLink::Plain(target_string)
    -- Fast path (inline): if desc.size ≤ max_inline_symlink_len
    --   target read from desc.block_ptrs bytes (60 bytes max)
    -- Slow path (page cache): if desc.size > max_inline_symlink_len
    --   target read from page cache data
    -- target_string is valid UTF-8
    -- len(target_string) = desc.size (excluding NUL terminator)

ENSURE_ERR:
    Err(EINVAL)  -- inode.type_ ≠ SymLink
    Err(EIO)     -- fs.upgrade() failed, block_size = 0, or disk error
    Err(EIO)     -- target bytes are not valid UTF-8

INVARIANT:
    S' = S  -- pure read

FRAME:
    no fields modified
```

### MSG-IO-04: write_link(target: &str) -> Result<()>

```
REQUIRE:
    inode.type_ = SymLink
    inode.fs.upgrade().is_some()
    fs.block_size() > 0
    target.len() + 1 does not overflow usize

ENSURE (Ok):
    let with_nul = target.len() + 1;
    S'.desc.size = target.len() as u64

    -- Fast path (inline): if with_nul ≤ max_inline_symlink_len
    --   target + NUL stored in desc.block_ptrs bytes
    --   no blocks allocated
    -- Slow path (page cache): if with_nul > max_inline_symlink_len
    --   target + NUL written to page cache
    --   blocks allocated as needed

    S'.desc.ctime = now()
    S'.desc.mtime = now()

ENSURE_ERR:
    Err(EINVAL)        -- inode.type_ ≠ SymLink
    Err(EIO)           -- fs.upgrade() failed or block_size = 0
    Err(ENAMETOOLONG)  -- target.len() + 1 overflows
    Err(ENOSPC)        -- no space for blocks (slow path)
    -- on error: rollback (restore old size, free allocated blocks)

FRAME:
    modified: desc.size, desc.block_ptrs (fast) or page_cache (slow),
              desc.ctime, desc.mtime, desc.blocks
```

### MSG-IO-05: sync_all() -> Result<()>

```
REQUIRE:
    inode.fs.upgrade().is_some()

ENSURE (Ok):
    -- All dirty pages in page cache flushed to block device
    -- Inode descriptor persisted to disk (inode table)
    -- Xattr block flushed if dirty
    -- After return: on-disk state matches in-memory state

ENSURE_ERR:
    Err(EIO)  -- block device write error

FRAME:
    modified: page_cache dirty flags cleared, xattr dirty flag cleared
    -- no logical state change, only durability guarantee
```

### MSG-IO-06: sync_data() -> Result<()>

```
REQUIRE:
    inode.fs.upgrade().is_some()

ENSURE (Ok):
    -- All dirty data pages flushed to block device
    -- Inode metadata NOT necessarily persisted (unlike sync_all)
    -- Guarantees data durability, not metadata durability

ENSURE_ERR:
    Err(EIO)  -- block device write error

FRAME:
    modified: page_cache dirty flags cleared
    -- metadata may remain dirty
```

### MSG-IO-07: fallocate(mode: FallocMode, offset: usize, len: usize) -> Result<()>

```
REQUIRE:
    inode.type_ = Reg
    inode.fs.upgrade().is_some()
    fs.block_size() > 0

ENSURE (Ok):
    -- Linux ext2 has no native .fallocate file_operation.
    -- Asterinas provides a compatibility implementation.
    -- Behavior depends on FallocMode:
    --   Allocate: pre-allocate blocks for [offset..offset+len]
    --   PunchHole: deallocate blocks, zero data in range
    --   CollapseRange / InsertRange / ZeroRange: mode-specific

ENSURE_ERR:
    Err(EOPNOTSUPP) -- unsupported fallocate mode
    Err(ENOSPC)     -- no space for allocation
    Err(EIO)        -- disk error

FRAME:
    modified: desc.size (mode-dependent), desc.blocks, page_cache, block_ptrs
```

### MSG-XATTR-01: set_xattr(name, value_reader, flags) -> Result<()>

```
REQUIRE:
    inode.fs.upgrade().is_some()
    self.check_permission(MAY_WRITE) = Ok
    name is valid XattrName with namespace and full_name
    stripped_name.len() ≤ 255
    value_reader.remain() ≤ block_size

ENSURE (Ok):
    let stripped = name.strip_prefix();
    let idx = XattrNameIndex::from(name.namespace());

    -- If flags = CREATE_ONLY ∧ entry(idx, stripped) existed in S:
    --   unreachable (would have returned EEXIST)
    -- If flags = REPLACE_ONLY ∧ entry(idx, stripped) not in S:
    --   unreachable (would have returned ENODATA)

    -- Entry stored in xattr block:
    S'.xattr_block.entries contains (idx, stripped) → value
    -- where value = bytes read from value_reader

    -- If no xattr block existed:
    --   new block allocated, desc.file_acl updated
    S'.desc.file_acl ≠ 0

    -- Block persisted to disk immediately
    -- Inode persisted with updated file_acl

ENSURE_ERR:
    Err(EACCES)   -- permission check failed (MAY_WRITE)
    Err(ERANGE)   -- stripped_name.len() > 255 or value > block_size
    Err(EEXIST)   -- flags = CREATE_ONLY and entry already exists
    Err(ENODATA)  -- flags = REPLACE_ONLY and entry not found
    Err(ENOSPC)   -- no space in xattr block or no free disk blocks
    Err(EIO)      -- disk error

FRAME:
    modified: xattr.block_buf, xattr.bid, xattr.dirty,
              desc.file_acl, inode persisted
```

### MSG-XATTR-02: get_xattr(name, value_writer) -> Result<usize>

```
REQUIRE:
    inode.fs.upgrade().is_some()
    self.check_permission(MAY_READ) = Ok
    name is valid XattrName

ENSURE (Ok):
    let size = result;
    let stripped = name.strip_prefix();
    let idx = XattrNameIndex::from(name.namespace());

    -- Entry must exist in xattr block:
    S.xattr_block.entries contains (idx, stripped)

    -- Size query mode (writer.avail() = 0):
    --   size = entry.e_value_size
    --   no data copied

    -- Data read mode (writer.avail() > 0):
    --   size = entry.e_value_size
    --   writer receives value bytes

ENSURE_ERR:
    Err(EACCES)  -- permission check failed (MAY_READ)
    Err(ENODATA) -- entry not found (bid = 0 or no matching entry)
    Err(ERANGE)  -- value_writer.avail() > 0 but < value size
    Err(EIO)     -- disk error loading xattr block

INVARIANT:
    S' = S  -- pure read (ensure_loaded may cache block, but logical state unchanged)

FRAME:
    no logical fields modified (xattr.block_buf may be populated)
```

### MSG-XATTR-03: list_xattr(namespace, list_writer) -> Result<usize>

```
REQUIRE:
    inode.fs.upgrade().is_some()
    self.check_permission(MAY_ACCESS) = Ok
    namespace ∈ {User, Trusted, System, Security}

ENSURE (Ok):
    let total = result;

    -- total = Σ(prefix.len() + entry.name_len + 1) for each entry matching namespace
    -- where prefix = XattrNameIndex::prefix() for the entry's name_index

    -- Size query mode (writer.avail() = 0):
    --   total computed but no data written

    -- Data mode (writer.avail() > 0):
    --   for each matching entry: write "prefix" + name + NUL
    --   entries filtered by namespace (Asterinas differs from Linux here)

    -- If no xattr block (bid = 0): total = 0

ENSURE_ERR:
    Err(EACCES) -- permission check failed (MAY_ACCESS)
    Err(ERANGE) -- writer.avail() > 0 but insufficient for all names
    Err(EIO)    -- disk error loading xattr block

INVARIANT:
    S' = S  -- pure read

FRAME:
    no logical fields modified
```

### MSG-XATTR-04: remove_xattr(name) -> Result<()>

```
REQUIRE:
    inode.fs.upgrade().is_some()
    self.check_permission(MAY_WRITE) = Ok
    name is valid XattrName

ENSURE (Ok):
    let stripped = name.strip_prefix();
    let idx = XattrNameIndex::from(name.namespace());

    -- Entry removed from xattr block:
    S'.xattr_block.entries does NOT contain (idx, stripped)

    -- If last entry removed:
    --   xattr block freed on disk
    --   S'.desc.file_acl = 0

    -- Block persisted to disk immediately
    -- Inode persisted with updated file_acl

ENSURE_ERR:
    Err(EACCES)  -- permission check failed (MAY_WRITE)
    Err(ENODATA) -- entry not found (bid = 0 or no matching entry)
    Err(EIO)     -- disk error

FRAME:
    modified: xattr.block_buf, xattr.bid, xattr.dirty,
              desc.file_acl, inode persisted
```
