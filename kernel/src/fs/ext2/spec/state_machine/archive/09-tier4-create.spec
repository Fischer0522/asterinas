// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Tier 4: create
//
// create dispatches to mkdir for directories or uses the
// create_inode + add_entry pattern for non-directory types.
//
// Reference: 00-state-model.spec for state notation.

/// =============================================================================
/// PROTOCOL: create (non-directory)
/// =============================================================================
/// Creates a new non-directory inode and links it into this directory.
///
/// CODE: kernel/src/fs/ext2/inode.rs:3452-3504
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:131-133
/// LINUX_REF: fs/ext2/namei.c:107 (ext2_create), fs/ext2/namei.c:136 (ext2_mknod)

PROTOCOL create_non_dir {
    TIER: 4
    SIGNATURE: fn create(&self, name: &str, type_: InodeType, mode: InodeMode) -> Result<Arc<dyn VfsInode>>
    DISPATCH: type_ ≠ Dir

    REQUIRE:
      self.type_ = Dir
      name ∉ {"", ".", ".."}
      name.len() ≤ 255
      type_ ∈ {File, SymLink, CharDevice, BlockDevice, NamedPipe}

    STEPS:
      // --- Validation ---
      1. GUARD  self.type_ == Dir, else Err(ENOTDIR)
      2. GUARD  name valid, else Err(EINVAL)
      3. GUARD  type_ valid, else Err(EINVAL)

      // --- Inode allocation ---
      4. EFFECT fs = self.fs_arc()?
      5. EFFECT child = fs.create_inode(self.ino, type_, perm)?
         // Allocates inode number from bitmap
         // Initializes RawInode with uid/gid/timestamps
         // Writes inode desc to inode table page cache
         // CODE: kernel/src/fs/ext2/fs.rs:587-664
      6. child_ino = child.ino()
      7. dir_ft = inode_type_to_dir_file_type(type_)

      // --- Directory entry insertion ---
      8. EFFECT self.add_entry(name, child_ino, dir_ft)?
         // Uses upread/upgrade pattern internally
         // CODE: kernel/src/fs/ext2/inode.rs:827-866
      // On add_entry error: free_inode(child_ino, false)

      // --- Cache insertion ---
      9. EFFECT fs.insert_inode_cache(child.clone())
      10. RETURN Ok(child)

    LOCKS:
      create_inode: SB_READ → SB_WRITE (for counter updates)
      add_entry: UPREAD(self.inner) → WRITE(self.inner)

    CRASH_ANALYSIS:
      - Crash after step 5, before step 8:
        Inode allocated in bitmap but no directory entry.
        Orphan inode with links_count=1; fsck detects via bitmap scan.
      - Crash after step 8:
        Directory entry written, inode allocated. Consistent.
      - Crash before inode table write in create_inode:
        Bitmap set but inode table not written; fsck repairs.

    ROLLBACK:
      add_entry failure (step 8):
        fs.free_inode(child_ino, false)
        // Clears inode bitmap bit, updates group/sb counters
        CODE: kernel/src/fs/ext2/inode.rs:3493-3497

    ENSURE:
      ∃ new inode child:
        S'.M.inode[child].ino = child_ino (freshly allocated)
        S'.M.inode[child].type_ = type_
        S'.M.inode[child].desc.links_count = 1
        S'.M.inode[child].desc.perm = mode
      S'.M.dir_entries[self] contains (name → child_ino)
      S'.M.sb.free_inodes_count = S.M.sb.free_inodes_count - 1
    ENSURE_ERR:
      ENOTDIR if self not a directory
      EINVAL  if name or type invalid
      ENOSPC  if no free inodes
      EEXIST  if name already exists (from add_entry)
      S'.M observable state = S.M (via rollback)
}

/// =============================================================================
/// PROTOCOL: create (mkdir dispatch)
/// =============================================================================
/// Creates a new directory inode with "." and ".." entries.
///
/// CODE: kernel/src/fs/ext2/inode.rs:1050-1129
/// LINUX_REF: fs/ext2/namei.c:228 (ext2_mkdir)

PROTOCOL create_mkdir {
    TIER: 4
    SIGNATURE: fn create(&self, name: &str, type_: InodeType, mode: InodeMode) -> Result<Arc<dyn VfsInode>>
    DISPATCH: type_ = Dir

    REQUIRE:
      self.type_ = Dir
      name ∉ {"", ".", ".."}

    STEPS:
      // --- Validation (same as non-dir) ---
      1. GUARD  self.type_ == Dir, else Err(ENOTDIR)
      2. GUARD  name valid, else Err(EINVAL)

      // --- Parent link count increment (upread → upgrade) ---
      3. EFFECT  fs = self.fs_arc()?
      4. LOCK    parent_upread = self.inner.upread()
      5. Scan for directory slot (may upgrade for growth, then downgrade)
      6. LOCK    parent_write = parent_upread.upgrade()
      7. MUTATE  parent_write.desc.links_count += 1  // for child's ".."
      8. parent_upread = parent_write.downgrade()

      // --- Child inode allocation ---
      9. EFFECT  child = fs.create_inode(self.ino, Dir, perm)?
      // On error: rollback parent links_count -= 1
      10. child_ino = child.ino()

      // --- Initialize child directory (make_empty) ---
      11. EFFECT child.make_empty(self.ino)?
          // Allocates first data block
          // Writes "." and ".." entries
          // CODE: kernel/src/fs/ext2/inode.rs:911-993
      // On error: free_inode(child_ino, true), rollback parent links

      // --- Add entry in parent ---
      12. EFFECT parent_upread.write_dir_entry(&slot, name, child_ino, Dir)?
      // On error: cleanup child data blocks, free_inode, rollback parent

      // --- Commit parent metadata ---
      13. LOCK   parent_write = parent_upread.upgrade()
      14. PERSIST parent_write.commit_dir_metadata(&fs)?
      // On error: delete_entry(name), cleanup child, rollback parent

      // --- Cache insertion ---
      15. EFFECT fs.insert_inode_cache(child.clone())
      16. RETURN Ok(child)

    LOCKS:
      UPREAD(self.inner) → WRITE(self.inner) (multiple transitions)
      WRITE(child.inner) during make_empty

    CRASH_ANALYSIS:
      - Crash after step 9, before step 11:
        Child inode allocated but no data block. Orphan inode.
      - Crash after step 11, before step 12:
        Child has "."/"..". Parent has no entry. Orphan directory.
      - Crash after step 12, before step 14:
        Entry in parent page cache but metadata not persisted.
      - Crash after step 14: consistent.

    ROLLBACK:
      Step 9 error:  parent.links_count -= 1
      Step 11 error: free_inode(child_ino, true), parent.links_count -= 1
      Step 12 error: release child data blocks, free_inode, parent.links_count -= 1
      Step 14 error: delete_entry(name), release child data blocks,
                     free_inode, parent.links_count -= 1
      CODE: kernel/src/fs/ext2/inode.rs:1086-1125

    ENSURE:
      ∃ new dir inode child:
        S'.M.inode[child].type_ = Dir
        S'.M.inode[child].desc.links_count = 2  ("." + parent entry)
        S'.M.dir_entries[child] = {"." → child_ino, ".." → self.ino}
      S'.M.dir_entries[self] contains (name → child_ino)
      S'.M.inode[self].desc.links_count = S.M + 1  (for child's "..")
    ENSURE_ERR:
      S'.M observable state = S.M (via multi-step rollback)
}
