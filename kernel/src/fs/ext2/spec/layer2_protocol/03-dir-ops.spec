// SPDX-License-Identifier: MPL-2.0
//
// Layer 2: Implementation Protocol -- Directory Operations
//
// 10 PROTOCOL specs mapping Layer 1 directory HOARE specs to Asterinas
// Ext2 implementation details: locks, page cache, persist helpers, rollback.
//
// Rename is split into two variants:
//   - rename_same_dir  (DISPATCH: src_dir.ino = dst_dir.ino)
//   - rename_cross_dir (DISPATCH: src_dir.ino != dst_dir.ino)
//
// Reference: layer1_hoare/03-dir-ops.spec for abstract contracts.
// Reference: layer2_protocol/00-impl-state.spec for concrete state model.
//
// Notation:
//   C        -- ConcreteState (pre)
//   C'       -- ConcreteState (post)
//   WRITE(x) -- exclusive lock on x
//   UPREAD(x)-- upgradable read lock on x
//   READ(x)  -- shared read lock on x

/// =============================================================================
/// SECTION 1: PURE READ PROTOCOLS (lookup, readdir_at)
/// =============================================================================

/// ---------------------------------------------------------------------------
/// PROTOCOL lookup
/// ---------------------------------------------------------------------------
/// Resolves a directory entry name to a child inode reference.
///
/// CODE: kernel/src/fs/ext2/inode.rs:815-822

PROTOCOL lookup {
    SATISFIES: layer1::lookup
    DISPATCH: always
    CODE: kernel/src/fs/ext2/inode.rs:815-822

    LOCKS:
        READ(self.inner)                    // find_entry scans page cache

    STEPS:
        1. GUARD  self.type_ == Dir, else Err(ENOTDIR)
        2. LOCK   inner = self.inner.read()
        3. EFFECT ino = inner.find_entry(name)?
        4. UNLOCK inner
        5. EFFECT child = fs.read_inode(ino)?
        6. RETURN Ok(child)

    CRASH_WINDOWS:
        -- None. Pure read; no mutation of on-disk state.

    ROLLBACK:
        -- None needed. No state modified.

    SATISFIES_PROOF {
        PRE:       self.type_ == Dir ==> HOARE.PRE (dir_ino is Dir)
        POST.ret:  step 3 find_entry returns ino from dirs[dir_ino][name]
                   ==> HOARE.POST (child_ino, _) = FS.dirs[dir_ino][name]
        FRAME:     READ lock only; no mutation ==> FS' = FS
        POST_ERR:  find_entry Err(ENOENT); type guard Err(ENOTDIR)
                   ==> HOARE.POST_ERR
        CRASH:     no persist ==> FS_recovered = FS.durable
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL readdir_at
/// ---------------------------------------------------------------------------
/// Iterates directory entries from a byte offset via DirentVisitor.
///
/// CODE: kernel/src/fs/ext2/inode.rs:868-878

PROTOCOL readdir_at {
    SATISFIES: layer1::readdir_at
    DISPATCH: always
    CODE: kernel/src/fs/ext2/inode.rs:868-878

    LOCKS:
        READ(self.inner)                    // scan page cache pages

    STEPS:
        1. GUARD  self.type_ == Dir, else Err(ENOTDIR)
        2. LOCK   inner = self.inner.read()
        3. EFFECT n = inner.readdir_at(offset, visitor)?
        4. UNLOCK inner
        5. RETURN Ok(n)

    CRASH_WINDOWS:
        -- None. Pure read; no mutation of on-disk state.

    ROLLBACK:
        -- None needed. No state modified.

    SATISFIES_PROOF {
        PRE:       self.type_ == Dir ==> HOARE.PRE
        POST.ret:  step 3 returns count of visited entries ==> HOARE.POST
        FRAME:     READ lock only; no mutation ==> FS' = FS
        POST_ERR:  type guard Err(ENOTDIR) ==> HOARE.POST_ERR
        CRASH:     no persist ==> FS_recovered = FS.durable
    }
}

/// =============================================================================
/// SECTION 2: CREATION PROTOCOLS (create, mkdir)
/// =============================================================================

/// ---------------------------------------------------------------------------
/// PROTOCOL create_non_dir
/// ---------------------------------------------------------------------------
/// Creates a new non-directory inode and links it into the parent directory.
///
/// CODE: kernel/src/fs/ext2/inode.rs:3486-3504

PROTOCOL create_non_dir {
    SATISFIES: layer1::create
    DISPATCH: type_ != Dir
    CODE: kernel/src/fs/ext2/inode.rs:3486-3504

    LOCKS:
        // create_inode: SB_READ -> SB_WRITE (counter updates)
        // add_entry: UPREAD(self.inner) -> WRITE(self.inner)
        UPREAD(self.inner) -> WRITE(self.inner)

    STEPS:
        // --- Validation ---
        1. GUARD  self.type_ == Dir, else Err(ENOTDIR)
        2. GUARD  name valid and not "."/".."), else Err(EINVAL)
        3. GUARD  type_ valid, else Err(EINVAL)

        // --- Inode allocation ---
        4. EFFECT fs = self.fs_arc()?
        5. EFFECT child = fs.create_inode(self.ino, type_, perm)?
           // Allocates inode number from bitmap
           // Initializes InodeDesc with uid/gid/timestamps
           // Writes to inode table page cache
        6. child_ino = child.ino()
        7. dir_ft = inode_type_to_dir_file_type(type_)

        // --- Directory entry insertion ---
        8. EFFECT self.add_entry(name, child_ino, dir_ft)?
           // Internally: UPREAD(self.inner) -> scan_dir_for_slot
           //   -> may upgrade for grow_dir_block -> downgrade
           //   -> write_dir_entry -> upgrade -> commit_dir_metadata
           // On error: goto ROLLBACK

        // --- Cache insertion ---
        9. EFFECT fs.insert_inode_cache(child.clone())
        10. RETURN Ok(child)

    CRASH_WINDOWS:
        W1: After step 5 (create_inode persisted), before step 8.
            Inode allocated in bitmap, inode table written.
            No directory entry. Orphan inode with links_count=1.
            fsck: bitmap scan detects unreferenced inode, reclaims.

        W2: During step 8 (add_entry partial persist).
            Directory page cache written but commit_dir_metadata
            may not have completed. Entry may or may not be visible.

        W3: After step 8 completes. Fully consistent.

    ROLLBACK:
        add_entry failure (step 8):
            fs.free_inode(child_ino, false)
            // Clears inode bitmap bit, updates group/sb counters.
            CODE: kernel/src/fs/ext2/inode.rs:3493-3497

    SATISFIES_PROOF {
        PRE:       type guards + name validation ==> HOARE.PRE
        POST.new_ino:  step 5 allocates fresh ino ==> HOARE.POST (new_ino fresh)
        POST.entry:    step 8 add_entry ==> dirs[dir_ino][name] = (new_ino, type_)
        POST.inode:    create_inode sets links_count=1, type_, mode
                       ==> HOARE.POST inode fields
        POST.sb:       create_inode decrements free_inodes
                       ==> HOARE.POST sb.free_inodes
        FRAME:     only self.inner and new inode modified ==> HOARE.FRAME
        POST_ERR:  rollback free_inode restores bitmap ==> FS' = FS
        CRASH:     W1 = orphan inode state in HOARE.CRASH set
                   W3 = FS' in HOARE.CRASH set
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL create_mkdir
/// ---------------------------------------------------------------------------
/// Creates a new directory inode with "." and ".." entries, links it into
/// the parent. Uses phased locking with multi-step rollback.
///
/// CODE: kernel/src/fs/ext2/inode.rs:1050-1129

PROTOCOL create_mkdir {
    SATISFIES: layer1::mkdir
    DISPATCH: type_ = Dir (create dispatches to mkdir)
    CODE: kernel/src/fs/ext2/inode.rs:1050-1129

    LOCKS:
        UPREAD(self.inner) -> WRITE(self.inner) (multiple transitions)
        WRITE(child.inner) during make_empty

    STEPS:
        // --- Validation ---
        1. GUARD  self.type_ == Dir, else Err(ENOTDIR)
        2. GUARD  name valid and not "."/".."), else Err(EINVAL)
        3. EFFECT fs = self.fs_arc()?

        // --- Scan for directory slot (upread phase) ---
        4. LOCK   parent_upread = self.inner.upread()
        5. slot = parent_upread.scan_dir_for_slot(name, &fs)?
           // May upgrade for grow_dir_block, then downgrade.

        // --- Increment parent link count (for child's "..") ---
        6. LOCK   parent_write = parent_upread.upgrade()
        7. MUTATE parent_write.desc.links_count += 1
        8. parent_upread = parent_write.downgrade()

        // --- Child inode allocation ---
        9. EFFECT child = fs.create_inode(self.ino, Dir, perm)?
           // On error: ROLLBACK_A (parent links_count -= 1)
        10. child_ino = child.ino()

        // --- Initialize child directory (make_empty) ---
        11. EFFECT child.make_empty(self.ino)?
            // WRITE(child.inner): allocate first data block,
            // write "." and ".." entries, persist child inode.
            // On error: ROLLBACK_B

        // --- Write parent directory entry ---
        12. EFFECT parent_upread.write_dir_entry(&slot, name, child_ino, Dir)?
            // On error: ROLLBACK_C

        // --- Commit parent metadata ---
        13. LOCK   parent_write = parent_upread.upgrade()
        14. PERSIST parent_write.commit_dir_metadata(&fs)?
            // On error: ROLLBACK_D

        // --- Cache insertion ---
        15. EFFECT fs.insert_inode_cache(child.clone())
        16. RETURN Ok(child)

    CRASH_WINDOWS:
        W1: After step 7 (parent links_count incremented in memory).
            Not persisted yet. On reboot: old link count from disk. No leak.

        W2: After step 9 (child inode allocated), before step 11.
            Child inode in bitmap but no data block. Orphan inode.
            fsck: reclaims orphan.

        W3: After step 11 (make_empty persisted), before step 12.
            Child has "."/"..". Parent has no entry. Orphan directory.
            fsck: reclaims orphan, adjusts link counts.

        W4: After step 12 (entry in page cache), before step 14.
            Entry written to parent page cache but metadata not committed.
            May or may not survive reboot depending on page writeback.

        W5: After step 14. Fully consistent.

    ROLLBACK:
        ROLLBACK_A (step 9 error):
            parent_write.desc.links_count -= 1
            CODE: kernel/src/fs/ext2/inode.rs:1088-1090

        ROLLBACK_B (step 11 error):
            fs.free_inode(child_ino, true)
            parent_write.desc.links_count -= 1
            CODE: kernel/src/fs/ext2/inode.rs:1096-1099

        ROLLBACK_C (step 12 error):
            child_inner.release_dir_data_blocks_for_cleanup(&fs)
            fs.free_inode(child_ino, true)
            parent_write.desc.links_count -= 1
            CODE: kernel/src/fs/ext2/inode.rs:1105-1112

        ROLLBACK_D (step 14 error):
            self.delete_entry(name)
            child_inner.release_dir_data_blocks_for_cleanup(&fs)
            fs.free_inode(child_ino, true)
            parent_write.desc.links_count -= 1
            CODE: kernel/src/fs/ext2/inode.rs:1117-1124

    SATISFIES_PROOF {
        PRE:       type guards + name validation ==> HOARE.PRE
        POST.new_ino:  step 9 allocates fresh ino ==> HOARE.POST (new_ino fresh)
        POST.child:    step 11 make_empty writes "."/".."; create_inode
                       sets links_count=2 ==> HOARE.POST child fields
        POST.entry:    step 12+14 add entry to parent ==> HOARE.POST dir entry
        POST.parent:   step 7 links_count += 1 ==> HOARE.POST parent links
        FRAME:     only self.inner and child modified ==> HOARE.FRAME
        POST_ERR:  ROLLBACK_A..D restore all mutations ==> FS' = FS
        CRASH:     W1..W4 subset of HOARE.CRASH set; W5 = FS'
    }
}

/// =============================================================================
/// SECTION 3: LINK MANAGEMENT PROTOCOLS (link, unlink, rmdir)
/// =============================================================================

/// ---------------------------------------------------------------------------
/// PROTOCOL link
/// ---------------------------------------------------------------------------
/// Creates a hard link in this directory to an existing non-directory inode.
///
/// CODE: kernel/src/fs/ext2/inode.rs:3509-3558

PROTOCOL link {
    SATISFIES: layer1::link
    DISPATCH: always
    CODE: kernel/src/fs/ext2/inode.rs:3509-3558

    LOCKS:
        WRITE(old.inner) for link count increment
        UPREAD(self.inner) -> WRITE(self.inner) for add_entry
        WRITE(old.inner) for persist

    STEPS:
        // --- Validation ---
        1. GUARD  self.type_ == Dir, else Err(ENOTDIR)
        2. GUARD  old.type_ != Dir, else Err(EPERM)
        3. GUARD  old.inner.read().desc.links_count < MAX_LINK_COUNT,
                  else Err(EOVERFLOW)
        4. GUARD  name valid and not "."/".."), else Err(EINVAL)
        5. EFFECT fs = self.fs_arc()?
        6. GUARD  Arc::ptr_eq(&fs, &old.fs_arc()?), else Err(EINVAL)

        // --- Increment target link count ---
        7. LOCK   old_inner = old.inner.write()
        8. MUTATE old_inner.desc.ctime = now()
        9. MUTATE old_inner.desc.links_count += 1
        10. UNLOCK old_inner

        // --- Add directory entry ---
        11. EFFECT self.add_entry(name, old.ino, dir_ft)?
            // On error: ROLLBACK (old.links_count -= 1)

        // --- Persist target inode ---
        12. LOCK   old_inner = old.inner.write()
        13. PERSIST old_inner.persist_inode_and_sync(&fs)?
        14. UNLOCK old_inner
        15. RETURN Ok(())

    CRASH_WINDOWS:
        W1: After step 9 (link count incremented in memory), before step 11.
            Link count incremented but no dir entry. On reboot: old link
            count restored from disk. No leak.

        W2: After step 11 (dir entry written), before step 13.
            Dir entry exists but link count not persisted to disk.
            fsck: detects link count mismatch, repairs.

        W3: After step 13. Fully consistent.

    ROLLBACK:
        add_entry failure (step 11):
            old.inner.write().desc.links_count -= 1
            CODE: kernel/src/fs/ext2/inode.rs:3551-3553

    SATISFIES_PROOF {
        PRE:       type guards + link count check ==> HOARE.PRE
        POST.entry:    step 11 add_entry ==> dirs[dir_ino][name] = (child_ino, type_)
        POST.links:    step 9 links_count += 1 ==> HOARE.POST links_count
        POST.ctime:    step 8 ctime = now() ==> HOARE.POST ctime
        FRAME:     only self.inner and old.inner modified ==> HOARE.FRAME
        POST_ERR:  rollback links_count -= 1 ==> FS' = FS
        CRASH:     W1 = no-op (memory only); W2 in HOARE.CRASH; W3 = FS'
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL unlink
/// ---------------------------------------------------------------------------
/// Removes a non-directory entry from this directory. Decrements child
/// links_count; marks freed if it reaches zero.
///
/// CODE: kernel/src/fs/ext2/inode.rs:3563-3605

PROTOCOL unlink {
    SATISFIES: layer1::unlink
    DISPATCH: always
    CODE: kernel/src/fs/ext2/inode.rs:3563-3605

    LOCKS:
        READ(self.inner) for lookup
        UPREAD(self.inner) -> WRITE(self.inner) for delete_entry
        WRITE(child.inner) for link count update + persist

    STEPS:
        // --- Validation ---
        1. GUARD  self.type_ == Dir, else Err(ENOTDIR)
        2. GUARD  name valid and not "."/".."), else Err(EINVAL)
        3. EFFECT fs = self.fs_arc()?

        // --- Lookup child ---
        4. LOCK   self_read = self.inner.read()
        5. child_ino = self_read.find_entry(name)?
        6. UNLOCK self_read
        7. EFFECT child = fs.read_inode(child_ino)?

        // --- Type check ---
        8. GUARD  child.type_ != Dir, else Err(EISDIR)

        // --- Delete directory entry ---
        9. EFFECT self.delete_entry(name)?
           // Internally: UPREAD -> find_entry_target -> delete_entry_in_cache
           //   -> upgrade -> commit_dir_metadata

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
        15. UNLOCK child_inner
        16. RETURN Ok(())

    CRASH_WINDOWS:
        W1: After step 9 (dir entry deleted), before step 14.
            Dir entry removed but child link count not decremented on disk.
            Child has stale link count. fsck: repairs link count.

        W2: After step 14. Fully consistent.
            If links_count=0: inode marked for reclamation.

    ROLLBACK:
        -- No explicit rollback. delete_entry and link count decrement
        -- are sequential. If persist fails, in-memory state is stale
        -- but child remains in inode cache.

    SATISFIES_PROOF {
        PRE:       type guards + name validation ==> HOARE.PRE
        POST.entry:    step 9 delete_entry ==> name not in dirs[dir_ino]
        POST.links:    step 12 links_count -= 1 ==> HOARE.POST links_count
        POST.ctime:    step 11 ctime = now() ==> HOARE.POST ctime
        POST.freed:    step 13 conditional freed ==> HOARE.POST freed
        FRAME:     only self.inner and child.inner modified ==> HOARE.FRAME
        POST_ERR:  guards fail before mutation ==> FS' = FS
        CRASH:     W1 in HOARE.CRASH (entry deleted, stale links); W2 = FS'
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL rmdir
/// ---------------------------------------------------------------------------
/// Removes an empty subdirectory. Three-phase persist: delete parent entry,
/// free child, decrement parent links_count.
///
/// CODE: kernel/src/fs/ext2/inode.rs:999-1048

PROTOCOL rmdir {
    SATISFIES: layer1::rmdir
    DISPATCH: always
    CODE: kernel/src/fs/ext2/inode.rs:999-1048

    LOCKS:
        UPREAD(self.inner) -> WRITE(self.inner)
        READ(child.inner) for empty check
        WRITE(child.inner) for link count update + persist

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

    CRASH_WINDOWS:
        W1: After step 14 (parent entry deleted), before step 20.
            Parent entry deleted but child not marked freed.
            Orphan directory with links_count=2.
            fsck: detects unreferenced directory, reclaims.

        W2: After step 20 (child freed), before step 23.
            Child freed but parent links_count not decremented.
            fsck: detects parent link count mismatch, repairs.

        W3: After step 23. Fully consistent.

    ROLLBACK:
        -- No explicit rollback. Operations are sequential and each
        -- persist is independent. Partial completion leaves state
        -- that fsck can repair.

    SATISFIES_PROOF {
        PRE:       type guards + empty_dir check ==> HOARE.PRE
        POST.entry:    step 12+14 delete entry ==> name not in dirs[dir_ino]
        POST.child:    steps 16-20 set size=0, links=0, freed
                       ==> HOARE.POST child fields
        POST.parent:   step 22 links_count -= 1 ==> HOARE.POST parent links
        FRAME:     only self.inner and child.inner modified ==> HOARE.FRAME
        POST_ERR:  guards fail before mutation ==> FS' = FS
        CRASH:     W1 = orphan dir in HOARE.CRASH;
                   W2 = stale parent links in HOARE.CRASH; W3 = FS'
    }
}

/// =============================================================================
/// SECTION 4: RENAME PROTOCOLS (same-dir, cross-dir)
/// =============================================================================

/// ---------------------------------------------------------------------------
/// PROTOCOL rename_same_dir
/// ---------------------------------------------------------------------------
/// Renames an entry within the same directory. Single write lock.
/// Handles optional replacement of an existing entry at new_name.
///
/// CODE: kernel/src/fs/ext2/inode.rs:3668-3748

PROTOCOL rename_same_dir {
    SATISFIES: layer1::rename
    DISPATCH: src_dir.ino = dst_dir.ino
    CODE: kernel/src/fs/ext2/inode.rs:3668-3748

    LOCKS:
        WRITE(self.inner)                   // single dir lock
        WRITE(old_inode.inner) for ctime
        WRITE(existing.inner) if replacement

    STEPS:
        // --- Single lock (same directory) ---
        1. LOCK   inner = self.inner.write()
        2. old_ino = inner.find_entry(old_name)?
        3. EFFECT old_inode = fs.read_inode(old_ino)?
        4. old_is_dir = old_inode.type_ == Dir
        5. moved_ft = inode_type_to_dir_file_type(old_inode.type_)

        // --- Check for existing entry at new_name ---
        6. existing_ino = inner.find_entry(new_name).ok()

        IF existing_ino.is_some():
            // --- Replacement path ---
            7a. EFFECT existing = fs.read_inode(existing_ino)?
            8a. GUARD  type compatibility (dir<->dir, non-dir<->non-dir)
            9a. IF existing is dir: GUARD empty_dir()
            10a. EFFECT inner.set_link(new_name, old_ino, ft, true)?
            11a. LOCK   existing_inner = existing.inner.write()
            12a. MUTATE existing_inner.desc.ctime = now()
            13a. IF old_is_dir: existing_inner.links_count -= 1  // for ".."
            14a. MUTATE existing_inner.links_count -= 1
            15a. IF links_count == 0: set dtime, is_freed
            16a. PERSIST existing_inner.persist_inode_and_sync(&fs)?
            17a. UNLOCK existing_inner
        ELSE:
            // --- No replacement: add new entry ---
            7b. EFFECT inner.add_entry(new_name, old_ino, ft)?

        // --- Update moved inode ctime ---
        18. LOCK   old_inner = old_inode.inner.write()
        19. MUTATE old_inner.desc.ctime = now()
        20. PERSIST old_inner.persist_inode_and_sync(&fs)?
        21. UNLOCK old_inner

        // --- Delete old entry ---
        22. EFFECT inner.delete_entry(old_name)?

        // --- Directory link count adjustments ---
        23. IF old_is_dir:
              IF no replacement: inner.desc.links_count += 1
              inner.desc.links_count -= 1
              PERSIST inner.persist_inode_and_sync(&fs)?

        24. UNLOCK inner
        25. RETURN Ok(())

    CRASH_WINDOWS:
        W1: After step 10a/7b (new entry added/replaced), before step 22.
            Both old and new entries exist. Duplicate references.
            fsck: detects duplicate directory entries, repairs.

        W2: After step 22 (old entry deleted), before step 23.
            Entry moved but link counts stale.
            fsck: repairs link counts from directory entries.

        W3: After step 23 (all persists). Fully consistent.

    ROLLBACK:
        -- No explicit rollback. Rename is a sequence of independent
        -- mutations. Partial completion is repaired by fsck.

    SATISFIES_PROOF {
        PRE:       type guards + name validation ==> HOARE.PRE
        POST.src:      step 22 delete_entry ==> old_name not in dirs[src_dir]
        POST.dst:      step 10a/7b ==> dirs[dst_dir][new_name] = (moved_ino, type_)
        POST.ctime:    step 19 ==> moved_ino.ctime = now()
        POST.replace:  steps 12a-16a ==> existing links decremented
        POST.dir_links: step 23 adjusts parent links ==> HOARE.POST link counts
        FRAME:     only dir inner + moved/existing inodes modified ==> HOARE.FRAME
        POST_ERR:  guards fail before mutation ==> FS' = FS
        CRASH:     W1 = duplicate refs in HOARE.CRASH;
                   W2 = stale links in HOARE.CRASH; W3 = FS'
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL rename_cross_dir
/// ---------------------------------------------------------------------------
/// Moves an entry from source directory to a different target directory.
/// Acquires write locks on both directories in ascending ino order to
/// prevent deadlock. Handles optional replacement and ".." update for
/// directory moves.
///
/// CODE: kernel/src/fs/ext2/inode.rs:3750-3855

PROTOCOL rename_cross_dir {
    SATISFIES: layer1::rename
    DISPATCH: src_dir.ino != dst_dir.ino
    CODE: kernel/src/fs/ext2/inode.rs:3750-3855

    LOCKS:
        WRITE(min_ino.inner) -> WRITE(max_ino.inner)  // deadlock prevention
        WRITE(old_inode.inner) for ".." check and ctime
        WRITE(existing.inner) if replacement

    STEPS:
        // --- Acquire two write locks (ascending ino order) ---
        1. LOCK   (self_inner, target_inner) = write_lock_two_inodes(self, target)
           // If self.ino < target.ino: lock self first
           // If target.ino < self.ino: lock target first

        // --- Lookup moved entry ---
        2. old_ino = self_inner.find_entry(old_name)?
        3. EFFECT old_inode = fs.read_inode(old_ino)?
        4. old_is_dir = old_inode.type_ == Dir
        5. moved_ft = inode_type_to_dir_file_type(old_inode.type_)

        // --- If moving dir, verify ".." points to self ---
        6. IF old_is_dir:
             LOCK old_inner = old_inode.inner.write()
             GUARD old_inner.find_entry("..") == self.ino, else Err(EIO)
             UNLOCK old_inner

        // --- Check existing entry at new_name in target ---
        7. existing_ino = target_inner.find_entry(new_name).ok()

        IF existing_ino.is_some():
            // --- Replacement path ---
            8a.  EFFECT existing = fs.read_inode(existing_ino)?
            9a.  GUARD  type compatibility
            10a. IF existing is dir: GUARD empty_dir()
            11a. EFFECT target_inner.set_link(new_name, old_ino, ft, true)?
            12a. LOCK   existing_inner = existing.inner.write()
            13a. MUTATE existing_inner.desc.ctime = now()
            14a. IF old_is_dir: existing_inner.links_count -= 1
            15a. MUTATE existing_inner.links_count -= 1
            16a. IF links_count == 0: set dtime, is_freed
            17a. PERSIST existing_inner.persist_inode_and_sync(&fs)?
            18a. UNLOCK existing_inner
        ELSE:
            // --- No replacement: add entry ---
            8b.  EFFECT target_inner.add_entry(new_name, old_ino, ft)?
            9b.  IF old_is_dir: target_inner.links_count += 1

        // --- Update moved inode ctime ---
        19. LOCK   old_inner = old_inode.inner.write()
        20. MUTATE old_inner.desc.ctime = now()
        21. PERSIST old_inner.persist_inode_and_sync(&fs)?
        22. UNLOCK old_inner

        // --- Delete old entry from source ---
        23. EFFECT self_inner.delete_entry(old_name)?

        // --- If dir: update ".." and adjust parent link counts ---
        24. IF old_is_dir:
              LOCK old_inner = old_inode.inner.write()
              old_inner.set_link("..", target.ino, Dir, false)?
              UNLOCK old_inner
              self_inner.links_count -= 1   // old parent loses subdir
              PERSIST self_inner.persist_inode_and_sync(&fs)?
              PERSIST target_inner.persist_inode_and_sync(&fs)?

        25. UNLOCK (self_inner, target_inner)
        26. RETURN Ok(())

    CRASH_WINDOWS:
        W1: After step 11a/8b (new entry in target), before step 23.
            Entry exists in both directories. Duplicate references.
            fsck: detects duplicate, repairs link counts.

        W2: After step 23 (old entry deleted), before step 24.
            Entry moved but ".." still points to old parent.
            fsck: repairs ".." and link counts.

        W3: During step 24 (".." updated, partial link count persist).
            ".." correct but parent link counts stale.
            fsck: recomputes link counts from directory entries.

        W4: After step 24 completes. Fully consistent.

    ROLLBACK:
        -- No explicit rollback. Rename is a multi-step sequence.
        -- Partial completion requires fsck repair.

    SATISFIES_PROOF {
        PRE:       type guards + name validation + same fs ==> HOARE.PRE
        POST.src:      step 23 delete_entry ==> old_name not in dirs[src_dir]
        POST.dst:      step 11a/8b ==> dirs[dst_dir][new_name] = (moved_ino, type_)
        POST.ctime:    step 20 ==> moved_ino.ctime = now()
        POST.replace:  steps 13a-17a ==> existing links decremented
        POST.dotdot:   step 24 set_link("..") ==> dirs[moved_ino][".."] = dst_dir
        POST.src_links: step 24 self_inner.links_count -= 1
                        ==> HOARE.POST src_dir links
        POST.dst_links: step 9b links_count += 1 (no replacement)
                        ==> HOARE.POST dst_dir links
        FRAME:     only dir inners + moved/existing inodes modified ==> HOARE.FRAME
        POST_ERR:  guards fail before mutation ==> FS' = FS
        CRASH:     W1..W3 subset of HOARE.CRASH set; W4 = FS'
    }
}

/// =============================================================================
/// SECTION 5: SPECIAL FILE CREATION PROTOCOL (mknod)
/// =============================================================================

/// ---------------------------------------------------------------------------
/// PROTOCOL mknod
/// ---------------------------------------------------------------------------
/// Creates a special file inode (char device, block device, or named pipe)
/// by delegating to create_non_dir and optionally encoding a device ID.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/inode.rs:135-151

PROTOCOL mknod {
    SATISFIES: layer1::mknod
    DISPATCH: always
    CODE: kernel/src/fs/ext2/impl_for_vfs/inode.rs:135-151

    LOCKS:
        // Inherits from create_non_dir protocol
        UPREAD(self.inner) -> WRITE(self.inner)
        // + WRITE(new_inode.inner) for set_device_id

    STEPS:
        // --- Map MknodType to InodeType + optional device_id ---
        1. MATCH type_:
             CharDevice(dev_id)  -> (InodeType::CharDevice, Some(dev_id))
             BlockDevice(dev_id) -> (InodeType::BlockDevice, Some(dev_id))
             NamedPipe           -> (InodeType::NamedPipe, None)

        // --- Delegate to create (non-dir path) ---
        2. EFFECT new_inode = Inode::create(self, name, inode_type, mode)?
           // See create_non_dir protocol for full steps.

        // --- Encode device ID if applicable ---
        3. IF device_id.is_some():
             EFFECT new_inode.set_device_id(device_id)?
             // LOCK   WRITE(new_inode.inner)
             // Encodes dev_id in block_ptrs[0..2]
             // Updates ctime
             // PERSIST persist_inode_and_sync
             // UNLOCK

        4. RETURN Ok(new_inode)

    CRASH_WINDOWS:
        W1: Inherits W1..W3 from create_non_dir protocol.
            Inode allocated but no directory entry (orphan).

        W2: After create completes, before set_device_id (step 3).
            Inode exists in directory but has no device encoding.
            Special file inode with zero block_ptrs.

        W3: After set_device_id completes. Fully consistent.

    ROLLBACK:
        create failure (step 2):
            Handled by create_non_dir rollback (free_inode).
        set_device_id failure (step 3):
            Inode exists but device ID not set.
            No explicit rollback of create on set_device_id failure.

    SATISFIES_PROOF {
        PRE:       type mapping + create_non_dir PRE ==> HOARE.PRE
        POST.inode:    step 2 create ==> new inode with type_, mode, links=1
        POST.device:   step 3 set_device_id ==> block_ptrs encode dev_id
        POST.entry:    step 2 create ==> dirs[dir_ino][name] = (new_ino, type_)
        FRAME:     inherits from create_non_dir ==> HOARE.FRAME
        POST_ERR:  create rollback ==> FS' = FS
        CRASH:     W1 inherits create_non_dir CRASH;
                   W2 = missing device encoding in HOARE.CRASH; W3 = FS'
    }
}
