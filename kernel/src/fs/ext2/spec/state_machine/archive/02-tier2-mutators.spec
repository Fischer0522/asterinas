// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Tier 2: Single-Inode Mutators
//
// Tier 2 methods acquire a single write lock, mutate one field, update ctime,
// and persist. They follow a uniform pattern: LOCK → MUTATE → PERSIST → UNLOCK.
//
// Reference: 00-state-model.spec for state notation.

/// =============================================================================
/// PROTOCOL: set_mode()
/// =============================================================================
/// Sets the permission mode bits.
///
/// CODE: kernel/src/fs/ext2/inode.rs:280-286
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:75-77
/// LINUX_REF: fs/ext2/inode.c:1589 (__ext2_write_inode)

PROTOCOL set_mode {
    TIER: 2
    SIGNATURE: fn set_mode(&self, mode: InodeMode) -> Result<()>

    STEPS:
      1. EFFECT  fs = self.fs_arc()?
      2. LOCK    inner = self.inner.write()
      3. MUTATE  inner.desc.perm = FilePerm::from_bits_truncate(mode.bits())
      4. MUTATE  inner.desc.ctime = now()
      5. PERSIST inner.persist_inode_and_sync(&fs)?
      6. UNLOCK  inner
      7. RETURN  Ok(())

    LOCKS: WRITE(self.inner)

    CRASH_ANALYSIS:
      - Crash before step 5: no on-disk change, M reverts on reboot
      - Crash during step 5: partial inode table write possible;
        fsck repairs from on-disk state
      - Crash after step 5: fully committed

    ROLLBACK: None needed — persist_inode_and_sync is the only fallible
              step after mutation; on error, in-memory state is stale but
              consistent (dirty flag remains set).

    ENSURE:
      S'.M.inode[self].desc.perm = mode
      S'.M.inode[self].desc.ctime ≥ S.M.inode[self].desc.ctime
    ENSURE_ERR:
      EIO if fs is dropped
}

/// =============================================================================
/// PROTOCOL: set_owner()
/// =============================================================================
/// Sets the owner UID.
///
/// CODE: kernel/src/fs/ext2/inode.rs:292-298
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:83-85
/// LINUX_REF: fs/ext2/inode.c:1589

PROTOCOL set_owner {
    TIER: 2
    SIGNATURE: fn set_owner(&self, uid: Uid) -> Result<()>

    STEPS:
      1. EFFECT  fs = self.fs_arc()?
      2. LOCK    inner = self.inner.write()
      3. MUTATE  inner.desc.uid = uid.into()
      4. MUTATE  inner.desc.ctime = now()
      5. PERSIST inner.persist_inode_and_sync(&fs)?
      6. UNLOCK  inner
      7. RETURN  Ok(())

    LOCKS: WRITE(self.inner)

    CRASH_ANALYSIS:
      Same as set_mode — single atomic persist step.

    ROLLBACK: None needed.

    ENSURE:
      S'.M.inode[self].desc.uid = uid
      S'.M.inode[self].desc.ctime ≥ S.M.inode[self].desc.ctime
    ENSURE_ERR:
      EIO if fs is dropped
}

/// =============================================================================
/// PROTOCOL: set_group()
/// =============================================================================
/// Sets the group GID.
///
/// CODE: kernel/src/fs/ext2/inode.rs:304-310
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:91-93
/// LINUX_REF: fs/ext2/inode.c:1589

PROTOCOL set_group {
    TIER: 2
    SIGNATURE: fn set_group(&self, gid: Gid) -> Result<()>

    STEPS:
      1. EFFECT  fs = self.fs_arc()?
      2. LOCK    inner = self.inner.write()
      3. MUTATE  inner.desc.gid = gid.into()
      4. MUTATE  inner.desc.ctime = now()
      5. PERSIST inner.persist_inode_and_sync(&fs)?
      6. UNLOCK  inner
      7. RETURN  Ok(())

    LOCKS: WRITE(self.inner)
    CRASH_ANALYSIS: Same as set_mode.
    ROLLBACK: None needed.

    ENSURE:
      S'.M.inode[self].desc.gid = gid
      S'.M.inode[self].desc.ctime ≥ S.M.inode[self].desc.ctime
    ENSURE_ERR:
      EIO if fs is dropped
}

/// =============================================================================
/// PROTOCOL: set_atime()
/// =============================================================================
/// Sets the access time. No persist — timestamp-only mutation.
///
/// CODE: kernel/src/fs/ext2/inode.rs:316-318
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:99-101

PROTOCOL set_atime {
    TIER: 2
    SIGNATURE: fn set_atime(&self, time: Duration)

    STEPS:
      1. LOCK   inner = self.inner.write()
      2. MUTATE inner.desc.atime = time
      3. UNLOCK inner

    LOCKS: WRITE(self.inner)
    CRASH_ANALYSIS: No persist — atime lost on crash (acceptable, matches Linux lazytime).
    ROLLBACK: N/A (infallible)

    ENSURE:
      S'.M.inode[self].desc.atime = time
      // No on-disk change until next sync
}

/// =============================================================================
/// PROTOCOL: set_mtime()
/// =============================================================================
/// Sets the modification time. No persist.
///
/// CODE: kernel/src/fs/ext2/inode.rs:324-326
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:107-109

PROTOCOL set_mtime {
    TIER: 2
    SIGNATURE: fn set_mtime(&self, time: Duration)

    STEPS:
      1. LOCK   inner = self.inner.write()
      2. MUTATE inner.desc.mtime = time
      3. UNLOCK inner

    LOCKS: WRITE(self.inner)
    CRASH_ANALYSIS: No persist — mtime lost on crash.
    ROLLBACK: N/A (infallible)

    ENSURE:
      S'.M.inode[self].desc.mtime = time
}

/// =============================================================================
/// PROTOCOL: set_ctime()
/// =============================================================================
/// Sets the change time. No persist.
///
/// CODE: kernel/src/fs/ext2/inode.rs:332-334
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:115-117

PROTOCOL set_ctime {
    TIER: 2
    SIGNATURE: fn set_ctime(&self, time: Duration)

    STEPS:
      1. LOCK   inner = self.inner.write()
      2. MUTATE inner.desc.ctime = time
      3. UNLOCK inner

    LOCKS: WRITE(self.inner)
    CRASH_ANALYSIS: No persist — ctime lost on crash.
    ROLLBACK: N/A (infallible)

    ENSURE:
      S'.M.inode[self].desc.ctime = time
}

/// =============================================================================
/// PROTOCOL: resize()
/// =============================================================================
/// Resizes the file (truncate or extend). Multi-phase with upread/upgrade.
///
/// CODE: kernel/src/fs/ext2/inode.rs:137-238
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:55-57
/// LINUX_REF: fs/ext2/inode.c:1275 (ext2_setsize)

PROTOCOL resize {
    TIER: 2
    SIGNATURE: fn resize(&self, new_size: usize) -> Result<()>

    REQUIRE:
      self.type_ ∈ {File, Dir, SymLink}
      ¬is_fast_symlink(self) ∨ self.desc.size = 0
      ¬self.desc.flags.intersects(APPEND_ONLY | IMMUTABLE)

    STEPS:
      // --- Validation phase (read lock) ---
      1. EFFECT  fs = self.fs_arc()?
      2. LOCK    inner_read = self.inner.read()
      3. GUARD   type ∈ {File, Dir, SymLink}, else Err(EINVAL)
      4. GUARD   ¬is_fast_symlink with size>0, else Err(EINVAL)
      5. GUARD   ¬(APPEND_ONLY | IMMUTABLE), else Err(EPERM)
      6. old_size = inner_read.desc.size
      7. GUARD   new_size ≠ old_size, else RETURN Ok(())
      8. UNLOCK  inner_read

      // --- Shrink path (upread → upgrade) ---
      IF new_size < old_size:
        9.  LOCK    upread = self.inner.upread()
        10. IF new_size % block_size ≠ 0:
              upread.page_cache.fill_zeros(new_size..align_up(new_size))
        11. LOCK    inner = upread.upgrade()
        12. Recheck: if new_size < inner.desc.size:
              inner.page_cache.discard_range(new_aligned..old_aligned)
              inner.page_cache.resize(new_aligned)?
              inner.desc.size = new_size
              inner.truncate_blocks(new_size)?
        13. MUTATE  inner.desc.mtime = now(), inner.desc.ctime = now()
        14. PERSIST inner.persist_inode_and_sync(&fs)?
        15. UNLOCK  inner

      // --- Grow path (write lock) ---
      IF new_size > old_size:
        9.  LOCK    inner = self.inner.write()
        10. inner.page_cache.resize(align_up(new_size))?
        11. inner.desc.size = new_size
        12. MUTATE  inner.desc.mtime = now(), inner.desc.ctime = now()
        13. PERSIST inner.persist_inode_and_sync(&fs)?
        14. UNLOCK  inner

    LOCKS:
      Shrink: UPREAD(self.inner) → WRITE(self.inner)
      Grow:   WRITE(self.inner)

    CRASH_ANALYSIS:
      - Crash before persist: size reverts to old on reboot
      - Crash during truncate_blocks: orphan blocks; fsck reclaims
      - Crash after persist: new size committed

    ROLLBACK:
      Shrink path has no explicit rollback — truncate_blocks failures
      leave blocks allocated but unreachable (fsck cleans up).
      Grow path: page_cache.resize failure is the only fallible step
      before size mutation.

    ENSURE:
      S'.M.inode[self].desc.size = new_size
      S'.M.inode[self].desc.mtime ≥ S.M.inode[self].desc.mtime
      S'.M.inode[self].desc.ctime ≥ S.M.inode[self].desc.ctime
      IF new_size < old_size:
        blocks freed for range [new_size_aligned, old_size_aligned)
    ENSURE_ERR:
      EINVAL if type not supported or fast symlink
      EPERM  if APPEND_ONLY or IMMUTABLE
      EIO    if fs dropped or block_size=0
}
