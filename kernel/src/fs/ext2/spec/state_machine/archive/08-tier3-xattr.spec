// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Tier 3: Extended Attributes
//
// Xattr operations use a separate RwMutex<Xattr> lock in addition to
// the inode inner lock. set_xattr and remove_xattr are two-lock protocols.
//
// Reference: 00-state-model.spec for state notation.

/// =============================================================================
/// PROTOCOL: get_xattr
/// =============================================================================
/// Reads one extended attribute value.
///
/// CODE: kernel/src/fs/ext2/inode.rs:339-349
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:225-228
/// LINUX_REF: fs/ext2/xattr.c:195-275 (ext2_xattr_get)

PROTOCOL get_xattr {
    TIER: 3
    SIGNATURE: fn get_xattr(&self, name: XattrName, writer: &mut VmWriter) -> Result<usize>

    REQUIRE:
      self.xattr.is_some()  // only Dir and File inodes
      caller has MAY_READ permission

    STEPS:
      1. VFS dispatch: self.check_permission(Permission::MAY_READ)?
      2. LOCK   xattr = self.xattr.as_ref()?.write()
      3. EFFECT result = xattr.get_xattr(name, writer)?
      4. UNLOCK xattr
      5. RETURN Ok(result)

    LOCKS: XATTR_WRITE(self.xattr)
    // Note: uses write lock because Xattr::get_xattr may lazy-load block

    CRASH: N/A (pure read after potential lazy load)
    ROLLBACK: N/A

    ENSURE:
      S' = S
      writer contains the xattr value bytes
      RETURN = number of bytes written
    ENSURE_ERR:
      EOPNOTSUPP if xattr not supported (non-Dir/File)
      ENODATA    if attribute not found
      ERANGE     if writer too small
}

/// =============================================================================
/// PROTOCOL: list_xattr
/// =============================================================================
/// Lists extended attribute names in one namespace.
///
/// CODE: kernel/src/fs/ext2/inode.rs:354-368
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:230-233
/// LINUX_REF: fs/ext2/xattr.c:287-364 (ext2_xattr_list)

PROTOCOL list_xattr {
    TIER: 3
    SIGNATURE: fn list_xattr(&self, ns: XattrNamespace, writer: &mut VmWriter) -> Result<usize>

    REQUIRE:
      self.xattr.is_some()
      caller has MAY_ACCESS permission

    STEPS:
      1. VFS dispatch: self.check_permission(Permission::MAY_ACCESS)?
      2. LOCK   xattr = self.xattr.as_ref()?.write()
      3. EFFECT result = xattr.list_xattr(ns, writer)?
      4. UNLOCK xattr
      5. RETURN Ok(result)

    LOCKS: XATTR_WRITE(self.xattr)
    CRASH: N/A (pure read)
    ROLLBACK: N/A

    ENSURE:
      S' = S
      writer contains null-separated xattr names in namespace
      RETURN = total bytes written
    ENSURE_ERR:
      EOPNOTSUPP if xattr not supported
      ERANGE     if writer too small
}

/// =============================================================================
/// PROTOCOL: set_xattr
/// =============================================================================
/// Creates or replaces one extended attribute. Two-lock protocol.
///
/// CODE: kernel/src/fs/ext2/inode.rs:373-396
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:215-223
/// LINUX_REF: fs/ext2/xattr.c:405-651 (ext2_xattr_set)

PROTOCOL set_xattr {
    TIER: 3
    SIGNATURE: fn set_xattr(&self, name: XattrName, reader: &mut VmReader, flags: XattrSetFlags) -> Result<()>

    REQUIRE:
      self.xattr.is_some()
      caller has MAY_WRITE permission

    STEPS:
      // Phase 1: Xattr block mutation (xattr lock)
      1. VFS dispatch: self.check_permission(Permission::MAY_WRITE)?
      2. LOCK    xattr = self.xattr.as_ref()?.write()
      3. EFFECT  xattr.set_xattr(name, reader, flags)?
      4. new_bid = xattr.bid()
      5. UNLOCK  xattr

      // Phase 2: Update inode file_acl + persist (inode lock)
      6. EFFECT  fs = self.fs_arc()?
      7. LOCK    inner = self.inner.write()
      8. MUTATE  inner.desc.file_acl = new_bid
      9. MUTATE  inner.desc.ctime = now()
      10. PERSIST inner.persist_inode_and_sync(&fs)?
      11. UNLOCK  inner
      12. RETURN  Ok(())

    LOCKS: XATTR_WRITE(self.xattr) → WRITE(self.inner)
    // Lock ordering: xattr lock first, then inode inner lock

    CRASH_ANALYSIS:
      - Crash after phase 1, before phase 2:
        Xattr block written but inode file_acl not updated.
        Orphan xattr block; fsck can detect via refcount.
      - Crash after phase 2: fully committed.

    ROLLBACK:
      Phase 1 failure: xattr.set_xattr handles internal rollback.
      Phase 2 failure: xattr block is written but file_acl not updated.
      No explicit rollback of xattr block on persist failure.

    ENSURE:
      S'.M.xattr[self] contains (name, value)
      S'.M.inode[self].desc.file_acl = new xattr block bid
      S'.M.inode[self].desc.ctime ≥ S.M.inode[self].desc.ctime
    ENSURE_ERR:
      EOPNOTSUPP if xattr not supported
      EEXIST     if XATTR_CREATE and attribute exists
      ENODATA    if XATTR_REPLACE and attribute missing
      ENOSPC     if no space for xattr block
}

/// =============================================================================
/// PROTOCOL: remove_xattr
/// =============================================================================
/// Removes one extended attribute. Two-lock protocol.
///
/// CODE: kernel/src/fs/ext2/inode.rs:401-418
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:235-238
/// LINUX_REF: fs/ext2/xattr.c:405-651 (ext2_xattr_set with value == NULL)

PROTOCOL remove_xattr {
    TIER: 3
    SIGNATURE: fn remove_xattr(&self, name: XattrName) -> Result<()>

    REQUIRE:
      self.xattr.is_some()
      caller has MAY_WRITE permission

    STEPS:
      // Phase 1: Xattr block mutation (xattr lock)
      1. VFS dispatch: self.check_permission(Permission::MAY_WRITE)?
      2. LOCK    xattr = self.xattr.as_ref()?.write()
      3. EFFECT  xattr.remove_xattr(name)?
      4. new_bid = xattr.bid()
      5. UNLOCK  xattr

      // Phase 2: Update inode file_acl + persist (inode lock)
      6. EFFECT  fs = self.fs_arc()?
      7. LOCK    inner = self.inner.write()
      8. MUTATE  inner.desc.file_acl = new_bid
      9. PERSIST inner.persist_inode_and_sync(&fs)?
      10. UNLOCK  inner
      11. RETURN  Ok(())

    LOCKS: XATTR_WRITE(self.xattr) → WRITE(self.inner)

    CRASH_ANALYSIS:
      Same as set_xattr — orphan xattr block possible on
      crash between phase 1 and phase 2.

    ROLLBACK:
      Phase 1 failure: xattr.remove_xattr handles internal rollback.
      No explicit rollback of xattr block on persist failure.

    ENSURE:
      S'.M.xattr[self] does not contain `name`
      S'.M.inode[self].desc.file_acl = updated xattr block bid
    ENSURE_ERR:
      EOPNOTSUPP if xattr not supported
      ENODATA    if attribute not found
}
