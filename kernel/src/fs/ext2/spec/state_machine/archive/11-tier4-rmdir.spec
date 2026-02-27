// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Tier 4: rmdir
//
// rmdir is a two-inode protocol that removes an empty directory,
// updating both parent and child link counts.
//
// Reference: 00-state-model.spec for state notation.

/// =============================================================================
/// PROTOCOL: rmdir
/// =============================================================================
/// Removes an empty subdirectory from this directory.
///
/// CODE: kernel/src/fs/ext2/inode.rs:999-1048
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:172-174
/// LINUX_REF: fs/ext2/namei.c:290 (ext2_rmdir)

PROTOCOL rmdir {
    TIER: 4
    SIGNATURE: fn rmdir(&self, name: &str) -> Result<()>

    REQUIRE:
      self.type_ = Dir
      name ∉ {"", ".", ".."}

    STEPS:
      // --- Validation ---
      1. GUARD  self.type_ == Dir, else Err(ENOTDIR)
      2. GUARD  name valid and not "."/".."), else Err(EINVAL)
      3. EFFECT fs = self.fs_arc()?

      // --- Lookup child under upread ---
      4. LOCK   parent_upread = self.inner.upread()
      5. child_ino = parent_upread.find_entry(name)?
      6. EFFECT child = fs.read_inode(child_ino)?

      // --- Validate child is empty dir ---
      7. LOCK   child_read = child.inner.read()
      8. GUARD  child_read.desc.type_ == Dir, else Err(ENOTDIR)
      9. GUARD  child_read.empty_dir(), else Err(ENOTEMPTY)
      10. UNLOCK child_read

      // --- Delete parent directory entry ---
      11. target = parent_upread.find_entry_target(name)?
      12. parent_upread.delete_entry_in_cache(&target)?
      13. LOCK   parent_write = parent_upread.upgrade()
      14. PERSIST parent_write.commit_dir_metadata(&fs)?

      // --- Update child: mark freed ---
      15. LOCK   child_write = child.inner.write()
      16. MUTATE child_write.desc.size = 0
      17. MUTATE child_write.desc.links_count -= 2
              // -1 for parent's entry, -1 for child's "."
      18. MUTATE child_write.desc.dtime = now()
      19. MUTATE child_write.is_freed = true
      20. PERSIST child_write.persist_inode_and_sync(&fs)?
      21. UNLOCK child_write

      // --- Update parent link count ---
      22. MUTATE parent_write.desc.links_count -= 1
              // Remove child's ".." reference
      23. PERSIST parent_write.commit_dir_metadata(&fs)?
      24. UNLOCK parent_write
      25. RETURN Ok(())

    LOCKS:
      UPREAD(self.inner) → WRITE(self.inner)
      READ(child.inner) for empty check
      WRITE(child.inner) for link count update

    CONCURRENCY:
      Parent lock held throughout (upread then write).
      Child read lock acquired/released for validation.
      Child write lock acquired after parent entry deletion.
      No deadlock risk: parent lock acquired first, child second.

    CRASH_ANALYSIS:
      - Crash after step 14, before step 20:
        Parent entry deleted but child not marked freed.
        Orphan directory with links_count=2.
        fsck: detects unreferenced directory.
      - Crash after step 20, before step 23:
        Child freed but parent links_count not decremented.
        fsck: detects parent link count mismatch.
      - Crash after step 23: fully consistent.

    ROLLBACK:
      No explicit rollback in rmdir — operations are sequential
      and each persist is independent. Partial completion leaves
      state that fsck can repair.

    ENSURE:
      S'.M.dir_entries[self] does not contain name
      S'.M.inode[child].desc.links_count = 0
      S'.M.inode[child].desc.size = 0
      S'.M.inode[child].is_freed = true
      S'.M.inode[child].desc.dtime > 0
      S'.M.inode[self].desc.links_count = S.M - 1
    ENSURE_ERR:
      ENOTDIR    if self or child not directory
      EINVAL     if name is "." or ".."
      ENOENT     if name not found
      ENOTEMPTY  if child directory not empty
}
