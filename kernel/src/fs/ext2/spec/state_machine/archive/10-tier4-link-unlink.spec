// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Tier 4: link, unlink
//
// link and unlink are two-inode protocols that modify both the parent
// directory and the target inode's link count.
//
// Reference: 00-state-model.spec for state notation.

/// =============================================================================
/// PROTOCOL: link
/// =============================================================================
/// Creates a hard link in this directory to an existing non-directory inode.
///
/// CODE: kernel/src/fs/ext2/inode.rs:3509-3558
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:161-166
/// LINUX_REF: fs/ext2/namei.c:204 (ext2_link)

PROTOCOL link {
    TIER: 4
    SIGNATURE: fn link(&self, old: &Arc<dyn VfsInode>, name: &str) -> Result<()>

    REQUIRE:
      self.type_ = Dir
      old.type_ ≠ Dir
      old.desc.links_count < MAX_LINK_COUNT (32000)
      name ∉ {"", ".", ".."}
      same filesystem (Arc::ptr_eq)

    STEPS:
      // --- Validation ---
      1. GUARD  self.type_ == Dir, else Err(ENOTDIR)
      2. GUARD  old.type_ ≠ Dir, else Err(EPERM)
      3. GUARD  old.links_count < MAX_LINK_COUNT, else Err(EOVERFLOW)
      4. GUARD  name valid, else Err(EINVAL)
      5. EFFECT fs = self.fs_arc()?
      6. GUARD  Arc::ptr_eq(&fs, &old.fs_arc()?), else Err(EINVAL)

      // --- Increment target link count ---
      7. LOCK   old_inner = old.inner.write()
      8. MUTATE old_inner.desc.ctime = now()
      9. MUTATE old_inner.desc.links_count += 1
      10. UNLOCK old_inner

      // --- Add directory entry ---
      11. EFFECT self.add_entry(name, old.ino, dir_ft)?
      // On error: old.inner.write().links_count -= 1

      // --- Persist target inode ---
      12. LOCK   old_inner = old.inner.write()
      13. PERSIST old_inner.persist_inode_and_sync(&fs)?
      14. UNLOCK old_inner
      15. RETURN Ok(())

    LOCKS:
      WRITE(old.inner) for link count
      UPREAD(self.inner) → WRITE(self.inner) for add_entry
      WRITE(old.inner) for persist

    CRASH_ANALYSIS:
      - Crash after step 9, before step 11:
        Link count incremented in memory but no dir entry.
        On reboot: old link count restored from disk. No leak.
      - Crash after step 11, before step 13:
        Dir entry exists but link count not persisted.
        On reboot: dir entry points to inode with old link count.
        fsck detects link count mismatch.
      - Crash after step 13: consistent.

    ROLLBACK:
      add_entry failure (step 11):
        old.inner.write().links_count -= 1
        CODE: kernel/src/fs/ext2/inode.rs:3551-3553

    ENSURE:
      S'.M.inode[old].desc.links_count = S.M + 1
      S'.M.inode[old].desc.ctime ≥ S.M.inode[old].desc.ctime
      S'.M.dir_entries[self] contains (name → old.ino)
    ENSURE_ERR:
      ENOTDIR   if self not directory
      EPERM     if old is directory
      EOVERFLOW if link count at max
      EINVAL    if name invalid or cross-fs
      S'.M observable state = S.M (via rollback)
}

/// =============================================================================
/// PROTOCOL: unlink
/// =============================================================================
/// Removes a non-directory entry from this directory.
///
/// CODE: kernel/src/fs/ext2/inode.rs:3563-3605
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:168-170
/// LINUX_REF: fs/ext2/namei.c:273 (ext2_unlink)

PROTOCOL unlink {
    TIER: 4
    SIGNATURE: fn unlink(&self, name: &str) -> Result<()>

    REQUIRE:
      self.type_ = Dir
      name ∉ {"", ".", ".."}

    STEPS:
      // --- Validation ---
      1. GUARD  self.type_ == Dir, else Err(ENOTDIR)
      2. GUARD  name valid, else Err(EINVAL)
      3. EFFECT fs = self.fs_arc()?

      // --- Lookup child ---
      4. LOCK   self_read = self.inner.read()
      5. child_ino = self_read.find_entry(name)?
      6. UNLOCK self_read
      7. EFFECT child = fs.read_inode(child_ino)?

      // --- Type check ---
      8. GUARD  child.type_ ≠ Dir, else Err(EISDIR)

      // --- Delete directory entry ---
      9. EFFECT self.delete_entry(name)?
         // Uses upread/upgrade pattern
         // CODE: kernel/src/fs/ext2/inode.rs:883-906

      // --- Decrement child link count ---
      10. LOCK   child_inner = child.inner.write()
      11. MUTATE child_inner.desc.ctime = now()
      12. MUTATE child_inner.desc.links_count -= 1  (saturating)

      // --- Mark freed if last link ---
      13. IF child_inner.desc.links_count == 0:
            MUTATE child_inner.desc.dtime = now()
            MUTATE child_inner.is_freed = true

      // --- Persist child ---
      14. PERSIST child_inner.persist_inode_and_sync(&fs)?
      15. UNLOCK  child_inner
      16. RETURN  Ok(())

    LOCKS:
      READ(self.inner) for lookup
      UPREAD(self.inner) → WRITE(self.inner) for delete_entry
      WRITE(child.inner) for link count update

    CRASH_ANALYSIS:
      - Crash after step 9, before step 14:
        Dir entry deleted but child link count not decremented.
        On reboot: child has stale link count; fsck repairs.
      - Crash after step 14: consistent.
        If links_count=0: inode marked for reclamation.

    ROLLBACK:
      No explicit rollback — delete_entry and link count
      decrement are sequential. If persist fails, in-memory
      state is stale but child remains in inode cache.

    ENSURE:
      S'.M.dir_entries[self] does not contain name
      S'.M.inode[child].desc.links_count = S.M - 1
      S'.M.inode[child].desc.ctime = now()
      IF S'.M.inode[child].desc.links_count = 0:
        S'.M.inode[child].is_freed = true
        S'.M.inode[child].desc.dtime > 0
    ENSURE_ERR:
      ENOTDIR if self not directory
      EINVAL  if name invalid
      ENOENT  if name not found
      EISDIR  if child is directory (use rmdir)
}
