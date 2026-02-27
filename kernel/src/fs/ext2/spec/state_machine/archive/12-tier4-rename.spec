// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Tier 4: rename
//
// rename is the most complex VFS protocol. It has two variants:
//   - Same-directory rename (single lock)
//   - Cross-directory rename (two locks, ascending ino order)
// Both variants handle optional replacement of existing entries.
//
// Reference: 00-state-model.spec for state notation.

/// =============================================================================
/// PROTOCOL: rename (same-directory)
/// =============================================================================
/// Renames an entry within the same directory.
///
/// CODE: kernel/src/fs/ext2/inode.rs:3668-3748
/// LINUX_REF: fs/ext2/namei.c:318 (ext2_rename)

PROTOCOL rename_same_dir {
    TIER: 4
    SIGNATURE: fn rename(&self, old_name: &str, target: &Arc<dyn VfsInode>, new_name: &str) -> Result<()>
    DISPATCH: self.ino == target.ino

    REQUIRE:
      self.type_ = Dir
      old_name ∉ {"", ".", ".."}
      new_name ∉ {"", ".", ".."}
      same filesystem

    STEPS:
      // --- Validation ---
      1. GUARD  self.type_ == Dir ∧ target.type_ == Dir, else Err(ENOTDIR)
      2. GUARD  names valid, else Err(EISDIR)
      3. EFFECT fs = self.fs_arc()?
      4. GUARD  same fs, else Err(EINVAL)
      5. GUARD  ¬(self.ino == target.ino ∧ old_name == new_name), else Ok(())

      // --- Single lock (same directory) ---
      6. LOCK   inner = self.inner.write()
      7. old_ino = inner.find_entry(old_name)?
      8. EFFECT old_inode = fs.read_inode(old_ino)?
      9. old_is_dir = old_inode.type_ == Dir

      // --- Check for existing entry at new_name ---
      10. existing_ino = inner.find_entry(new_name).ok()

      IF existing_ino.is_some():
        // --- Replacement path ---
        11a. EFFECT existing = fs.read_inode(existing_ino)?
        12a. GUARD  type compatibility (dir↔dir, non-dir↔non-dir)
        13a. IF existing is dir: GUARD empty_dir()
        14a. EFFECT inner.set_link(new_name, old_ino, ft, true)?
        15a. LOCK   existing_inner = existing.inner.write()
        16a. MUTATE existing_inner.desc.ctime = now()
        17a. IF old_is_dir: existing_inner.links_count -= 1  // for ".."
        18a. MUTATE existing_inner.links_count -= 1
        19a. IF links_count == 0: set dtime, is_freed
        20a. PERSIST existing_inner.persist_inode_and_sync(&fs)?
        21a. UNLOCK existing_inner
      ELSE:
        // --- No replacement: add new entry ---
        11b. EFFECT inner.add_entry(new_name, old_ino, ft)?

      // --- Update moved inode ctime ---
      22. LOCK   old_inner = old_inode.inner.write()
      23. MUTATE old_inner.desc.ctime = now()
      24. PERSIST old_inner.persist_inode_and_sync(&fs)?
      25. UNLOCK old_inner

      // --- Delete old entry ---
      26. EFFECT inner.delete_entry(old_name)?

      // --- Directory link count adjustments ---
      27. IF old_is_dir:
            IF no replacement: inner.desc.links_count += 1
            inner.desc.links_count -= 1
            PERSIST inner.persist_inode_and_sync(&fs)?

      28. UNLOCK inner
      29. RETURN Ok(())

    LOCKS: WRITE(self.inner)
           WRITE(old_inode.inner) for ctime
           WRITE(existing.inner) if replacement

    CRASH_ANALYSIS:
      - Crash after set_link/add_entry, before delete old:
        Both old and new entries exist. Duplicate references.
        fsck: detects duplicate directory entries.
      - Crash after delete old, before link count adjust:
        Entry moved but link counts stale.
        fsck: repairs link counts.
      - Crash after all persists: consistent.

    ROLLBACK:
      No explicit rollback — rename is a sequence of independent
      mutations. Partial completion is repaired by fsck.

    ENSURE:
      S'.M.dir_entries[self] contains (new_name → old_ino)
      S'.M.dir_entries[self] does not contain old_name
      S'.M.inode[old_inode].desc.ctime = now()
      IF replacement:
        S'.M.inode[existing].desc.links_count decreased
      IF old_is_dir ∧ no replacement:
        net link count change on self = 0 (+1 then -1)
      IF old_is_dir ∧ replacement:
        S'.M.inode[self].desc.links_count = S.M - 1
    ENSURE_ERR:
      ENOTDIR    if self/target not directory
      EISDIR     if names are "."/".."; or non-dir replacing dir
      ENOTDIR    if dir replacing non-dir
      ENOTEMPTY  if replacing non-empty directory
      ENOENT     if old_name not found
}

/// =============================================================================
/// PROTOCOL: rename (cross-directory)
/// =============================================================================
/// Moves an entry from source directory to a different target directory.
/// Acquires write locks on both directories in ascending ino order.
///
/// CODE: kernel/src/fs/ext2/inode.rs:3750-3855
/// LINUX_REF: fs/ext2/namei.c:318 (ext2_rename)

PROTOCOL rename_cross_dir {
    TIER: 4
    SIGNATURE: fn rename(&self, old_name: &str, target: &Arc<dyn VfsInode>, new_name: &str) -> Result<()>
    DISPATCH: self.ino ≠ target.ino

    REQUIRE:
      self.type_ = Dir ∧ target.type_ = Dir
      old_name ∉ {"", ".", ".."}
      new_name ∉ {"", ".", ".."}
      same filesystem

    STEPS:
      // --- Validation ---
      1. GUARD  both dirs, names valid, same fs
      2. EFFECT fs = self.fs_arc()?

      // --- Acquire two write locks (ascending ino order) ---
      3. LOCK   (self_inner, target_inner) = write_lock_two_inodes(self, target)
         // If self.ino < target.ino: lock self first
         // If target.ino < self.ino: lock target first
         // CODE: kernel/src/fs/ext2/inode.rs (write_lock_two_inodes)

      // --- Lookup moved entry ---
      4. old_ino = self_inner.find_entry(old_name)?
      5. EFFECT old_inode = fs.read_inode(old_ino)?
      6. old_is_dir = old_inode.type_ == Dir

      // --- If moving dir, verify ".." points to self ---
      7. IF old_is_dir:
           LOCK old_inner = old_inode.inner.write()
           GUARD old_inner.find_entry("..") == self.ino, else Err(EIO)
           UNLOCK old_inner

      // --- Check existing entry at new_name in target ---
      8. existing_ino = target_inner.find_entry(new_name).ok()

      IF existing_ino.is_some():
        // --- Replacement path ---
        9a.  EFFECT existing = fs.read_inode(existing_ino)?
        10a. GUARD  type compatibility
        11a. IF existing is dir: GUARD empty_dir()
        12a. EFFECT target_inner.set_link(new_name, old_ino, ft, true)?
        13a. Update existing: ctime, links_count--, freed if 0
        14a. PERSIST existing
      ELSE:
        // --- No replacement: add entry + inc target links if dir ---
        9b.  EFFECT target_inner.add_entry(new_name, old_ino, ft)?
        10b. IF old_is_dir: target_inner.links_count += 1

      // --- Update moved inode ctime ---
      15. LOCK   old_inner = old_inode.inner.write()
      16. MUTATE old_inner.desc.ctime = now()
      17. PERSIST old_inner.persist_inode_and_sync(&fs)?
      18. UNLOCK old_inner

      // --- Delete old entry from source ---
      19. EFFECT self_inner.delete_entry(old_name)?

      // --- If dir: update ".." and adjust parent link counts ---
      20. IF old_is_dir:
            LOCK old_inner = old_inode.inner.write()
            old_inner.set_link("..", target.ino, Dir, false)?
            UNLOCK old_inner
            self_inner.links_count -= 1  // old parent loses subdir
            PERSIST self_inner
            PERSIST target_inner

      21. UNLOCK (self_inner, target_inner)
      22. RETURN Ok(())

    LOCKS:
      WRITE(min_ino.inner) → WRITE(max_ino.inner)  // deadlock prevention
      WRITE(old_inode.inner) for ".." check and ctime
      WRITE(existing.inner) if replacement

    CONCURRENCY:
      Lock ordering by ascending inode number prevents deadlock
      between concurrent renames involving overlapping directories.
      CODE: kernel/src/fs/ext2/inode.rs (write_lock_two_inodes)

    CRASH_ANALYSIS:
      - Crash after add/set_link in target, before delete in source:
        Entry exists in both directories. Duplicate references.
      - Crash after delete in source, before ".." update:
        Entry moved but ".." still points to old parent.
      - Crash after ".." update, before link count adjustments:
        ".." correct but parent link counts stale.
      - Crash after all persists: fully consistent.

    ROLLBACK:
      No explicit rollback — rename is a multi-step sequence.
      Partial completion requires fsck repair.

    ENSURE:
      S'.M.dir_entries[target] contains (new_name → old_ino)
      S'.M.dir_entries[self] does not contain old_name
      IF old_is_dir:
        S'.M.dir_entries[old_inode][".." ] = target.ino
        S'.M.inode[self].desc.links_count = S.M - 1
        IF no replacement:
          S'.M.inode[target].desc.links_count = S.M + 1
      IF replacement:
        S'.M.inode[existing].desc.links_count decreased
    ENSURE_ERR:
      ENOTDIR    if self/target not directory
      EISDIR     if names are "."/".."; or type mismatch
      ENOTEMPTY  if replacing non-empty directory
      ENOENT     if old_name not found
      EIO        if ".." doesn't point to expected parent
}
