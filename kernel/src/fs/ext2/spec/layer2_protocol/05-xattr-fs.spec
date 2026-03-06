// SPDX-License-Identifier: MPL-2.0
//
// Layer 2: Implementation Protocol -- Extended Attributes & FileSystem Trait
//
// Maps each Layer 1 HOARE spec to concrete Asterinas Ext2 implementation:
// lock sequences, step-by-step execution, crash windows, rollback.
//
// References:
//   Layer 1: layer1_hoare/05-xattr-fs.spec
//   State:   layer2_protocol/00-impl-state.spec

/// =============================================================================
/// SECTION 1: EXTENDED ATTRIBUTE PROTOCOLS
/// =============================================================================

/// ---------------------------------------------------------------------------
/// PROTOCOL 1: get_xattr
/// ---------------------------------------------------------------------------
/// Reads one extended-attribute value. Single-lock read protocol.
///
/// CODE: kernel/src/fs/ext2/inode.rs:339-349
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:225-228

PROTOCOL get_xattr {
    SATISFIES: layer1::get_xattr
    CODE: kernel/src/fs/ext2/inode.rs:339-349

    LOCKS: XATTR_WRITE(self.xattr)
    // Note: write lock because Xattr::get_xattr may lazy-load the block
    // from disk on first access.

    STEPS:
        1. VFS dispatch: self.check_permission(Permission::MAY_READ)?
        2. LOCK   xattr = self.xattr.as_ref()
                          .ok_or(EOPNOTSUPP)?
                          .write()
        3. CALL   result = xattr.get_xattr(name, value_writer)?
           // Lazy-loads xattr block from disk if not yet cached.
           // Searches entries for matching name.
           // Writes value bytes into value_writer.
        4. UNLOCK xattr  (implicit drop at scope exit)
        5. RETURN Ok(result)

    CRASH_WINDOWS:
        -- None. The lazy-load reads from disk but does not write.
        -- No crash window: operation is a pure read.

    ROLLBACK:
        -- Step 3 failure: xattr lock released, no state modified.
        -- No rollback needed.

    SATISFIES_PROOF {
        PRE:
            Step 1 checks permission.
            Step 2 checks self.xattr.is_some() => type_ in {Dir, Reg}.
            PROTOCOL.REQUIRE => HOARE.PRE.

        POST.FS_unchanged:
            No mutation steps => FS' = FS.

        POST.buf:
            Step 3 writes xattr value to value_writer =>
            buf[0..size] = FS.xattrs[ino][name].

        POST.size:
            Step 3 returns byte count written => size = |FS.xattrs[ino][name]|.

        FRAME:
            No fields modified => everything unchanged.

        POST_ERR:
            Step 2 ok_or(EOPNOTSUPP) => EOPNOTSUPP if xattr unsupported.
            Step 3 get_xattr => ENODATA if name not found, ERANGE if buf too small.
            No mutation on any error path => FS' = FS.

        CRASH:
            No writes => FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL 2: set_xattr
/// ---------------------------------------------------------------------------
/// Creates or replaces one extended attribute. Two-lock, two-phase protocol.
///
/// CODE: kernel/src/fs/ext2/inode.rs:373-396
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:215-223

PROTOCOL set_xattr {
    SATISFIES: layer1::set_xattr
    CODE: kernel/src/fs/ext2/inode.rs:373-396

    LOCKS: XATTR_WRITE(self.xattr) -> WRITE(self.inner)
    // Lock ordering: L1 (xattr) before L2 (inner), per LOCK_ORDER.

    STEPS:
        // Phase 1: Xattr block mutation (xattr lock held)
        1. VFS dispatch: self.check_permission(Permission::MAY_WRITE)?
        2. LOCK    xattr = self.xattr.as_ref()
                           .ok_or(EOPNOTSUPP)?
                           .write()
        3. CALL    xattr.set_xattr(name, value_reader, flags)?
           // Allocates xattr block if file_acl == 0.
           // Checks CREATE/REPLACE flag semantics.
           // Inserts or replaces entry in xattr block.
        4. READ    new_bid = xattr.bid()
        5. UNLOCK  xattr  (explicit drop)

        // Phase 2: Update inode descriptor (inner lock held)
        6. CALL    fs = self.fs_arc()?
        7. LOCK    inner = self.inner.write()
        8. MUTATE  inner.desc.file_acl = new_bid
        9. MUTATE  inner.desc.ctime = now()
        10. PERSIST inner.persist_inode_and_sync(&fs)?
        11. UNLOCK  inner  (implicit drop)
        12. RETURN  Ok(())

    CRASH_WINDOWS:
        W1: After step 5, before step 10.
            Xattr block written to page cache but inode file_acl not persisted.
            Recovery: orphan xattr block; fsck reclaims via refcount check.
        W2: During step 10 (persist_inode_and_sync).
            Inode descriptor partially written.
            Recovery: fsck repairs inode from on-disk data.

    ROLLBACK:
        Phase 1 failure (step 3):
            xattr.set_xattr handles internal rollback.
            Xattr lock released, no inode mutation.
        Phase 2 failure (step 10):
            Xattr block already written; inode file_acl not updated.
            No explicit rollback of xattr block.
            Orphan block reclaimed by fsck.

    SATISFIES_PROOF {
        PRE:
            Step 1 checks MAY_WRITE permission.
            Step 2 ok_or(EOPNOTSUPP) => type_ in {Dir, Reg}.
            PROTOCOL.REQUIRE => HOARE.PRE.

        POST.xattrs:
            Step 3 inserts/replaces entry =>
            FS'.xattrs[ino][name] = value.

        POST.file_acl:
            Steps 4,8 propagate new_bid =>
            FS'.inodes[ino].file_acl = new_bid.

        POST.ctime:
            Step 9 sets ctime = now() =>
            FS'.inodes[ino].ctime >= FS.inodes[ino].ctime.

        FRAME:
            Only self.xattr and self.inner modified =>
            other inodes, dirs, data unchanged.

        POST_ERR:
            Step 2 => EOPNOTSUPP.
            Step 3 => EEXIST (CREATE + exists),
                      ENODATA (REPLACE + missing),
                      ENOSPC (no block space).
            Phase 1 failure: no inode mutation => FS' = FS.

        CRASH:
            W1: xattr block in page cache, inode stale =>
                FS_recovered in {FS.durable, partial}.
                fsck reclaims orphan xattr block.
            W2: persist partial => FS_recovered in {FS.durable, FS'}.
            All reachable states subset of HOARE.CRASH.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL 3: list_xattr
/// ---------------------------------------------------------------------------
/// Lists extended-attribute names in one namespace. Single-lock read protocol.
///
/// CODE: kernel/src/fs/ext2/inode.rs:354-368
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:230-233

PROTOCOL list_xattr {
    SATISFIES: layer1::list_xattr
    CODE: kernel/src/fs/ext2/inode.rs:354-368

    LOCKS: XATTR_WRITE(self.xattr)
    // Write lock because Xattr may lazy-load block from disk.

    STEPS:
        1. VFS dispatch: self.check_permission(Permission::MAY_ACCESS)?
        2. LOCK   xattr = self.xattr.as_ref()
                          .ok_or(EOPNOTSUPP)?
                          .write()
        3. CALL   result = xattr.list_xattr(namespace, list_writer)?
           // Lazy-loads xattr block if not cached.
           // Iterates entries matching namespace.
           // Writes null-separated names into list_writer.
        4. UNLOCK xattr  (implicit drop at scope exit)
        5. RETURN Ok(result)

    CRASH_WINDOWS:
        -- None. Pure read operation.

    ROLLBACK:
        -- Step 3 failure: xattr lock released, no state modified.
        -- No rollback needed.

    SATISFIES_PROOF {
        PRE:
            Step 1 checks MAY_ACCESS permission.
            Step 2 ok_or(EOPNOTSUPP) => type_ in {Dir, Reg}.
            PROTOCOL.REQUIRE => HOARE.PRE.

        POST.FS_unchanged:
            No mutation steps => FS' = FS.

        POST.buf:
            Step 3 writes null-separated names to list_writer =>
            buf[0..total_size] = null_join(names in namespace).

        POST.total_size:
            Step 3 returns byte count => total_size = |null_join(names)|.

        FRAME:
            No fields modified => everything unchanged.

        POST_ERR:
            Step 2 => EOPNOTSUPP if xattr unsupported.
            Step 3 => ERANGE if list_writer too small.
            No mutation on any error path => FS' = FS.

        CRASH:
            No writes => FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL 4: remove_xattr
/// ---------------------------------------------------------------------------
/// Removes one extended attribute. Two-lock, two-phase protocol.
///
/// CODE: kernel/src/fs/ext2/inode.rs:401-418
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:235-238

PROTOCOL remove_xattr {
    SATISFIES: layer1::remove_xattr
    CODE: kernel/src/fs/ext2/inode.rs:401-418

    LOCKS: XATTR_WRITE(self.xattr) -> WRITE(self.inner)
    // Lock ordering: L1 (xattr) before L2 (inner), per LOCK_ORDER.

    STEPS:
        // Phase 1: Xattr block mutation (xattr lock held)
        1. VFS dispatch: self.check_permission(Permission::MAY_WRITE)?
        2. LOCK    xattr = self.xattr.as_ref()
                           .ok_or(EOPNOTSUPP)?
                           .write()
        3. CALL    xattr.remove_xattr(name)?
           // Removes entry from xattr block.
           // May free xattr block if last entry removed.
        4. READ    new_bid = xattr.bid()
           // 0 if block was freed, otherwise same block id.
        5. UNLOCK  xattr  (explicit drop)

        // Phase 2: Update inode descriptor (inner lock held)
        6. CALL    fs = self.fs_arc()?
        7. LOCK    inner = self.inner.write()
        8. MUTATE  inner.desc.file_acl = new_bid
        9. PERSIST inner.persist_inode_and_sync(&fs)?
        10. UNLOCK  inner  (implicit drop)
        11. RETURN  Ok(())

    CRASH_WINDOWS:
        W1: After step 5, before step 9.
            Xattr block modified in page cache but inode file_acl not persisted.
            Recovery: stale file_acl points to old xattr block (or freed block).
            fsck detects via refcount mismatch.
        W2: During step 9 (persist_inode_and_sync).
            Inode descriptor partially written.
            Recovery: fsck repairs inode from on-disk data.

    ROLLBACK:
        Phase 1 failure (step 3):
            xattr.remove_xattr handles internal rollback.
            Xattr lock released, no inode mutation.
        Phase 2 failure (step 9):
            Xattr block already modified; inode file_acl not updated.
            No explicit rollback of xattr block.
            Orphan or dangling block reclaimed by fsck.

    SATISFIES_PROOF {
        PRE:
            Step 1 checks MAY_WRITE permission.
            Step 2 ok_or(EOPNOTSUPP) => type_ in {Dir, Reg}.
            PROTOCOL.REQUIRE => HOARE.PRE.

        POST.xattrs:
            Step 3 removes entry =>
            FS'.xattrs[ino] = FS.xattrs[ino] \ {name}.

        POST.file_acl:
            Steps 4,8 propagate new_bid =>
            FS'.inodes[ino].file_acl = new_bid.

        FRAME:
            Only self.xattr and self.inner modified =>
            other inodes, dirs, data unchanged.

        POST_ERR:
            Step 2 => EOPNOTSUPP.
            Step 3 => ENODATA if name not found.
            Phase 1 failure: no inode mutation => FS' = FS.

        CRASH:
            W1: xattr block in page cache, inode stale =>
                FS_recovered in {FS.durable, partial}.
                fsck reclaims orphan/dangling xattr block.
            W2: persist partial => FS_recovered in {FS.durable, FS'}.
            All reachable states subset of HOARE.CRASH.
    }
}

/// =============================================================================
/// SECTION 2: FILESYSTEM TRAIT PROTOCOLS
/// =============================================================================

/// ---------------------------------------------------------------------------
/// PROTOCOL 5: fs_name
/// ---------------------------------------------------------------------------
/// Returns the filesystem type name. Trivial accessor.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:12-15

PROTOCOL fs_name {
    SATISFIES: layer1::fs_name
    CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:12-15

    LOCKS: none

    STEPS:
        1. RETURN "ext2"

    CRASH_WINDOWS:
        -- None. Pure constant return.

    ROLLBACK:
        -- None.

    SATISFIES_PROOF {
        PRE:
            No precondition required => HOARE.PRE trivially satisfied.

        POST.ret:
            Step 1 returns "ext2" => ret = "ext2".

        FRAME:
            No fields accessed or modified => everything unchanged.

        CRASH:
            No writes => FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL 6: fs_sync
/// ---------------------------------------------------------------------------
/// Full filesystem sync: all inodes, metadata, device flush.
/// Three-phase protocol with multiple lock acquisitions.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:17-23
/// CODE: kernel/src/fs/ext2/fs.rs:709-803 (sync_metadata)
/// CODE: kernel/src/fs/ext2/fs.rs:806-834 (sync_all_inodes)

PROTOCOL fs_sync {
    SATISFIES: layer1::fs_sync
    CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:17-23

    LOCKS:
        Phase 1: per-inode READ/WRITE locks (during sync_all)
        Phase 2: SB_WRITE, per-group locks (during sync_metadata)
        Phase 3: none (device sync)

    STEPS:
        // Phase 1: Sync all cached inodes
        1. CALL  self.sync_all()?
           // Iterates all block group inode caches.
           // For each cached inode: acquires inner lock, persists descriptor,
           // flushes page cache, evicts freed inodes, syncs bitmaps, and
           // writes dirty group descriptors into the descriptor-table segment.

        // Phase 2: Sync filesystem-global metadata
        2. CALL  self.sync_metadata()?
           // 2a. Recomputes sb.free_blocks from group descriptors.
           // 2b. Recomputes sb.free_inodes from group descriptors.
           // 2c. Writes descriptor-table segment to device copies.
           // 2d. Writes primary superblock to device.
           // 2e. Writes backup superblocks to device.

        // Phase 3: Flush device write cache
        3. CALL  self.block_device().sync()?

        4. RETURN Ok(())

    CRASH_WINDOWS:
        W1: During step 1 (sync_all).
            Some inodes persisted to device, others not.
            Recovery: unpersisted inodes revert to FS.durable state.
        W2: During step 2a-2c (sync_metadata group writes).
            Some group descriptors written, others not.
            Superblock counters may be stale.
            Recovery: fsck recomputes free counts from bitmaps.
        W3: During step 2d-2f (superblock writes).
            Primary superblock written but backups not (or vice versa).
            Recovery: fsck uses most recent valid superblock copy.
        W4: During step 3 (device sync).
            Device write cache partially flushed.
            Recovery: depends on device write ordering guarantees.

    ROLLBACK:
        -- Sync is idempotent. No rollback needed.
        -- Partial sync leaves a valid (if stale) on-disk state.
        -- Re-running sync completes any unfinished work.

    SATISFIES_PROOF {
        PRE:
            No precondition => HOARE.PRE trivially satisfied.

        POST.durable:
            Step 1 persists all inodes.
            Step 2 persists metadata.
            Step 3 flushes device cache.
            After step 3: FS'.durable = FS'.

        POST.sb_counters:
            Step 2b,2c recompute free_blocks and free_inodes
            from group descriptors =>
            FS'.sb.free_blocks = actual_free_blocks(FS'),
            FS'.sb.free_inodes = actual_free_inodes(FS').

        FRAME:
            Sync does not modify in-memory inode/dir/data/xattr state.
            Only durable snapshot and sb counters change.

        POST_ERR:
            Steps 1,2,3 propagate EIO on device failure.
            Abstract model: FS' = FS on error.

        CRASH:
            W1-W4 produce partial states between FS.durable and FS'.
            All satisfy: FS.durable <= FS_partial <= FS'.
            fsck repairs structural inconsistencies.
            Subset of HOARE.CRASH.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL 7: fs_root_inode
/// ---------------------------------------------------------------------------
/// Returns the cached root inode. Trivial accessor.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:25-28
/// CODE: kernel/src/fs/ext2/fs.rs:137-139

PROTOCOL fs_root_inode {
    SATISFIES: layer1::fs_root_inode
    CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:25-28

    LOCKS: none
    // root_inode field is immutable after mount (Arc<Inode>).

    STEPS:
        1. RETURN self.root_inode.clone()
           // Clones the Arc; no lock needed.

    CRASH_WINDOWS:
        -- None. Pure accessor.

    ROLLBACK:
        -- None.

    SATISFIES_PROOF {
        PRE:
            No precondition => HOARE.PRE trivially satisfied.

        POST.ret:
            self.root_inode is initialized at mount with ino = ROOT_INO = 2.
            Step 1 clones Arc => ret.ino = ROOT_INO.

        POST.invariants:
            INV-02 guarantees root inode is Dir, alive, links >= 2.

        FRAME:
            No fields modified => everything unchanged.

        CRASH:
            No writes => FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL 8: fs_sb
/// ---------------------------------------------------------------------------
/// Returns a snapshot of the superblock as a VFS SuperBlock struct.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:30-48

PROTOCOL fs_sb {
    SATISFIES: layer1::fs_sb
    CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:30-48

    LOCKS: SB_READ (via self.super_block())

    STEPS:
        1. LOCK   sb_guard = self.super_block()   // self.super_block.read()
        2. CONSTRUCT SuperBlock {
               magic   = MAGIC_NUM,
               bsize   = sb_guard.block_size(),
               blocks  = sb_guard.total_blocks(),
               bfree   = sb_guard.free_blocks_count(),
               bavail  = sb_guard.free_blocks_count()
                         .saturating_sub(sb_guard.reserved_blocks_count()),
               files   = sb_guard.total_inodes(),
               ffree   = sb_guard.free_inodes_count(),
               namelen = NAME_MAX,
               frsize  = sb_guard.fragment_size(),
           }
        3. UNLOCK sb_guard  (implicit drop)
        4. RETURN superblock

    CRASH_WINDOWS:
        -- None. Pure read under shared lock.

    ROLLBACK:
        -- None.

    SATISFIES_PROOF {
        PRE:
            No precondition => HOARE.PRE trivially satisfied.

        POST.ret:
            Step 2 reads sb fields under SB_READ lock.
            ABSTRACTION_MAP maps C.fs.super_block fields to FS.sb.
            ret.magic = MAGIC_NUM, ret.bsize = FS.sb.block_size, etc.

        FRAME:
            No fields modified => everything unchanged.

        CRASH:
            No writes => FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL 9: fs_event_subscriber_stats
/// ---------------------------------------------------------------------------
/// Returns the filesystem event subscriber statistics. Trivial accessor.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:50-53
/// CODE: kernel/src/fs/ext2/fs.rs:132-134

PROTOCOL fs_event_subscriber_stats {
    SATISFIES: layer1::fs_event_subscriber_stats
    CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:50-53

    LOCKS: none
    // fs_event_subscriber_stats field is immutable after mount.

    STEPS:
        1. RETURN &self.fs_event_subscriber_stats
           // Returns a reference to the immutable field.

    CRASH_WINDOWS:
        -- None. Pure accessor.

    ROLLBACK:
        -- None.

    SATISFIES_PROOF {
        PRE:
            No precondition => HOARE.PRE trivially satisfied.

        POST.ret:
            Step 1 returns reference to immutable field.
            ret is the FsEventSubscriberStats (opaque).

        FRAME:
            No fields modified => everything unchanged.

        CRASH:
            No writes => FS_recovered = FS.durable.
    }
}
