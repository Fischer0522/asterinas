# Spec: VFS Contract Verification — Global Invariants
#
# Defines the state model, global invariants, and composition properties
# shared across all VFS trait method contracts for Ext2.
#
# Notation:
#   S      — pre-state (before method call)
#   S'     — post-state (after method call)
#   inode  — the Ext2 Inode actor receiving the message
#   fs     — the Ext2 filesystem actor
#   ⊥      — undefined / not applicable

## [STATE MODEL]

### Filesystem State (FS)

```
FS = {
    super_block   : SuperBlock,          -- on-disk superblock (Dirty<SuperBlock>)
    block_groups  : Vec<BlockGroup>,     -- per-group descriptor + bitmap caches
    inode_cache   : Map<u32, Arc<Inode>>,-- cached inode objects
    block_device  : Arc<dyn BlockDevice>,-- backing device
    root_inode    : Arc<Inode>,          -- cached root inode (ino=2)
    mounted       : bool,               -- filesystem is live
}
```

### Inode State (I)

```
I = {
    ino           : u32,                 -- inode number (immutable after creation)
    type_         : InodeType,           -- file type (immutable after creation)
    desc          : InodeDesc,           -- on-disk descriptor fields
    page_cache    : PageCache,           -- data page cache (for Reg/Dir/SymLink)
    xattr         : Option<Xattr>,       -- extended attribute block cache
    fs            : Weak<Ext2>,          -- back-pointer to filesystem
    is_freed      : bool,                -- marked for deferred reclamation
}
```

### InodeDesc Fields (D)

```
D = {
    mode          : u16,                 -- file permission bits
    uid           : u32,                 -- owner user id
    gid           : u32,                 -- owner group id
    size          : u64,                 -- file size in bytes
    atime         : Duration,            -- last access time
    mtime         : Duration,            -- last modification time
    ctime         : Duration,            -- last status change time
    dtime         : Duration,            -- deletion time (0 if alive)
    links_count   : u16,                 -- hard link count
    blocks        : u32,                 -- 512-byte block count
    flags         : u32,                 -- inode flags
    block_ptrs    : [u32; 15],           -- direct + indirect block pointers
    file_acl      : u32,                 -- xattr block number (0 = none)
    generation    : u32,                 -- inode generation number
}
```

### Directory State (Dir) — for InodeType::Dir inodes

```
Dir = {
    entries       : OrderedMap<String, (u32, DirEntryFileType)>,
    -- maps name → (child_ino, file_type)
    -- always contains "." → (self.ino, Dir) and ".." → (parent_ino, Dir)
    size          : u64,                 -- directory data size in bytes
}
```

### Actor Message

```
Message<M, Args, Ret> = {
    target        : ActorRef,            -- inode or filesystem reference
    method        : M,                   -- trait method name
    args          : Args,                -- typed argument tuple
    pre_state     : S,                   -- state snapshot before call
}

Response<Ret> = Ok(Ret) | Err(Errno)
```

## [GLOBAL INVARIANTS]

### INV-1: Inode identity immutability

```
∀ inode ∈ FS.inode_cache:
    inode.ino  = inode.ino@creation   ∧
    inode.type_ = inode.type_@creation
```

An inode's number and type never change after construction.

### INV-2: Root inode existence

```
FS.mounted ⟹
    FS.root_inode.ino = 2           ∧
    FS.root_inode.type_ = Dir       ∧
    FS.root_inode.desc.links_count ≥ 2
```

### INV-3: Directory dot-entries consistency

```
∀ inode where inode.type_ = Dir ∧ ¬inode.is_freed:
    inode.dir.entries["."]  = (inode.ino, Dir)  ∧
    inode.dir.entries[".."] ∈ FS.inode_cache
```

### INV-4: Link count consistency

```
∀ inode ∈ FS.inode_cache where ¬inode.is_freed:
    inode.desc.links_count =
        |{ (parent, name) : parent.dir.entries[name].0 = inode.ino }|
        + (if inode.type_ = Dir then 1 else 0)
        -- +1 for "." self-link in directories
```

### INV-5: Freed inode marking

```
∀ inode where inode.is_freed:
    inode.desc.links_count = 0  ∧
    inode.desc.dtime ≠ 0
```

### INV-6: Filesystem back-pointer validity

```
∀ inode ∈ FS.inode_cache:
    inode.fs.upgrade().is_some() ⟺ FS.mounted
```

### INV-7: Block allocation exclusivity

```
∀ b ∈ allocated_blocks:
    |{ inode : b ∈ inode.data_blocks ∪ inode.indirect_blocks ∪ {inode.desc.file_acl} }| ≤ 1
```

No block is referenced by more than one inode.

### INV-8: Superblock counter consistency

```
FS.super_block.free_blocks_count =
    Σ(bg.free_blocks_count for bg in FS.block_groups)

FS.super_block.free_inodes_count =
    Σ(bg.free_inodes_count for bg in FS.block_groups)
```

### INV-9: Timestamp monotonicity (ctime)

```
∀ inode, ∀ mutation M that modifies inode metadata:
    S'.desc.ctime ≥ S.desc.ctime
```

ctime never decreases (except via explicit set_ctime from VFS layer).

### INV-10: Xattr block validity

```
∀ inode where inode.desc.file_acl ≠ 0:
    block_at(inode.desc.file_acl).header.h_magic = XATTR_MAGIC  ∧
    block_at(inode.desc.file_acl).header.h_blocks = 1           ∧
    block_at(inode.desc.file_acl).header.h_refcount = 1
```

## [COMPOSITION PROPERTIES]

### COMP-1: Create-Lookup roundtrip

```
∀ dir where dir.type_ = Dir, ∀ name, type_, mode:
    let child = dir.create(name, type_, mode)?;
    ⟹ dir.lookup(name)? = child
    ∧ child.type_() = type_
    ∧ child.ino() = child.ino
```

### COMP-2: Link-Unlink inverse

```
∀ dir, inode where inode.type_ ≠ Dir:
    let links_before = inode.desc.links_count;
    dir.link(&inode, name)?;
    dir.unlink(name)?;
    ⟹ inode.desc.links_count = links_before
    ∧ dir.lookup(name) = Err(ENOENT)
```

### COMP-3: Create-Rmdir inverse (empty directory)

```
∀ dir:
    let child = dir.create("sub", Dir, mode)?;
    dir.rmdir("sub")?;
    ⟹ dir.lookup("sub") = Err(ENOENT)
    ∧ child.is_freed = true
```

### COMP-4: Rename preserves inode identity

```
∀ src_dir, dst_dir, old_name, new_name:
    let ino_before = src_dir.lookup(old_name)?.ino();
    src_dir.rename(old_name, &dst_dir, new_name)?;
    ⟹ dst_dir.lookup(new_name)?.ino() = ino_before
```

### COMP-5: Write-Read roundtrip

```
∀ inode where inode.type_ = Reg, ∀ offset, data:
    inode.write_at(offset, data, flags)?;
    let buf = inode.read_at(offset, len(data), flags)?;
    ⟹ buf = data[..written_len]
```

### COMP-6: Resize-Size consistency

```
∀ inode where inode.type_ = Reg:
    inode.resize(new_size)?;
    ⟹ inode.size() = new_size
```

### COMP-7: Xattr set-get roundtrip

```
∀ inode, ∀ name, value:
    inode.set_xattr(name, value, CREATE_OR_REPLACE)?;
    let result = inode.get_xattr(name, buf)?;
    ⟹ result = len(value) ∧ buf[..result] = value
```

### COMP-8: Xattr set-remove-get sequence

```
∀ inode, ∀ name, value:
    inode.set_xattr(name, value, CREATE_OR_REPLACE)?;
    inode.remove_xattr(name)?;
    ⟹ inode.get_xattr(name, buf) = Err(ENODATA)
```

### COMP-9: Symlink write-read roundtrip

```
∀ inode where inode.type_ = SymLink:
    inode.write_link(target)?;
    ⟹ inode.read_link()? = target
```

### COMP-10: Sync idempotence

```
∀ inode:
    inode.sync_all()?;
    inode.sync_all()?;
    ⟹ S'' = S'   -- second sync is a no-op on clean state
```

## [ERROR MODEL]

### Error preservation property

```
∀ method M, ∀ args:
    M(args) = Err(e) ⟹ S' = S
    -- failed operations must not modify observable state
    -- Exception: partial I/O (read_at/write_at) may return Ok(n) where n < requested
```

### Common error preconditions

```
ERR-COMMON-1: fs.upgrade() fails     ⟹ Err(EIO)    -- filesystem dropped
ERR-COMMON-2: block_device I/O fails ⟹ Err(EIO)    -- disk error
ERR-COMMON-3: name.len() > 255       ⟹ Err(EINVAL)  -- name too long
ERR-COMMON-4: name ∈ {"", ".", ".."}  ⟹ Err(EINVAL) or Err(EISDIR) -- reserved names
```

## [CONCURRENCY MODEL]

### Lock ordering

```
Lock acquisition order (must be respected to avoid deadlock):
    1. Inode.inner (RwMutex) — per-inode data lock
    2. Inode.xattr (RwMutex) — per-inode xattr lock
    3. FS.super_block (RwMutex) — global superblock lock
    4. BlockGroup locks — per-group bitmap/descriptor locks

For two-inode operations (rename, link):
    Acquire by ascending inode number: min(ino_a, ino_b) first.
```

### Atomicity guarantees

```
ATOM-1: Directory mutations (create/link/unlink/rmdir/rename)
    are atomic with respect to lookup — no partial state visible.

ATOM-2: Metadata updates (set_mode, set_owner, etc.)
    are atomic per-field.

ATOM-3: Inode persist (persist_inode_and_sync)
    writes the full inode descriptor atomically to disk.
```
