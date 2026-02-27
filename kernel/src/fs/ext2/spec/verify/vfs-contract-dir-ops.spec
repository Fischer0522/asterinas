# Spec: VFS Contract Verification — Inode Directory Operations
#
# Formal contracts for directory mutation methods on the Inode trait.
# Each method is one Actor message with REQUIRE/ENSURE/ENSURE_ERR.
#
# Reference: kernel/src/fs/ext2/impl_for_vfs/inode.rs
# Reference: kernel/src/fs/ext2/inode.rs (internal implementations)
# Reference: kernel/src/fs/utils/inode.rs (trait definition)

## [SOURCE]

ext2_create   → /root/linux/fs/ext2/namei.c:102
ext2_mkdir    → /root/linux/fs/ext2/namei.c:228
ext2_mknod    → /root/linux/fs/ext2/namei.c:136
ext2_lookup   → /root/linux/fs/ext2/namei.c:67
ext2_readdir  → /root/linux/fs/ext2/dir.c:257
ext2_link     → /root/linux/fs/ext2/namei.c:204
ext2_unlink   → /root/linux/fs/ext2/namei.c:273
ext2_rmdir    → /root/linux/fs/ext2/namei.c:283
ext2_rename   → /root/linux/fs/ext2/namei.c:318

## [CONTRACTS]

### MSG-DIR-01: create(name: &str, type_: InodeType, mode: InodeMode) -> Result<Arc<dyn VfsInode>>

```
REQUIRE:
    self.type_ = Dir                        -- caller must be a directory
    self.fs.upgrade().is_some()             -- filesystem alive
    ¬self.is_freed                          -- directory not deleted
    self.desc.links_count > 0               -- directory is live
    name.as_bytes().len() ∈ [1, 255]        -- valid name length
    name ∉ {".", ".."}                      -- reserved names rejected
    type_ ∈ {Reg, Dir, SymLink, CharDevice, BlockDevice, NamedPipe, Socket}

ENSURE (Ok):
    let child = result;
    -- New inode allocated:
    child.ino ∉ S.FS.inode_cache
    child.type_ = type_
    child.desc.mode = mode.bits() as u16
    child.desc.links_count = (if type_ = Dir then 2 else 1)
    child.desc.uid = current_uid()
    child.desc.gid = self.desc.gid          -- inherit parent group
    child.desc.size = (if type_ = Dir then block_size else 0)
    child.desc.ctime = child.desc.mtime = child.desc.atime = now()

    -- Directory entry added:
    S'.dir.entries[name] = (child.ino, type_to_ft(type_))
    S'.desc.mtime = now()
    S'.desc.ctime = now()

    -- If child is Dir:
    --   child.dir.entries["."]  = (child.ino, Dir)
    --   child.dir.entries[".."] = (self.ino, Dir)
    --   S'.desc.links_count = S.desc.links_count + 1  (for ".." backlink)

ENSURE_ERR:
    Err(ENOTDIR)       -- self.type_ ≠ Dir
    Err(EINVAL)        -- name empty, or name ∈ {".", ".."}
    Err(ENOENT)        -- self.desc.links_count = 0 (dir removed)
    Err(EEXIST)        -- name already exists in directory
    Err(ENOSPC)        -- no free inodes or blocks
    Err(EIO)           -- fs.upgrade() failed or disk I/O error
    -- on error: S' = S (full rollback: free allocated inode/blocks)

FRAME:
    modified: self.dir.entries, self.desc.mtime, self.desc.ctime,
              self.desc.links_count (if child is Dir),
              FS.inode_cache (new entry), FS.super_block counters
```

### MSG-DIR-02: mknod(name: &str, mode: InodeMode, type_: MknodType) -> Result<Arc<dyn VfsInode>>

```
REQUIRE:
    self.type_ = Dir
    self.fs.upgrade().is_some()
    ¬self.is_freed
    name.as_bytes().len() ∈ [1, 255]
    name ∉ {".", ".."}
    type_ ∈ {CharDevice(dev), BlockDevice(dev), NamedPipe}

ENSURE (Ok):
    let child = result;
    child.type_ = match type_ {
        CharDevice(_) => CharDevice,
        BlockDevice(_) => BlockDevice,
        NamedPipe     => NamedPipe,
    }
    child.desc.mode = mode.bits() as u16
    child.desc.links_count = 1
    S'.dir.entries[name] = (child.ino, type_to_ft(child.type_))

    -- For CharDevice/BlockDevice:
    --   child.desc.block_ptrs encodes device_id (Linux i_block[0..2] encoding)

    S'.desc.mtime = now()
    S'.desc.ctime = now()

ENSURE_ERR:
    Err(ENOTDIR)  -- self.type_ ≠ Dir
    Err(EINVAL)   -- invalid name
    Err(EEXIST)   -- name already exists
    Err(ENOSPC)   -- no free inodes
    Err(EIO)      -- disk error
    -- on error: S' = S

FRAME:
    modified: self.dir.entries, self.desc.mtime, self.desc.ctime,
              child.desc.block_ptrs (device encoding),
              FS.inode_cache, FS.super_block counters
```

### MSG-DIR-03: lookup(name: &str) -> Result<Arc<dyn VfsInode>>

```
REQUIRE:
    self.type_ = Dir
    self.fs.upgrade().is_some()
    name.as_bytes().len() ∈ [1, 255]

ENSURE (Ok):
    let child = result;
    S.dir.entries[name] = (child.ino, _)    -- name exists in directory
    child.ino = S.dir.entries[name].0
    child.type_ matches S.dir.entries[name].1

ENSURE_ERR:
    Err(ENOTDIR)  -- self.type_ ≠ Dir
    Err(ENOENT)   -- name not found in directory entries
    Err(EIO)      -- disk I/O error reading inode

INVARIANT:
    S' = S  -- pure read, no state change

FRAME:
    no fields modified (may populate FS.inode_cache as side effect)
```

### MSG-DIR-04: readdir_at(offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize>

```
REQUIRE:
    self.type_ = Dir
    self.fs.upgrade().is_some()

ENSURE (Ok):
    let new_offset = result;
    new_offset ≥ offset
    -- visitor.visit() called for each entry starting at byte offset `offset`
    -- entries yielded in on-disk order (not sorted by name)
    -- each visit: (name, ino, type, entry_offset)
    -- stops when visitor.visit() returns Err or end of directory reached
    -- new_offset = byte offset after last successfully visited entry

ENSURE_ERR:
    Err(ENOTDIR)  -- self.type_ ≠ Dir
    Err(EIO)      -- corrupt directory entry or disk error

INVARIANT:
    S' = S  -- pure read

FRAME:
    no fields modified
```

### MSG-DIR-05: link(old: &Arc<dyn VfsInode>, name: &str) -> Result<()>

```
REQUIRE:
    self.type_ = Dir
    self.fs.upgrade().is_some()
    old.type_ ≠ Dir                         -- hard links to directories forbidden
    old.desc.links_count < MAX_LINK_COUNT   -- link count not saturated
    name.as_bytes().len() ∈ [1, 255]
    name ∉ {".", ".."}
    Arc::ptr_eq(self.fs, old.fs)            -- same filesystem
    name ∉ self.dir.entries                 -- name must not exist

ENSURE (Ok):
    S'.dir.entries[name] = (old.ino, type_to_ft(old.type_))
    S'(old).desc.links_count = S(old).desc.links_count + 1
    S'(old).desc.ctime = now()
    S'(old) persisted to disk

ENSURE_ERR:
    Err(EXDEV)      -- old is not from same filesystem (downcast fails)
    Err(ENOTDIR)    -- self.type_ ≠ Dir
    Err(EPERM)      -- old.type_ = Dir
    Err(EOVERFLOW)  -- old.desc.links_count ≥ MAX_LINK_COUNT
    Err(EINVAL)     -- invalid name or cross-fs
    Err(EEXIST)     -- name already exists in directory
    Err(EIO)        -- disk error
    -- on error: S' = S (rollback: decrement links_count if incremented)

FRAME:
    modified: self.dir.entries, old.desc.links_count, old.desc.ctime
```

### MSG-DIR-06: unlink(name: &str) -> Result<()>

```
REQUIRE:
    self.type_ = Dir
    self.fs.upgrade().is_some()
    name.as_bytes().len() ∈ [1, 255]
    name ∉ {".", ".."}
    name ∈ self.dir.entries                 -- entry must exist
    self.dir.entries[name].type_ ≠ Dir      -- must not be a directory

ENSURE (Ok):
    let child_ino = S.dir.entries[name].0;
    let child = FS.read_inode(child_ino);

    -- Directory entry removed:
    name ∉ S'.dir.entries

    -- Child inode updated:
    S'(child).desc.links_count = S(child).desc.links_count - 1
    S'(child).desc.ctime = now()

    -- If links_count reaches 0:
    --   S'(child).desc.dtime = now()
    --   S'(child).is_freed = true
    --   (actual block reclamation deferred to cache eviction)

ENSURE_ERR:
    Err(ENOTDIR)  -- self.type_ ≠ Dir
    Err(EINVAL)   -- invalid name
    Err(ENOENT)   -- name not found in directory
    Err(EISDIR)   -- target is a directory (use rmdir instead)
    Err(EIO)      -- disk error

FRAME:
    modified: self.dir.entries, child.desc.links_count,
              child.desc.ctime, child.desc.dtime, child.is_freed
```

### MSG-DIR-07: rmdir(name: &str) -> Result<()>

```
REQUIRE:
    self.type_ = Dir
    self.fs.upgrade().is_some()
    name.as_bytes().len() ∈ [1, 255]
    name ∉ {".", ".."}
    name ∈ self.dir.entries
    self.dir.entries[name].type_ = Dir      -- target must be a directory
    target_dir.dir.entries = {".", ".."}     -- target must be empty

ENSURE (Ok):
    let child_ino = S.dir.entries[name].0;
    let child = FS.read_inode(child_ino);

    -- Directory entry removed:
    name ∉ S'.dir.entries

    -- Child directory inode updated:
    S'(child).desc.links_count = 0
    S'(child).desc.ctime = now()
    S'(child).desc.dtime = now()
    S'(child).is_freed = true

    -- Parent link count decremented (lost ".." backlink):
    S'.desc.links_count = S.desc.links_count - 1
    S'.desc.mtime = now()
    S'.desc.ctime = now()

ENSURE_ERR:
    Err(ENOTDIR)    -- self.type_ ≠ Dir, or child.type_ ≠ Dir
    Err(EINVAL)     -- invalid name
    Err(ENOENT)     -- name not found
    Err(ENOTEMPTY)  -- child directory is not empty
    Err(EIO)        -- disk error

FRAME:
    modified: self.dir.entries, self.desc.links_count,
              self.desc.mtime, self.desc.ctime,
              child.desc.links_count, child.desc.ctime,
              child.desc.dtime, child.is_freed
```

### MSG-DIR-08: rename(old_name: &str, target: &Arc<dyn VfsInode>, new_name: &str) -> Result<()>

```
REQUIRE:
    self.type_ = Dir
    target.type_ = Dir
    self.fs.upgrade().is_some()
    Arc::ptr_eq(self.fs, target.fs)         -- same filesystem
    old_name.as_bytes().len() ∈ [1, 255]
    new_name.as_bytes().len() ∈ [1, 255]
    old_name ∉ {".", ".."}
    new_name ∉ {".", ".."}
    old_name ∈ self.dir.entries             -- source must exist

-- Sub-cases based on state:
-- CASE A: self.ino = target.ino ∧ old_name = new_name → no-op
-- CASE B: same dir rename (self.ino = target.ino)
-- CASE C: cross-dir rename (self.ino ≠ target.ino)
-- CASE D: destination exists (new_name ∈ target.dir.entries)
-- CASE E: destination does not exist

ENSURE (Ok, CASE A: self-rename no-op):
    S' = S

ENSURE (Ok, CASE B/C without existing destination):
    let moved_ino = S.self.dir.entries[old_name].0;
    let moved = FS.read_inode(moved_ino);

    -- Source entry removed:
    old_name ∉ S'.self.dir.entries

    -- Destination entry added:
    S'.target.dir.entries[new_name] = (moved_ino, type_to_ft(moved.type_))

    -- If moved is Dir and cross-dir:
    --   moved.dir.entries[".."] updated to target.ino
    --   S'.self.desc.links_count = S.self.desc.links_count - 1
    --   S'.target.desc.links_count = S.target.desc.links_count + 1

    -- Timestamps:
    S'(moved).desc.ctime = now()
    S'.self.desc.mtime = now()
    S'.self.desc.ctime = now()
    S'.target.desc.mtime = now()
    S'.target.desc.ctime = now()
```

```
ENSURE (Ok, CASE D: destination exists — replacement):
    let moved_ino = S.self.dir.entries[old_name].0;
    let moved = FS.read_inode(moved_ino);
    let existing_ino = S.target.dir.entries[new_name].0;
    let existing = FS.read_inode(existing_ino);

    -- Type compatibility check (enforced by VFS layer above, but ext2 also checks):
    --   If moved.type_ = Dir then existing.type_ must = Dir ∧ existing must be empty
    --   If moved.type_ ≠ Dir then existing.type_ must ≠ Dir

    -- Source entry removed:
    old_name ∉ S'.self.dir.entries

    -- Destination entry replaced (set_link):
    S'.target.dir.entries[new_name] = (moved_ino, type_to_ft(moved.type_))

    -- Replaced inode updated:
    S'(existing).desc.ctime = now()
    S'(existing).desc.links_count = S(existing).desc.links_count - 1
    --   If existing.type_ = Dir: extra -1 for lost "." self-link
    --   If links_count reaches 0:
    --     S'(existing).desc.dtime = now()
    --     S'(existing).is_freed = true

    -- If moved is Dir and cross-dir:
    --   moved.dir.entries[".."] updated to target.ino
    --   S'.self.desc.links_count -= 1
    --   (target.links_count not incremented because existing dir's ".." was already counted)
```

```
ENSURE_ERR:
    Err(ENOTDIR)    -- self.type_ ≠ Dir or target.type_ ≠ Dir
    Err(EXDEV)      -- target is not from same filesystem (downcast fails)
    Err(EISDIR)     -- old_name or new_name ∈ {".", ".."}
    Err(EINVAL)     -- cross-fs or invalid name
    Err(ENOENT)     -- old_name not found in source directory
    Err(ENOTEMPTY)  -- replacing non-empty directory
    Err(EIO)        -- disk error

CONCURRENCY:
    -- Lock ordering: acquire by ascending inode number
    -- Same-dir rename: single inode write lock
    -- Cross-dir rename: write_lock_two_inodes(min_ino, max_ino)

FRAME:
    modified: self.dir.entries, target.dir.entries,
              self.desc.links_count (if moved is Dir, cross-dir),
              target.desc.links_count (if moved is Dir, cross-dir, no replacement),
              moved.desc.ctime, moved.dir.entries[".."] (if Dir, cross-dir),
              existing.desc.links_count, existing.desc.ctime,
              existing.desc.dtime, existing.is_freed (if replaced)
```
