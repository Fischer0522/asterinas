// SPDX-License-Identifier: MPL-2.0
//
// Layer 2: Implementation Protocol — Metadata Operations
//
// 21 PROTOCOL specs mapping Layer 1 HOARE specs to Asterinas Ext2 implementation.
// Each PROTOCOL names its SATISFIES target, lock sequence, crash windows,
// and a SATISFIES_PROOF discharging every Layer 1 clause.
//
// Reference: 00-impl-state.spec for ConcreteState, lock model, abstraction map.
// Reference: layer1_hoare/01-metadata-ops.spec for the 21 HOARE specs.

/// =============================================================================
/// SECTION 1: PURE READ PROTOCOLS (13 methods)
/// =============================================================================

/// ---------------------------------------------------------------------------
/// PROTOCOL ino
/// ---------------------------------------------------------------------------

PROTOCOL ino {
    SATISFIES: layer1::ino
    CODE: kernel/src/fs/ext2/inode.rs:86-88
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:63-65

    LOCKS: none (self.ino is immutable, no lock needed)

    STEPS:
        1. RETURN self.ino as u64

    CRASH_WINDOWS: none (pure read)
    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:
            self.ino is set at construction time and never changes.
            I ∈ dom(FS.inodes) holds for any live Inode reference.
        POST.ret:
            Step 1 returns self.ino, which equals I by ABSTRACTION_MAP.
        FRAME:
            No fields read under lock, no mutation. FS' = FS trivially.
        POST_ERR:
            No error path exists.
        CRASH:
            No in-flight mutation. FS_recovered = FS.durable trivially.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL type_
/// ---------------------------------------------------------------------------

PROTOCOL type_ {
    SATISFIES: layer1::type_
    CODE: kernel/src/fs/ext2/inode.rs:272-274
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:67-69

    LOCKS: none (self.type_ is immutable)

    STEPS:
        1. RETURN self.type_

    CRASH_WINDOWS: none
    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:
            self.type_ is set at construction and never changes.
        POST.ret:
            Step 1 returns self.type_.
            ABSTRACTION_MAP: C.inode[ino].type_ = FS.inodes[I].type_.
        FRAME:
            No mutation. FS' = FS.
        POST_ERR:
            No error path.
        CRASH:
            No mutation. FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL size
/// ---------------------------------------------------------------------------

PROTOCOL size {
    SATISFIES: layer1::size
    CODE: kernel/src/fs/ext2/inode.rs:100-102
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:51-53

    LOCKS: READ(self.inner)

    STEPS:
        1. LOCK    inner = self.inner.read()
        2. result = inner.desc.size as usize
        3. UNLOCK  inner
        4. RETURN  result

    CRASH_WINDOWS: none
    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:
            READ lock is always acquirable; no precondition beyond existence.
        POST.ret:
            Step 2 reads inner.desc.size.
            ABSTRACTION_MAP: C.inner[ino].desc.size = FS.inodes[I].size.
        FRAME:
            READ lock prevents concurrent writes during read.
            No mutation performed. FS' = FS.
        POST_ERR:
            No error path.
        CRASH:
            No mutation. FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL mode
/// ---------------------------------------------------------------------------

PROTOCOL mode {
    SATISFIES: layer1::mode
    CODE: kernel/src/fs/ext2/inode.rs:276-278
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:71-73

    LOCKS: READ(self.inner)

    STEPS:
        1. LOCK    inner = self.inner.read()
        2. result = InodeMode::from_bits_truncate(inner.desc.perm.bits() as _)
        3. UNLOCK  inner
        4. RETURN  Ok(result)

    CRASH_WINDOWS: none
    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:
            READ lock always acquirable.
        POST.ret:
            Step 2 reads inner.desc.perm and converts to InodeMode.
            ABSTRACTION_MAP: C.inner[ino].desc.perm.bits() = FS.inodes[I].mode.
        FRAME:
            No mutation. FS' = FS.
        POST_ERR:
            VFS wraps in Ok(); no actual error path.
        CRASH:
            No mutation. FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL owner
/// ---------------------------------------------------------------------------

PROTOCOL owner {
    SATISFIES: layer1::owner
    CODE: kernel/src/fs/ext2/inode.rs:288-290
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:79-81

    LOCKS: READ(self.inner)

    STEPS:
        1. LOCK    inner = self.inner.read()
        2. uid = inner.desc.uid
        3. UNLOCK  inner
        4. RETURN  Ok(Uid::new(uid))

    CRASH_WINDOWS: none
    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:
            READ lock always acquirable.
        POST.ret:
            Step 2 reads inner.desc.uid.
            ABSTRACTION_MAP: C.inner[ino].desc.uid = FS.inodes[I].uid.
        FRAME:
            No mutation. FS' = FS.
        POST_ERR:
            VFS wraps in Ok(); no actual error path.
        CRASH:
            No mutation. FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL group
/// ---------------------------------------------------------------------------

PROTOCOL group {
    SATISFIES: layer1::group
    CODE: kernel/src/fs/ext2/inode.rs:300-302
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:87-89

    LOCKS: READ(self.inner)

    STEPS:
        1. LOCK    inner = self.inner.read()
        2. gid = inner.desc.gid
        3. UNLOCK  inner
        4. RETURN  Ok(Gid::new(gid))

    CRASH_WINDOWS: none
    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:
            READ lock always acquirable.
        POST.ret:
            Step 2 reads inner.desc.gid.
            ABSTRACTION_MAP: C.inner[ino].desc.gid = FS.inodes[I].gid.
        FRAME:
            No mutation. FS' = FS.
        POST_ERR:
            VFS wraps in Ok(); no actual error path.
        CRASH:
            No mutation. FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL atime
/// ---------------------------------------------------------------------------

PROTOCOL atime {
    SATISFIES: layer1::atime
    CODE: kernel/src/fs/ext2/inode.rs:312-314
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:95-97

    LOCKS: READ(self.inner)

    STEPS:
        1. LOCK    inner = self.inner.read()
        2. result = inner.desc.atime
        3. UNLOCK  inner
        4. RETURN  result

    CRASH_WINDOWS: none
    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:
            READ lock always acquirable.
        POST.ret:
            Step 2 reads inner.desc.atime.
            ABSTRACTION_MAP: C.inner[ino].desc.atime = FS.inodes[I].atime.
        FRAME:
            No mutation. FS' = FS.
        POST_ERR:
            Infallible; no error path.
        CRASH:
            No mutation. FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL mtime
/// ---------------------------------------------------------------------------

PROTOCOL mtime {
    SATISFIES: layer1::mtime
    CODE: kernel/src/fs/ext2/inode.rs:320-322
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:103-105

    LOCKS: READ(self.inner)

    STEPS:
        1. LOCK    inner = self.inner.read()
        2. result = inner.desc.mtime
        3. UNLOCK  inner
        4. RETURN  result

    CRASH_WINDOWS: none
    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:
            READ lock always acquirable.
        POST.ret:
            Step 2 reads inner.desc.mtime.
            ABSTRACTION_MAP: C.inner[ino].desc.mtime = FS.inodes[I].mtime.
        FRAME:
            No mutation. FS' = FS.
        POST_ERR:
            Infallible; no error path.
        CRASH:
            No mutation. FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL ctime
/// ---------------------------------------------------------------------------

PROTOCOL ctime {
    SATISFIES: layer1::ctime
    CODE: kernel/src/fs/ext2/inode.rs:328-330
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:111-113

    LOCKS: READ(self.inner)

    STEPS:
        1. LOCK    inner = self.inner.read()
        2. result = inner.desc.ctime
        3. UNLOCK  inner
        4. RETURN  result

    CRASH_WINDOWS: none
    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:
            READ lock always acquirable.
        POST.ret:
            Step 2 reads inner.desc.ctime.
            ABSTRACTION_MAP: C.inner[ino].desc.ctime = FS.inodes[I].ctime.
        FRAME:
            No mutation. FS' = FS.
        POST_ERR:
            Infallible; no error path.
        CRASH:
            No mutation. FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL metadata
/// ---------------------------------------------------------------------------

PROTOCOL metadata {
    SATISFIES: layer1::metadata
    CODE: kernel/src/fs/ext2/inode.rs:241-270
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:59-61

    LOCKS: READ(self.inner)

    STEPS:
        1. LOCK    inner = self.inner.read()
        2. EFFECT  (dev, blk_size) = self.fs.upgrade() match:
                     Some(fs) => (fs.block_device().id(), fs.block_size())
                     None     => (0, BLOCK_SIZE)
        3. EFFECT  rdev = if type_ ∈ {CharDevice, BlockDevice}:
                            inner.desc.decode_device_id()
                          else: 0
        4. CONSTRUCT Metadata { dev, ino, size, blk_size, blocks,
                                atime, mtime, ctime, type_, mode,
                                nlinks, uid, gid, rdev }
        5. UNLOCK  inner
        6. RETURN  metadata

    CRASH_WINDOWS: none
    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:
            READ lock always acquirable.
        POST.ret:
            Steps 2-4 read all fields under a single READ lock, ensuring
            a consistent snapshot. Each field maps via ABSTRACTION_MAP:
              ret.ino    = self.ino = I
              ret.size   = inner.desc.size = FS.inodes[I].size
              ret.mode   = inner.desc.perm = FS.inodes[I].mode
              ret.uid    = inner.desc.uid  = FS.inodes[I].uid
              ret.gid    = inner.desc.gid  = FS.inodes[I].gid
              ret.atime  = inner.desc.atime = FS.inodes[I].atime
              ret.mtime  = inner.desc.mtime = FS.inodes[I].mtime
              ret.ctime  = inner.desc.ctime = FS.inodes[I].ctime
              ret.nlinks = inner.desc.links_count = FS.inodes[I].links_count
              ret.blocks = inner.desc.blocks = FS.inodes[I].blocks
              ret.type_  = self.type_ = FS.inodes[I].type_
        FRAME:
            No mutation. FS' = FS.
        POST_ERR:
            Infallible; no error path.
        CRASH:
            No mutation. FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL page_cache
/// ---------------------------------------------------------------------------

PROTOCOL page_cache {
    SATISFIES: layer1::page_cache
    CODE: kernel/src/fs/ext2/inode.rs:1242-1244
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:119-121

    LOCKS: READ(self.inner)

    STEPS:
        1. LOCK    inner = self.inner.read()
        2. vmo = inner.page_cache.pages().clone()
        3. UNLOCK  inner
        4. RETURN  Some(vmo)

    CRASH_WINDOWS: none
    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:
            READ lock always acquirable.
        POST.ret:
            Step 2 clones the VMO backing the page cache.
            Returns Some(vmo) which backs FS.data[I] or FS.dirs[I].
        FRAME:
            No mutation. FS' = FS.
        POST_ERR:
            Infallible; always returns Some.
        CRASH:
            No mutation. FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL open
/// ---------------------------------------------------------------------------

PROTOCOL open {
    SATISFIES: layer1::open
    CODE: kernel/src/fs/ext2/impl_for_vfs/inode.rs:123-129

    LOCKS: none

    STEPS:
        1. RETURN None

    CRASH_WINDOWS: none
    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:
            No precondition; always callable.
        POST.ret:
            Step 1 returns None unconditionally.
            Matches layer1::open POST: ret = None.
        FRAME:
            No state accessed or mutated. FS' = FS.
        POST_ERR:
            No error path.
        CRASH:
            No mutation. FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL fs
/// ---------------------------------------------------------------------------

PROTOCOL fs {
    SATISFIES: layer1::fs
    CODE: kernel/src/fs/ext2/inode.rs:94-98
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:206-209

    LOCKS: none (Weak::upgrade is atomic)

    STEPS:
        1. EFFECT  arc = self.fs.upgrade().unwrap()
        2. RETURN  arc as Arc<dyn FileSystem>

    CRASH_WINDOWS: none
    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:
            self.fs.upgrade() must succeed (filesystem alive).
            Panics otherwise; no Err path at VFS level.
        POST.ret:
            Step 1 upgrades Weak<Ext2> to Arc<Ext2>.
            The returned Arc is the filesystem containing I,
            matching layer1::fs POST: ret = FS.sb.
        FRAME:
            No mutation. FS' = FS.
        POST_ERR:
            Panics on failure; no Err variant.
        CRASH:
            No mutation. FS_recovered = FS.durable.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL extension
/// ---------------------------------------------------------------------------

PROTOCOL extension {
    SATISFIES: layer1::extension
    CODE: kernel/src/fs/ext2/inode.rs:1238-1240
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:211-213

    LOCKS: none (self.extension is immutable)

    STEPS:
        1. RETURN &self.extension

    CRASH_WINDOWS: none
    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:
            self.extension is set at construction and never changes.
        POST.ret:
            Step 1 returns a reference to the immutable Extension field.
        FRAME:
            No mutation. FS' = FS.
        POST_ERR:
            Infallible; no error path.
        CRASH:
            No mutation. FS_recovered = FS.durable.
    }
}

/// =============================================================================
/// SECTION 2: MUTATOR PROTOCOLS (8 methods)
/// =============================================================================

/// ---------------------------------------------------------------------------
/// PROTOCOL set_mode
/// ---------------------------------------------------------------------------

PROTOCOL set_mode {
    SATISFIES: layer1::set_mode
    CODE: kernel/src/fs/ext2/inode.rs:280-286
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:75-77

    LOCKS: WRITE(self.inner)

    STEPS:
        1. EFFECT  fs = self.fs_arc()?                              -- may Err(EIO)
        2. LOCK    inner = self.inner.write()
        3. MUTATE  inner.desc.perm = FilePerm::from_bits_truncate(mode.bits())
        4. MUTATE  inner.desc.ctime = now()
        5. PERSIST inner.persist_inode_and_sync(&fs)?               -- W1 | W2
        6. UNLOCK  inner
        7. RETURN  Ok(())

    CRASH_WINDOWS:
        W1: crash before step 5 completes -- in-memory perm/ctime lost,
            on-disk state = FS.durable
        W2: crash during step 5 -- partial inode-table page write;
            fsck repairs from on-disk state

    ROLLBACK:
        None needed. persist_inode_and_sync is the only fallible step
        after mutation; on error the dirty flag remains set.

    SATISFIES_PROOF {
        PRE:
            Step 1 checks fs is alive. If dropped, returns Err(EIO)
            before any mutation, satisfying POST_ERR: FS' = FS.
            WRITE lock in step 2 guarantees exclusive access.
        POST.mode:
            Step 3 sets inner.desc.perm = mode.
            ABSTRACTION_MAP: C.inner[ino].desc.perm.bits() = FS'.inodes[I].mode.
            Therefore FS'.inodes[I].mode = new_mode.
        POST.ctime:
            Step 4 sets inner.desc.ctime = now() >= old ctime.
            Therefore FS'.inodes[I].ctime >= FS.inodes[I].ctime.
        FRAME:
            Only inner.desc.perm and inner.desc.ctime modified.
            All other fields of this inode, all other inodes, dirs,
            data, xattrs, sb unchanged.
        POST_ERR:
            Step 1 failure: no lock acquired, no mutation. FS' = FS.
            Step 5 failure: perm/ctime mutated in memory but persist
            failed. Dirty flag ensures eventual re-persist.
        CRASH:
            W1: crash before persist => FS_recovered = FS.durable.
            W2: crash after persist => FS_recovered includes new mode/ctime.
            Both are in layer1::set_mode CRASH set.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL set_owner
/// ---------------------------------------------------------------------------

PROTOCOL set_owner {
    SATISFIES: layer1::set_owner
    CODE: kernel/src/fs/ext2/inode.rs:292-298
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:83-85

    LOCKS: WRITE(self.inner)

    STEPS:
        1. EFFECT  fs = self.fs_arc()?
        2. LOCK    inner = self.inner.write()
        3. MUTATE  inner.desc.uid = uid.into()
        4. MUTATE  inner.desc.ctime = now()
        5. PERSIST inner.persist_inode_and_sync(&fs)?
        6. UNLOCK  inner
        7. RETURN  Ok(())

    CRASH_WINDOWS:
        W1: crash before step 5 -- uid/ctime lost, disk = FS.durable
        W2: crash during step 5 -- partial inode-table write

    ROLLBACK: None needed.

    SATISFIES_PROOF {
        PRE:
            Step 1 validates fs liveness. Err(EIO) if dropped.
        POST.uid:
            Step 3 sets inner.desc.uid = uid.
            ABSTRACTION_MAP: C.inner[ino].desc.uid = FS'.inodes[I].uid.
        POST.ctime:
            Step 4: now() >= old ctime.
        FRAME:
            Only inner.desc.uid and inner.desc.ctime modified.
        POST_ERR:
            Step 1 failure: no mutation. FS' = FS.
        CRASH:
            W1 => FS.durable. W2 => FS.durable with new uid/ctime.
            Both in layer1::set_owner CRASH set.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL set_group
/// ---------------------------------------------------------------------------

PROTOCOL set_group {
    SATISFIES: layer1::set_group
    CODE: kernel/src/fs/ext2/inode.rs:304-310
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:91-93

    LOCKS: WRITE(self.inner)

    STEPS:
        1. EFFECT  fs = self.fs_arc()?
        2. LOCK    inner = self.inner.write()
        3. MUTATE  inner.desc.gid = gid.into()
        4. MUTATE  inner.desc.ctime = now()
        5. PERSIST inner.persist_inode_and_sync(&fs)?
        6. UNLOCK  inner
        7. RETURN  Ok(())

    CRASH_WINDOWS:
        W1: crash before step 5 -- gid/ctime lost, disk = FS.durable
        W2: crash during step 5 -- partial inode-table write

    ROLLBACK: None needed.

    SATISFIES_PROOF {
        PRE:
            Step 1 validates fs liveness. Err(EIO) if dropped.
        POST.gid:
            Step 3 sets inner.desc.gid = gid.
            ABSTRACTION_MAP: C.inner[ino].desc.gid = FS'.inodes[I].gid.
        POST.ctime:
            Step 4: now() >= old ctime.
        FRAME:
            Only inner.desc.gid and inner.desc.ctime modified.
        POST_ERR:
            Step 1 failure: no mutation. FS' = FS.
        CRASH:
            W1 => FS.durable. W2 => FS.durable with new gid/ctime.
            Both in layer1::set_group CRASH set.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL set_atime
/// ---------------------------------------------------------------------------

PROTOCOL set_atime {
    SATISFIES: layer1::set_atime
    CODE: kernel/src/fs/ext2/inode.rs:316-318
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:99-101

    LOCKS: WRITE(self.inner)

    STEPS:
        1. LOCK    inner = self.inner.write()
        2. MUTATE  inner.desc.atime = time
        3. UNLOCK  inner

    CRASH_WINDOWS:
        W1: crash at any point -- no persist, atime reverts to FS.durable

    ROLLBACK: N/A (infallible, no persist)

    SATISFIES_PROOF {
        PRE:
            WRITE lock always acquirable. No precondition beyond existence.
        POST.atime:
            Step 2 sets inner.desc.atime = time.
            ABSTRACTION_MAP: C.inner[ino].desc.atime = FS'.inodes[I].atime.
            Therefore FS'.inodes[I].atime = new_time.
        FRAME:
            Only inner.desc.atime modified. All other fields unchanged.
        POST_ERR:
            Infallible; no error path.
        CRASH:
            No persist step. On crash, in-memory atime is lost.
            FS_recovered = FS.durable. Matches layer1::set_atime CRASH.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL set_mtime
/// ---------------------------------------------------------------------------

PROTOCOL set_mtime {
    SATISFIES: layer1::set_mtime
    CODE: kernel/src/fs/ext2/inode.rs:324-326
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:107-109

    LOCKS: WRITE(self.inner)

    STEPS:
        1. LOCK    inner = self.inner.write()
        2. MUTATE  inner.desc.mtime = time
        3. UNLOCK  inner

    CRASH_WINDOWS:
        W1: crash at any point -- no persist, mtime reverts to FS.durable

    ROLLBACK: N/A (infallible, no persist)

    SATISFIES_PROOF {
        PRE:
            WRITE lock always acquirable.
        POST.mtime:
            Step 2 sets inner.desc.mtime = time.
            ABSTRACTION_MAP: C.inner[ino].desc.mtime = FS'.inodes[I].mtime.
            Therefore FS'.inodes[I].mtime = new_time.
        FRAME:
            Only inner.desc.mtime modified. All other fields unchanged.
        POST_ERR:
            Infallible; no error path.
        CRASH:
            No persist step. On crash, in-memory mtime is lost.
            FS_recovered = FS.durable. Matches layer1::set_mtime CRASH.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL set_ctime
/// ---------------------------------------------------------------------------

PROTOCOL set_ctime {
    SATISFIES: layer1::set_ctime
    CODE: kernel/src/fs/ext2/inode.rs:332-334
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:115-117

    LOCKS: WRITE(self.inner)

    STEPS:
        1. LOCK    inner = self.inner.write()
        2. MUTATE  inner.desc.ctime = time
        3. UNLOCK  inner

    CRASH_WINDOWS:
        W1: crash at any point -- no persist, ctime reverts to FS.durable

    ROLLBACK: N/A (infallible, no persist)

    SATISFIES_PROOF {
        PRE:
            WRITE lock always acquirable.
        POST.ctime:
            Step 2 sets inner.desc.ctime = time.
            ABSTRACTION_MAP: C.inner[ino].desc.ctime = FS'.inodes[I].ctime.
            Therefore FS'.inodes[I].ctime = new_time.
        FRAME:
            Only inner.desc.ctime modified. All other fields unchanged.
        POST_ERR:
            Infallible; no error path.
        CRASH:
            No persist step. On crash, in-memory ctime is lost.
            FS_recovered = FS.durable. Matches layer1::set_ctime CRASH.
        NOTE:
            INV-09 exception: VFS may set arbitrary ctime value,
            so this protocol does not enforce monotonicity.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL resize_shrink
/// ---------------------------------------------------------------------------
/// Shrink path: upread lock for tail-zeroing, then upgrade to write for
/// truncation and persist.

PROTOCOL resize_shrink {
    SATISFIES: layer1::resize
    DISPATCH: new_size < old_size
    CODE: kernel/src/fs/ext2/inode.rs:137-211
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:55-57

    LOCKS: READ(self.inner) -> UPREAD(self.inner) -> WRITE(self.inner)

    STEPS:
        // --- Validation phase (read lock) ---
        1.  EFFECT  fs = self.fs_arc()?
        2.  EFFECT  block_size = fs.block_size()
        3.  GUARD   block_size != 0, else Err(EIO)
        4.  LOCK    inner_read = self.inner.read()
        5.  GUARD   type_ in {File, Dir, SymLink}, else Err(EINVAL)
        6.  GUARD   not (fast_symlink and size>0), else Err(EINVAL)
        7.  GUARD   not (APPEND_ONLY | IMMUTABLE), else Err(EPERM)
        8.  old_size = inner_read.desc.size
        9.  GUARD   new_size != old_size, else RETURN Ok(())
        10. UNLOCK  inner_read

        // --- Tail-zeroing phase (upread lock) ---
        11. LOCK    upread = self.inner.upread()
        12. current_size = upread.desc.size
        13. IF new_size < current_size AND new_size % block_size != 0:
              upread.page_cache.fill_zeros(new_size..align_up(new_size, block_size))?

        // --- Truncation phase (upgraded write lock) ---
        14. LOCK    inner = upread.upgrade()
        15. old_size = inner.desc.size                              -- recheck
        16. GUARD   new_size != old_size, else RETURN Ok(())        -- recheck
        17. IF new_size < old_size:
              old_aligned = align_up(old_size, block_size)
              new_aligned = align_up(new_size, block_size)
              IF new_aligned < old_aligned:
                inner.page_cache.discard_range(new_aligned..old_aligned)
              inner.page_cache.resize(new_aligned)?                 -- W1
              inner.desc.size = new_size
              inner.truncate_blocks(new_size)?                      -- W2
            ELSE (race: another thread grew between read and upgrade):
              inner.page_cache.resize(align_up(new_size, block_size))?
              inner.desc.size = new_size
        18. MUTATE  inner.desc.mtime = now()
        19. MUTATE  inner.desc.ctime = now()
        20. PERSIST inner.persist_inode_and_sync(&fs)?               -- W3
        21. UNLOCK  inner
        22. RETURN  Ok(())

    CRASH_WINDOWS:
        W1: crash after page_cache.resize but before truncate_blocks --
            page cache shrunk, but blocks not yet freed. Size not yet
            persisted. On reboot: FS.durable (old size, old blocks).
        W2: crash during truncate_blocks -- some blocks freed in bitmap,
            but block pointers not fully cleared. Orphan blocks possible.
            fsck reclaims orphan blocks and recomputes free counts.
        W3: crash before/during persist_inode_and_sync -- size/mtime/ctime
            not yet on disk. On reboot: FS.durable with possibly
            partially freed blocks (fsck repairs).

    ROLLBACK:
        No explicit rollback. truncate_blocks failures leave blocks
        allocated but unreachable (orphans). fsck reclaims them.
        page_cache.resize failure before size mutation is safe --
        size unchanged, no blocks freed.

    SATISFIES_PROOF {
        PRE:
            Steps 1-3 validate fs liveness and block_size > 0.
            Steps 5-7 validate type, fast-symlink, and flag guards.
            All guards return Err before any mutation => POST_ERR: FS' = FS.
            Step 9 handles no-op case (new_size == old_size).
            Steps 15-16 recheck under write lock for TOCTOU safety.
        POST.size:
            Step 17 sets inner.desc.size = new_size.
            ABSTRACTION_MAP: C.inner[ino].desc.size = FS'.inodes[I].size.
        POST.data:
            Step 13 zeros tail of last partial block.
            Step 17 discards pages beyond new_aligned and resizes page cache.
            truncate_blocks frees block pointers beyond new_size.
            Net effect: FS'.data[I] = FS.data[I][0..new_size].
        POST.blocks:
            truncate_blocks frees blocks for range [new_aligned, old_aligned).
            FS'.inodes[I].blocks <= FS.inodes[I].blocks.
            FS'.sb.free_blocks >= FS.sb.free_blocks.
        POST.timestamps:
            Steps 18-19 set mtime/ctime = now() >= old values.
        FRAME:
            Only size, blocks, mtime, ctime modified on this inode.
            sb.free_blocks may change. All other state unchanged.
        POST_ERR:
            Guards (steps 5-7) fail before mutation => FS' = FS.
            Step 1 failure (EIO) => no mutation.
            Step 3 failure (EIO) => no mutation.
        CRASH:
            W1/W2: reachable states are FS.durable or FS.durable with
            partially freed blocks. Both in layer1::resize CRASH set
            (shrink case, b in [blocks_for(new_size)..old_blocks]).
            W3: persist may or may not complete. If complete,
            FS_recovered has new size/timestamps. Both cases covered.
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL resize_grow
/// ---------------------------------------------------------------------------
/// Grow path: write lock, extend page cache, update size, persist.

PROTOCOL resize_grow {
    SATISFIES: layer1::resize
    DISPATCH: new_size > old_size
    CODE: kernel/src/fs/ext2/inode.rs:213-238
    VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:55-57

    LOCKS: READ(self.inner) -> WRITE(self.inner)

    STEPS:
        // --- Validation phase (read lock, same as shrink) ---
        1.  EFFECT  fs = self.fs_arc()?
        2.  EFFECT  block_size = fs.block_size()
        3.  GUARD   block_size != 0, else Err(EIO)
        4.  LOCK    inner_read = self.inner.read()
        5.  GUARD   type_ in {File, Dir, SymLink}, else Err(EINVAL)
        6.  GUARD   not (fast_symlink and size>0), else Err(EINVAL)
        7.  GUARD   not (APPEND_ONLY | IMMUTABLE), else Err(EPERM)
        8.  old_size = inner_read.desc.size
        9.  GUARD   new_size != old_size, else RETURN Ok(())
        10. UNLOCK  inner_read

        // --- Grow phase (write lock) ---
        11. LOCK    inner = self.inner.write()
        12. old_size = inner.desc.size                              -- recheck
        13. GUARD   new_size != old_size, else RETURN Ok(())        -- recheck
        14. inner.page_cache.resize(align_up(new_size, block_size))?  -- W1
        15. inner.desc.size = new_size
        16. MUTATE  inner.desc.mtime = now()
        17. MUTATE  inner.desc.ctime = now()
        18. PERSIST inner.persist_inode_and_sync(&fs)?               -- W2
        19. UNLOCK  inner
        20. RETURN  Ok(())

    CRASH_WINDOWS:
        W1: crash after page_cache.resize but before size mutation --
            page cache extended but desc.size unchanged. On reboot:
            FS.durable (old size). Extra pages are harmless (beyond size).
        W2: crash before/during persist -- size/mtime/ctime set in memory
            but not on disk. On reboot: FS.durable (old size).

    ROLLBACK:
        page_cache.resize failure (step 14) is the only fallible step
        before size mutation. On failure, no state has changed.
        persist failure (step 18): size/timestamps mutated in memory
        but not persisted. Dirty flag remains set for eventual re-persist.

    SATISFIES_PROOF {
        PRE:
            Steps 1-3 validate fs liveness and block_size > 0.
            Steps 5-7 validate type, fast-symlink, and flag guards.
            All guards return Err before any mutation => POST_ERR: FS' = FS.
            Steps 12-13 recheck under write lock for TOCTOU safety.
        POST.size:
            Step 15 sets inner.desc.size = new_size.
            ABSTRACTION_MAP: C.inner[ino].desc.size = FS'.inodes[I].size.
        POST.data:
            Step 14 extends page cache with zero-filled pages.
            FS'.data[I][0..old_size] = FS.data[I] (existing data preserved).
            FS'.data[I][old_size..new_size] = zeros (sparse extension).
        POST.timestamps:
            Steps 16-17 set mtime/ctime = now() >= old values.
        FRAME:
            Only size, mtime, ctime modified on this inode.
            blocks unchanged (grow is sparse, no new block allocation).
            All other inodes, dirs, xattrs, sb unchanged.
        POST_ERR:
            Guards (steps 5-7) fail before mutation => FS' = FS.
            Step 14 failure => no size mutation yet => FS' = FS.
        CRASH:
            W1: FS_recovered = FS.durable (page cache extension lost).
            W2: FS_recovered = FS.durable (size not persisted) or
                FS_recovered with new size (persist completed).
            Both in layer1::resize CRASH set (grow case).
    }
}

/// ---------------------------------------------------------------------------
/// PROTOCOL resize_noop
/// ---------------------------------------------------------------------------
/// No-op path: new_size == old_size, detected during validation.

PROTOCOL resize_noop {
    SATISFIES: layer1::resize
    DISPATCH: new_size = old_size
    CODE: kernel/src/fs/ext2/inode.rs:137-173

    LOCKS: READ(self.inner)

    STEPS:
        1. EFFECT  fs = self.fs_arc()?
        2. EFFECT  block_size = fs.block_size()
        3. GUARD   block_size != 0, else Err(EIO)
        4. LOCK    inner = self.inner.read()
        5. GUARD   type_ in {File, Dir, SymLink}, else Err(EINVAL)
        6. GUARD   not (fast_symlink and size>0), else Err(EINVAL)
        7. GUARD   not (APPEND_ONLY | IMMUTABLE), else Err(EPERM)
        8. old_size = inner.desc.size
        9. GUARD   new_size == old_size => RETURN Ok(())

    CRASH_WINDOWS: none (no mutation)
    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:
            Same validation as other resize variants.
        POST:
            Step 9 detects new_size == old_size and returns Ok(()).
            FS' = FS. Matches layer1::resize POST no-op case.
        FRAME:
            No mutation. All fields unchanged.
        POST_ERR:
            Guards may still fail (EINVAL, EPERM, EIO) => FS' = FS.
        CRASH:
            No mutation. FS_recovered = FS.durable.
    }
}
