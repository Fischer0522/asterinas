// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Tier 1: Simple Read Methods
//
// Tier 1 methods are single-step, lock-free or read-locked accessors.
// They never mutate state and have no crash/rollback concerns.
//
// Reference: 00-state-model.spec for state notation.

/// =============================================================================
/// PROTOCOL: ino()
/// =============================================================================
/// Returns the inode number. No lock needed — field is immutable.
///
/// CODE: kernel/src/fs/ext2/inode.rs:86-88
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:63-65

PROTOCOL ino {
    TIER: 1
    SIGNATURE: fn ino(&self) -> u64

    STEPS:
      1. RETURN self.ino as u64

    LOCKS: none
    CRASH: N/A (pure read)
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN = S.M.inode[self].ino as u64
}

/// =============================================================================
/// PROTOCOL: type_()
/// =============================================================================
/// Returns the inode type. No lock needed — field is immutable.
///
/// CODE: kernel/src/fs/ext2/inode.rs:272-274
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:67-69

PROTOCOL type_ {
    TIER: 1
    SIGNATURE: fn type_(&self) -> InodeType

    STEPS:
      1. RETURN self.type_

    LOCKS: none
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN = S.M.inode[self].type_
}

/// =============================================================================
/// PROTOCOL: size()
/// =============================================================================
/// Returns the file size. Acquires read lock on inner.
///
/// CODE: kernel/src/fs/ext2/inode.rs:100-102
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:51-53

PROTOCOL size {
    TIER: 1
    SIGNATURE: fn size(&self) -> usize

    STEPS:
      1. LOCK inner = self.inner.read()
      2. result = inner.desc.size as usize
      3. UNLOCK inner
      4. RETURN result

    LOCKS: READ(self.inner)
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN = S.M.inode[self].desc.size as usize
}

/// =============================================================================
/// PROTOCOL: mode()
/// =============================================================================
/// Returns the permission mode. Acquires read lock.
///
/// CODE: kernel/src/fs/ext2/inode.rs:276-278
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:71-73

PROTOCOL mode {
    TIER: 1
    SIGNATURE: fn mode(&self) -> Result<InodeMode>

    STEPS:
      1. LOCK inner = self.inner.read()
      2. result = InodeMode::from_bits_truncate(inner.desc.perm.bits() as _)
      3. UNLOCK inner
      4. RETURN Ok(result)

    LOCKS: READ(self.inner)
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN = Ok(S.M.inode[self].desc.perm as InodeMode)
}

/// =============================================================================
/// PROTOCOL: owner()
/// =============================================================================
/// Returns the owner UID. Acquires read lock.
///
/// CODE: kernel/src/fs/ext2/inode.rs:288-290
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:79-81

PROTOCOL owner {
    TIER: 1
    SIGNATURE: fn owner(&self) -> Result<Uid>

    STEPS:
      1. LOCK inner = self.inner.read()
      2. uid = inner.desc.uid
      3. UNLOCK inner
      4. RETURN Ok(Uid::new(uid))

    LOCKS: READ(self.inner)
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN = Ok(Uid::new(S.M.inode[self].desc.uid))
}

/// =============================================================================
/// PROTOCOL: group()
/// =============================================================================
/// Returns the group GID. Acquires read lock.
///
/// CODE: kernel/src/fs/ext2/inode.rs:300-302
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:87-89

PROTOCOL group {
    TIER: 1
    SIGNATURE: fn group(&self) -> Result<Gid>

    STEPS:
      1. LOCK inner = self.inner.read()
      2. gid = inner.desc.gid
      3. UNLOCK inner
      4. RETURN Ok(Gid::new(gid))

    LOCKS: READ(self.inner)
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN = Ok(Gid::new(S.M.inode[self].desc.gid))
}

/// =============================================================================
/// PROTOCOL: atime()
/// =============================================================================
/// CODE: kernel/src/fs/ext2/inode.rs:312-314
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:95-97

PROTOCOL atime {
    TIER: 1
    SIGNATURE: fn atime(&self) -> Duration

    STEPS:
      1. LOCK inner = self.inner.read()
      2. result = inner.desc.atime
      3. UNLOCK inner
      4. RETURN result

    LOCKS: READ(self.inner)
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN = S.M.inode[self].desc.atime
}

/// =============================================================================
/// PROTOCOL: mtime()
/// =============================================================================
/// CODE: kernel/src/fs/ext2/inode.rs:320-322
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:103-105

PROTOCOL mtime {
    TIER: 1
    SIGNATURE: fn mtime(&self) -> Duration

    STEPS:
      1. LOCK inner = self.inner.read()
      2. result = inner.desc.mtime
      3. UNLOCK inner
      4. RETURN result

    LOCKS: READ(self.inner)
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN = S.M.inode[self].desc.mtime
}

/// =============================================================================
/// PROTOCOL: ctime()
/// =============================================================================
/// CODE: kernel/src/fs/ext2/inode.rs:328-330
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:111-113

PROTOCOL ctime {
    TIER: 1
    SIGNATURE: fn ctime(&self) -> Duration

    STEPS:
      1. LOCK inner = self.inner.read()
      2. result = inner.desc.ctime
      3. UNLOCK inner
      4. RETURN result

    LOCKS: READ(self.inner)
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN = S.M.inode[self].desc.ctime
}

/// =============================================================================
/// PROTOCOL: metadata()
/// =============================================================================
/// Returns a snapshot of all inode metadata fields.
///
/// CODE: kernel/src/fs/ext2/inode.rs:241-270
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:59-61

PROTOCOL metadata {
    TIER: 1
    SIGNATURE: fn metadata(&self) -> Metadata

    STEPS:
      1. LOCK inner = self.inner.read()
      2. Resolve fs via Weak::upgrade for dev and blk_size
      3. If type_ ∈ {CharDevice, BlockDevice}: rdev = inner.desc.decode_device_id()
         Else: rdev = 0
      4. Construct Metadata { dev, ino, size, blk_size, blocks, atime, mtime,
                              ctime, type_, mode, nlinks, uid, gid, rdev }
      5. UNLOCK inner
      6. RETURN metadata

    LOCKS: READ(self.inner)
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN.ino = S.M.inode[self].ino
      RETURN.size = S.M.inode[self].desc.size
      RETURN.nlinks = S.M.inode[self].desc.links_count
}

/// =============================================================================
/// PROTOCOL: page_cache()
/// =============================================================================
/// Returns the page cache VMO for this inode.
///
/// CODE: kernel/src/fs/ext2/inode.rs:1242-1244
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:119-121

PROTOCOL page_cache {
    TIER: 1
    SIGNATURE: fn page_cache(&self) -> Option<Arc<Vmo>>

    STEPS:
      1. LOCK inner = self.inner.read()
      2. vmo = inner.page_cache.pages().clone()
      3. UNLOCK inner
      4. RETURN Some(vmo)

    LOCKS: READ(self.inner)
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN = Some(S.M.page_cache[self].pages())
}

/// =============================================================================
/// PROTOCOL: open()
/// =============================================================================
/// Ext2 inodes do not provide custom FileIo — always returns None.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/inode.rs:123-129

PROTOCOL open {
    TIER: 1
    SIGNATURE: fn open(&self, _: AccessMode, _: StatusFlags) -> Option<Result<Box<dyn FileIo>>>

    STEPS:
      1. RETURN None

    LOCKS: none
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN = None
}

/// =============================================================================
/// PROTOCOL: fs()
/// =============================================================================
/// Returns the owning filesystem. Panics if fs is dropped.
///
/// CODE: kernel/src/fs/ext2/inode.rs:94-98
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:206-209

PROTOCOL fs {
    TIER: 1
    SIGNATURE: fn fs(&self) -> Arc<dyn FileSystem>

    STEPS:
      1. Upgrade self.fs Weak → Arc
      2. RETURN Arc as Arc<dyn FileSystem>

    LOCKS: none (Weak::upgrade is atomic)
    CRASH: N/A
    ROLLBACK: N/A

    REQUIRE: self.fs.upgrade().is_some()
    ENSURE:
      S' = S
      RETURN points to the Ext2 instance owning this inode
}

/// =============================================================================
/// PROTOCOL: extension()
/// =============================================================================
/// Returns the VFS extension object. No lock needed — field is immutable.
///
/// CODE: kernel/src/fs/ext2/inode.rs:1238-1240
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:211-213

PROTOCOL extension {
    TIER: 1
    SIGNATURE: fn extension(&self) -> &Extension

    STEPS:
      1. RETURN &self.extension

    LOCKS: none
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
}

/// =============================================================================
/// PROTOCOL: lookup()
/// =============================================================================
/// Looks up a child inode by name in this directory.
///
/// CODE: kernel/src/fs/ext2/inode.rs:815-822
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:153-155

PROTOCOL lookup {
    TIER: 1
    SIGNATURE: fn lookup(&self, name: &str) -> Result<Arc<dyn VfsInode>>

    REQUIRE:
      self.type_ = Dir

    STEPS:
      1. GUARD self.type_ == Dir, else RETURN Err(ENOTDIR)
      2. LOCK inner = self.inner.read()
      3. ino = inner.find_entry(name)?   // scans dir page cache
      4. UNLOCK inner
      5. child = fs.read_inode(ino)?     // may hit inode cache
      6. RETURN Ok(child)

    LOCKS: READ(self.inner)
    CRASH: N/A (pure read)
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN.ino = ino of entry named `name` in self's directory
    ENSURE_ERR:
      S' = S
      ENOTDIR if self is not a directory
      ENOENT  if name not found
}

/// =============================================================================
/// PROTOCOL: readdir_at()
/// =============================================================================
/// Iterates directory entries starting at byte offset.
///
/// CODE: kernel/src/fs/ext2/inode.rs:868-878
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:157-159

PROTOCOL readdir_at {
    TIER: 1
    SIGNATURE: fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize>

    REQUIRE:
      self.type_ = Dir

    STEPS:
      1. GUARD self.type_ == Dir, else RETURN Err(ENOTDIR)
      2. LOCK inner = self.inner.read()
      3. Iterate directory entries from `offset` via DirEntryIter
      4. For each valid entry: call visitor.visit(name, ino, type_, offset)
      5. UNLOCK inner
      6. RETURN Ok(next_offset)

    LOCKS: READ(self.inner)
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
      visitor received all entries from offset to end (or until visitor stops)
    ENSURE_ERR:
      S' = S
      ENOTDIR if self is not a directory
}
