// SPDX-License-Identifier: MPL-2.0
//
// Layer 1: Crash Hoare Logic -- Directory Operations
//
// 9 HOARE specs for directory namespace operations: lookup, readdir_at,
// create, mkdir, link, unlink, rmdir, rename, mknod.
//
// Pure abstract state only -- no locks, no page cache, no Dirty<>, no Rust types.
//
// Reference: 00-abstract-state.spec for AbstractFS model and notation.
//
// Notation:
//   FS       -- pre-state  (AbstractFS before operation)
//   FS'      -- post-state (AbstractFS after operation)
//   FS.durable -- last synced persistent state
//   {P} C {Q} | {R} -- Crash Hoare quadruple (P=pre, Q=post, R=crash)
//   D        -- the parent directory inode (identified by dir_ino)

/// =============================================================================
/// SECTION 1: PURE READS (lookup, readdir_at)
/// =============================================================================

/// ---------------------------------------------------------------------------
/// HOARE lookup(dir_ino, name)
/// ---------------------------------------------------------------------------
/// Resolves a name in a directory to the child inode.
/// Pure read -- no mutation of abstract state.
///
/// CODE: kernel/src/fs/ext2/inode.rs:815-822
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:153-155
/// LINUX_REF: fs/ext2/namei.c:56 (ext2_lookup)

HOARE lookup(dir_ino: Ino, name: Name) -> Result<Ino> {

    PRE:
        dir_ino in dom(FS.inodes)
        FS.inodes[dir_ino].alive
        FS.inodes[dir_ino].type_ = Dir
        name != "" AND |name| <= MAX_NAME_LEN

    POST (Ok(child_ino)):
        (child_ino, _) = FS.dirs[dir_ino][name]
        child_ino in dom(FS.inodes)
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        e in {ENOTDIR, ENOENT, EIO}
            // ENOTDIR: dir_ino is not a directory
            // ENOENT:  name not found in dir_ino's entries
            // EIO:     filesystem dead or I/O error
        FS' = FS

    CRASH:
        // Pure read; no in-flight mutation.
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/namei.c:56
}

/// ---------------------------------------------------------------------------
/// HOARE readdir_at(dir_ino, offset)
/// ---------------------------------------------------------------------------
/// Reads directory entries starting at byte offset. Returns the number of
/// entries successfully visited. Pure read -- no mutation.
///
/// CODE: kernel/src/fs/ext2/inode.rs:868-878
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:157-159
/// LINUX_REF: fs/ext2/dir.c:260 (ext2_readdir)

HOARE readdir_at(dir_ino: Ino, offset: usize) -> Result<usize> {

    PRE:
        dir_ino in dom(FS.inodes)
        FS.inodes[dir_ino].alive
        FS.inodes[dir_ino].type_ = Dir

    POST (Ok(n)):
        // n = number of entries visited starting from offset.
        // The entries returned are a subset of FS.dirs[dir_ino]
        // in on-disk order starting at byte position `offset`.
        n >= 0
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        e in {ENOTDIR, EIO}
            // ENOTDIR: dir_ino is not a directory
            // EIO:     filesystem dead or I/O error
        FS' = FS

    CRASH:
        // Pure read; no in-flight mutation.
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/dir.c:260
}

/// =============================================================================
/// SECTION 2: CREATION (create, mkdir)
/// =============================================================================

/// ---------------------------------------------------------------------------
/// HOARE create(dir_ino, name, type_, mode)
/// ---------------------------------------------------------------------------
/// Creates a new non-directory inode and links it into the parent directory.
/// Dispatches to mkdir when type_ = Dir (see separate spec).
///
/// CODE: kernel/src/fs/ext2/inode.rs:3452-3504
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:131-133
/// LINUX_REF: fs/ext2/namei.c:107 (ext2_create)

HOARE create(dir_ino: Ino, name: Name, type_: FileType, mode: u16)
    -> Result<Ino> {

    PRE:
        dir_ino in dom(FS.inodes)
        FS.inodes[dir_ino].alive
        FS.inodes[dir_ino].type_ = Dir
        name != "" AND |name| <= MAX_NAME_LEN
        name not in {".", ".."}
        type_ in {Reg, SymLink, CharDevice, BlockDevice, NamedPipe}
        name not in dom(FS.dirs[dir_ino])       -- no duplicate
        FS.sb.free_inodes > 0                    -- space available

    POST (Ok(new_ino)):
        // -- New inode allocated --
        new_ino not in dom(FS.inodes)            -- fresh
        new_ino in dom(FS'.inodes)

        FS'.inodes[new_ino].type_       = type_
        FS'.inodes[new_ino].mode        = mode
        FS'.inodes[new_ino].links_count = 1
        FS'.inodes[new_ino].alive       = true
        FS'.inodes[new_ino].size        = 0
        FS'.inodes[new_ino].ctime       = now()
        FS'.inodes[new_ino].mtime       = now()

        // -- Directory entry added --
        FS'.dirs[dir_ino][name] = (new_ino, type_)

        // -- Parent metadata updated --
        FS'.inodes[dir_ino].mtime = now()
        FS'.inodes[dir_ino].ctime = now()

        // -- Superblock counters --
        FS'.sb.free_inodes = FS.sb.free_inodes - 1

        // -- FRAME --
        FS'.inodes[dir_ino] \ {mtime, ctime} =
            FS.inodes[dir_ino] \ {mtime, ctime}
        forall name' != name:
            FS'.dirs[dir_ino][name'] = FS.dirs[dir_ino][name']
        forall j not in {dir_ino, new_ino}:
            FS'.inodes[j] = FS.inodes[j]
        FS'.data   = FS.data
        FS'.xattrs = FS.xattrs

    POST_ERR (Err(e)):
        e in {ENOTDIR, EINVAL, ENOSPC, EEXIST, EIO}
            // ENOTDIR: dir_ino is not a directory
            // EINVAL:  name or type_ invalid
            // ENOSPC:  no free inodes
            // EEXIST:  name already exists in directory
            // EIO:     filesystem dead or I/O error
        FS' = FS
            // Rollback: free_inode reclaims allocated inode on add_entry failure.

    CRASH:
        // Multi-step operation without journal. Possible intermediate states:
        FS_recovered in {
            FS.durable,
                // Crash before any persist.

            FS.durable with {
                // Inode allocated in bitmap but no directory entry.
                // Orphan inode: bitmap bit set, inode table written,
                // but parent directory unchanged.
                inodes[new_ino].alive = true,
                inodes[new_ino].links_count = 1,
                dirs[dir_ino] = FS.durable.dirs[dir_ino],  -- no entry
                // fsck: detects unreferenced inode, reclaims it.
            },

            FS'
                // Crash after all persists complete.
        }
        POST_FSCK: INV-01..INV-10 hold

    LINUX_REF: fs/ext2/namei.c:107
}

/// ---------------------------------------------------------------------------
/// HOARE mkdir(dir_ino, name, mode)
/// ---------------------------------------------------------------------------
/// Creates a new directory inode with "." and ".." entries, links it into
/// the parent. Parent links_count incremented for child's ".." reference.
///
/// CODE: kernel/src/fs/ext2/inode.rs:1050-1129
/// LINUX_REF: fs/ext2/namei.c:228 (ext2_mkdir)

HOARE mkdir(dir_ino: Ino, name: Name, mode: u16) -> Result<Ino> {

    PRE:
        dir_ino in dom(FS.inodes)
        FS.inodes[dir_ino].alive
        FS.inodes[dir_ino].type_ = Dir
        name != "" AND |name| <= MAX_NAME_LEN
        name not in {".", ".."}
        name not in dom(FS.dirs[dir_ino])
        FS.sb.free_inodes > 0
        FS.inodes[dir_ino].links_count < MAX_LINK_COUNT

    POST (Ok(new_ino)):
        // -- New directory inode allocated --
        new_ino not in dom(FS.inodes)
        new_ino in dom(FS'.inodes)

        FS'.inodes[new_ino].type_       = Dir
        FS'.inodes[new_ino].mode        = mode
        FS'.inodes[new_ino].links_count = 2      // "." + parent entry
        FS'.inodes[new_ino].alive       = true
        FS'.inodes[new_ino].size        = FS.sb.block_size
        FS'.inodes[new_ino].ctime       = now()
        FS'.inodes[new_ino].mtime       = now()

        // -- Child directory initialized with "." and ".." --
        FS'.dirs[new_ino]["."]  = (new_ino, Dir)
        FS'.dirs[new_ino][".."] = (dir_ino, Dir)
        child_count(FS', new_ino) = 0             // empty besides dots

        // -- Parent directory entry added --
        FS'.dirs[dir_ino][name] = (new_ino, Dir)

        // -- Parent links_count incremented (child's "..") --
        FS'.inodes[dir_ino].links_count = FS.inodes[dir_ino].links_count + 1
        FS'.inodes[dir_ino].mtime = now()
        FS'.inodes[dir_ino].ctime = now()

        // -- Superblock counters --
        FS'.sb.free_inodes = FS.sb.free_inodes - 1
        FS'.sb.free_blocks <= FS.sb.free_blocks   // one data block allocated

        // -- FRAME --
        forall name' != name:
            FS'.dirs[dir_ino][name'] = FS.dirs[dir_ino][name']
        forall j not in {dir_ino, new_ino}:
            FS'.inodes[j] = FS.inodes[j]
        FS'.data   = FS.data
        FS'.xattrs = FS.xattrs

    POST_ERR (Err(e)):
        e in {ENOTDIR, EINVAL, ENOSPC, EEXIST, EMLINK, EIO}
            // ENOTDIR: dir_ino is not a directory
            // EINVAL:  name invalid
            // ENOSPC:  no free inodes or blocks
            // EEXIST:  name already exists
            // EMLINK:  parent at MAX_LINK_COUNT
            // EIO:     filesystem dead or I/O error
        FS' = FS
            // Multi-step rollback: free child blocks, free inode,
            // restore parent links_count.

    CRASH:
        FS_recovered in {
            FS.durable,
                // Crash before any persist.

            FS.durable with {
                // Parent links_count incremented but child not yet allocated.
                inodes[dir_ino].links_count =
                    FS.durable.inodes[dir_ino].links_count + 1,
                // fsck: recomputes link count from directory entries.
            },

            FS.durable with {
                // Child inode allocated but no data block / no "."/"..".
                // Orphan inode in bitmap.
                inodes[new_ino].alive = true,
                inodes[new_ino].links_count = 2,
                dirs[new_ino] = {},
                dirs[dir_ino] = FS.durable.dirs[dir_ino],
                // fsck: reclaims orphan directory.
            },

            FS.durable with {
                // Child has "."/".."; parent has no entry yet.
                // Orphan directory with valid internal structure.
                inodes[new_ino].alive = true,
                dirs[new_ino] = {"." -> (new_ino, Dir), ".." -> (dir_ino, Dir)},
                dirs[dir_ino] = FS.durable.dirs[dir_ino],
                // fsck: reclaims orphan, adjusts link counts.
            },

            FS'
                // Crash after all persists complete.
        }
        POST_FSCK: INV-01..INV-10 hold

    LINUX_REF: fs/ext2/namei.c:228
}

/// =============================================================================
/// SECTION 3: LINK MANAGEMENT (link, unlink, rmdir)
/// =============================================================================

/// ---------------------------------------------------------------------------
/// HOARE link(dir_ino, child_ino, name)
/// ---------------------------------------------------------------------------
/// Creates a hard link in dir_ino pointing to an existing non-directory inode.
/// Increments child's links_count.
///
/// CODE: kernel/src/fs/ext2/inode.rs:3509-3558
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:161-166
/// LINUX_REF: fs/ext2/namei.c:204 (ext2_link)

HOARE link(dir_ino: Ino, child_ino: Ino, name: Name) -> Result<()> {

    PRE:
        dir_ino in dom(FS.inodes)
        child_ino in dom(FS.inodes)
        FS.inodes[dir_ino].alive
        FS.inodes[dir_ino].type_ = Dir
        FS.inodes[child_ino].alive
        FS.inodes[child_ino].type_ != Dir         -- no hard links to dirs
        FS.inodes[child_ino].links_count < MAX_LINK_COUNT
        name != "" AND |name| <= MAX_NAME_LEN
        name not in {".", ".."}
        name not in dom(FS.dirs[dir_ino])          -- no duplicate
        -- same filesystem (both inodes belong to same FS instance)

    POST (Ok(())):
        // -- Directory entry added --
        FS'.dirs[dir_ino][name] = (child_ino, FS.inodes[child_ino].type_)

        // -- Child link count incremented --
        FS'.inodes[child_ino].links_count = FS.inodes[child_ino].links_count + 1
        FS'.inodes[child_ino].ctime = now()

        // -- Parent metadata updated --
        FS'.inodes[dir_ino].mtime = now()
        FS'.inodes[dir_ino].ctime = now()

        // -- FRAME --
        FS'.inodes[child_ino] \ {links_count, ctime} =
            FS.inodes[child_ino] \ {links_count, ctime}
        FS'.inodes[dir_ino] \ {mtime, ctime} =
            FS.inodes[dir_ino] \ {mtime, ctime}
        forall name' != name:
            FS'.dirs[dir_ino][name'] = FS.dirs[dir_ino][name']
        forall j not in {dir_ino, child_ino}:
            FS'.inodes[j] = FS.inodes[j]
        FS'.data   = FS.data
        FS'.xattrs = FS.xattrs
        FS'.sb     = FS.sb

    POST_ERR (Err(e)):
        e in {ENOTDIR, EPERM, EOVERFLOW, EINVAL, EEXIST, EIO}
            // ENOTDIR:   dir_ino is not a directory
            // EPERM:     child_ino is a directory
            // EOVERFLOW: child at MAX_LINK_COUNT
            // EINVAL:    name invalid or cross-filesystem
            // EEXIST:    name already exists
            // EIO:       filesystem dead or I/O error
        FS' = FS
            // Rollback: links_count decremented on add_entry failure.

    CRASH:
        FS_recovered in {
            FS.durable,
                // Crash before any persist.

            FS.durable with {
                // Link count incremented in memory but not persisted,
                // and no directory entry written.
                // On reboot: old link count restored from disk. No leak.
            },

            FS.durable with {
                // Directory entry written but child link count not persisted.
                dirs[dir_ino][name] = (child_ino, type_),
                inodes[child_ino].links_count =
                    FS.durable.inodes[child_ino].links_count,
                // fsck: detects link count mismatch, repairs.
            },

            FS'
                // Crash after persist completes.
        }
        POST_FSCK: INV-01..INV-10 hold

    LINUX_REF: fs/ext2/namei.c:204
}

/// ---------------------------------------------------------------------------
/// HOARE unlink(dir_ino, name)
/// ---------------------------------------------------------------------------
/// Removes a non-directory entry from the parent directory. Decrements the
/// child's links_count; if it reaches zero, marks the child as freed.
///
/// CODE: kernel/src/fs/ext2/inode.rs:3563-3605
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:168-170
/// LINUX_REF: fs/ext2/namei.c:273 (ext2_unlink)

HOARE unlink(dir_ino: Ino, name: Name) -> Result<()> {

    PRE:
        dir_ino in dom(FS.inodes)
        FS.inodes[dir_ino].alive
        FS.inodes[dir_ino].type_ = Dir
        name != "" AND |name| <= MAX_NAME_LEN
        name not in {".", ".."}
        name in dom(FS.dirs[dir_ino])
        LET (child_ino, child_type) = FS.dirs[dir_ino][name]
        child_type != Dir                          -- use rmdir for directories

    POST (Ok(())):
        LET (child_ino, _) = FS.dirs[dir_ino][name]

        // -- Directory entry removed --
        name not in dom(FS'.dirs[dir_ino])

        // -- Child link count decremented --
        FS'.inodes[child_ino].links_count =
            FS.inodes[child_ino].links_count - 1
        FS'.inodes[child_ino].ctime = now()

        // -- Freed if last link --
        IF FS'.inodes[child_ino].links_count = 0:
            FS'.inodes[child_ino].alive = false
            FS'.inodes[child_ino].dtime > 0

        // -- Parent metadata updated --
        FS'.inodes[dir_ino].mtime = now()
        FS'.inodes[dir_ino].ctime = now()

        // -- FRAME --
        FS'.inodes[child_ino] \ {links_count, ctime, dtime, alive} =
            FS.inodes[child_ino] \ {links_count, ctime, dtime, alive}
        FS'.inodes[dir_ino] \ {mtime, ctime} =
            FS.inodes[dir_ino] \ {mtime, ctime}
        forall name' != name:
            FS'.dirs[dir_ino][name'] = FS.dirs[dir_ino][name']
        forall j not in {dir_ino, child_ino}:
            FS'.inodes[j] = FS.inodes[j]
        FS'.data   = FS.data
        FS'.xattrs = FS.xattrs
        FS'.sb     = FS.sb

    POST_ERR (Err(e)):
        e in {ENOTDIR, EINVAL, ENOENT, EISDIR, EIO}
            // ENOTDIR: dir_ino is not a directory
            // EINVAL:  name invalid or is "."/"..""
            // ENOENT:  name not found
            // EISDIR:  child is a directory (use rmdir)
            // EIO:     filesystem dead or I/O error
        FS' = FS

    CRASH:
        LET (child_ino, _) = FS.dirs[dir_ino][name]
        FS_recovered in {
            FS.durable,
                // Crash before any persist.

            FS.durable with {
                // Directory entry deleted but child link count not persisted.
                dirs[dir_ino] = FS.durable.dirs[dir_ino] \ {name},
                inodes[child_ino].links_count =
                    FS.durable.inodes[child_ino].links_count,
                // fsck: detects stale link count, repairs.
            },

            FS'
                // Crash after child persist completes.
        }
        POST_FSCK: INV-01..INV-10 hold

    LINUX_REF: fs/ext2/namei.c:273
}

/// ---------------------------------------------------------------------------
/// HOARE rmdir(dir_ino, name)
/// ---------------------------------------------------------------------------
/// Removes an empty subdirectory. Decrements child links by 2 ("." + parent
/// entry), marks child freed, decrements parent links by 1 (child's "..").
///
/// CODE: kernel/src/fs/ext2/inode.rs:999-1048
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:172-174
/// LINUX_REF: fs/ext2/namei.c:290 (ext2_rmdir)

HOARE rmdir(dir_ino: Ino, name: Name) -> Result<()> {

    PRE:
        dir_ino in dom(FS.inodes)
        FS.inodes[dir_ino].alive
        FS.inodes[dir_ino].type_ = Dir
        name != "" AND |name| <= MAX_NAME_LEN
        name not in {".", ".."}
        name in dom(FS.dirs[dir_ino])
        LET (child_ino, child_type) = FS.dirs[dir_ino][name]
        child_type = Dir
        is_empty_dir(FS, child_ino)                -- only "." and ".."

    POST (Ok(())):
        LET (child_ino, _) = FS.dirs[dir_ino][name]

        // -- Directory entry removed from parent --
        name not in dom(FS'.dirs[dir_ino])

        // -- Child fully freed --
        FS'.inodes[child_ino].links_count = 0
        FS'.inodes[child_ino].size        = 0
        FS'.inodes[child_ino].alive       = false
        FS'.inodes[child_ino].dtime       > 0

        // -- Parent links_count decremented (lost child's "..") --
        FS'.inodes[dir_ino].links_count =
            FS.inodes[dir_ino].links_count - 1
        FS'.inodes[dir_ino].mtime = now()
        FS'.inodes[dir_ino].ctime = now()

        // -- FRAME --
        FS'.inodes[dir_ino] \ {links_count, mtime, ctime} =
            FS.inodes[dir_ino] \ {links_count, mtime, ctime}
        forall name' != name:
            FS'.dirs[dir_ino][name'] = FS.dirs[dir_ino][name']
        forall j not in {dir_ino, child_ino}:
            FS'.inodes[j] = FS.inodes[j]
        FS'.data   = FS.data
        FS'.xattrs = FS.xattrs

    POST_ERR (Err(e)):
        e in {ENOTDIR, EINVAL, ENOENT, ENOTEMPTY, EIO}
            // ENOTDIR:    dir_ino or child is not a directory
            // EINVAL:     name is "." or ".."
            // ENOENT:     name not found
            // ENOTEMPTY:  child directory is not empty
            // EIO:        filesystem dead or I/O error
        FS' = FS

    CRASH:
        LET (child_ino, _) = FS.dirs[dir_ino][name]
        FS_recovered in {
            FS.durable,
                // Crash before any persist.

            FS.durable with {
                // Parent entry deleted, child not yet freed.
                // Orphan directory with links_count=2.
                dirs[dir_ino] = FS.durable.dirs[dir_ino] \ {name},
                inodes[child_ino] = FS.durable.inodes[child_ino],
                // fsck: detects unreferenced directory, reclaims.
            },

            FS.durable with {
                // Parent entry deleted, child freed, but parent
                // links_count not yet decremented.
                dirs[dir_ino] = FS.durable.dirs[dir_ino] \ {name},
                inodes[child_ino].alive = false,
                inodes[child_ino].links_count = 0,
                inodes[dir_ino].links_count =
                    FS.durable.inodes[dir_ino].links_count,
                // fsck: recomputes parent link count.
            },

            FS'
                // Crash after all persists complete.
        }
        POST_FSCK: INV-01..INV-10 hold

    LINUX_REF: fs/ext2/namei.c:290
}

/// =============================================================================
/// SECTION 4: RENAME
/// =============================================================================

/// ---------------------------------------------------------------------------
/// HOARE rename(src_dir, old_name, dst_dir, new_name)
/// ---------------------------------------------------------------------------
/// Atomically moves a directory entry from src_dir/old_name to
/// dst_dir/new_name. Handles same-dir and cross-dir cases uniformly
/// at the abstract level. If new_name already exists, it is replaced
/// (with type compatibility checks).
///
/// CODE: kernel/src/fs/ext2/inode.rs:3610-3855
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:176-181
/// LINUX_REF: fs/ext2/namei.c:318 (ext2_rename)

HOARE rename(src_dir: Ino, old_name: Name, dst_dir: Ino, new_name: Name)
    -> Result<()> {

    PRE:
        src_dir in dom(FS.inodes)
        dst_dir in dom(FS.inodes)
        FS.inodes[src_dir].alive
        FS.inodes[dst_dir].alive
        FS.inodes[src_dir].type_ = Dir
        FS.inodes[dst_dir].type_ = Dir
        old_name != "" AND |old_name| <= MAX_NAME_LEN
        new_name != "" AND |new_name| <= MAX_NAME_LEN
        old_name not in {".", ".."}
        new_name not in {".", ".."}
        old_name in dom(FS.dirs[src_dir])
        -- same filesystem
        -- not a no-op: NOT (src_dir = dst_dir AND old_name = new_name)

        // Type compatibility for replacement:
        LET (moved_ino, moved_type) = FS.dirs[src_dir][old_name]
        IF new_name in dom(FS.dirs[dst_dir]):
            LET (existing_ino, existing_type) = FS.dirs[dst_dir][new_name]
            (moved_type = Dir) = (existing_type = Dir)   -- dir<->dir only
            IF existing_type = Dir:
                is_empty_dir(FS, existing_ino)

    POST (Ok(())):
        LET (moved_ino, moved_type) = FS.dirs[src_dir][old_name]
        LET replacing = new_name in dom(FS.dirs[dst_dir])
        LET old_is_dir = (moved_type = Dir)

        // -- Source entry removed --
        old_name not in dom(FS'.dirs[src_dir])

        // -- Destination entry added/replaced --
        FS'.dirs[dst_dir][new_name] = (moved_ino, moved_type)

        // -- Moved inode ctime updated --
        FS'.inodes[moved_ino].ctime = now()

        // -- Replacement: existing inode links decremented --
        IF replacing:
            LET (existing_ino, existing_type) = FS.dirs[dst_dir][new_name]
            IF old_is_dir:
                // Dir replacing dir: -2 (parent entry + ".")
                FS'.inodes[existing_ino].links_count =
                    FS.inodes[existing_ino].links_count - 2
            ELSE:
                // Non-dir replacing non-dir: -1
                FS'.inodes[existing_ino].links_count =
                    FS.inodes[existing_ino].links_count - 1
            FS'.inodes[existing_ino].ctime = now()
            IF FS'.inodes[existing_ino].links_count = 0:
                FS'.inodes[existing_ino].alive = false
                FS'.inodes[existing_ino].dtime > 0

        // -- Cross-dir directory move: update ".." --
        IF old_is_dir AND src_dir != dst_dir:
            FS'.dirs[moved_ino][".."] = (dst_dir, Dir)
            FS'.inodes[src_dir].links_count =
                FS.inodes[src_dir].links_count - 1
            IF NOT replacing:
                FS'.inodes[dst_dir].links_count =
                    FS.inodes[dst_dir].links_count + 1
            IF replacing:
                // dst_dir link count unchanged (replaced dir's ".."
                // was already counted; new dir's ".." takes its place)
                FS'.inodes[dst_dir].links_count =
                    FS.inodes[dst_dir].links_count

        // -- Same-dir directory move: net link count change --
        IF old_is_dir AND src_dir = dst_dir:
            IF NOT replacing:
                // +1 (add_entry) then -1 (delete old) = net 0
                FS'.inodes[src_dir].links_count =
                    FS.inodes[src_dir].links_count
            IF replacing:
                // -1 (old_dir loses subdir reference)
                FS'.inodes[src_dir].links_count =
                    FS.inodes[src_dir].links_count - 1

        // -- Parent timestamps --
        FS'.inodes[src_dir].mtime = now()
        FS'.inodes[src_dir].ctime = now()
        IF src_dir != dst_dir:
            FS'.inodes[dst_dir].mtime = now()
            FS'.inodes[dst_dir].ctime = now()

        // -- FRAME --
        forall name' != old_name:
            FS'.dirs[src_dir][name'] = FS.dirs[src_dir][name']
        forall name' != new_name:
            FS'.dirs[dst_dir][name'] = FS.dirs[dst_dir][name']
        forall j not in {src_dir, dst_dir, moved_ino}
                    ++ (IF replacing THEN {existing_ino} ELSE {}):
            FS'.inodes[j] = FS.inodes[j]
        FS'.data   = FS.data
        FS'.xattrs = FS.xattrs

    POST_ERR (Err(e)):
        e in {ENOTDIR, EISDIR, ENOENT, ENOTEMPTY, EINVAL, EIO}
            // ENOTDIR:    src_dir or dst_dir not a directory;
            //             or dir replacing non-dir
            // EISDIR:     names are "."/".."; or non-dir replacing dir
            // ENOENT:     old_name not found in src_dir
            // ENOTEMPTY:  replacing a non-empty directory
            // EINVAL:     cross-filesystem
            // EIO:        filesystem dead, ".." mismatch, or I/O error
        FS' = FS

    CRASH:
        LET (moved_ino, moved_type) = FS.dirs[src_dir][old_name]
        LET old_is_dir = (moved_type = Dir)
        LET replacing = new_name in dom(FS.dirs[dst_dir])

        // Rename is a multi-step operation without journal.
        // Crash can leave the filesystem in several intermediate states.
        FS_recovered in {
            FS.durable,
                // Crash before any persist.

            FS.durable with {
                // New entry added/replaced in dst_dir, but old entry
                // not yet deleted from src_dir. Duplicate references.
                dirs[dst_dir][new_name] = (moved_ino, moved_type),
                dirs[src_dir][old_name] = (moved_ino, moved_type),
                // fsck: detects duplicate, repairs link counts.
            },

            FS.durable with {
                // Entry moved (old deleted, new added) but link counts
                // and ".." not yet updated.
                dirs[dst_dir][new_name] = (moved_ino, moved_type),
                old_name not in dirs[src_dir],
                // If old_is_dir: ".." still points to src_dir.
                // fsck: repairs ".." and link counts.
            },

            FS.durable with {
                // Entry moved, ".." updated, but parent link counts
                // not yet adjusted.
                dirs[dst_dir][new_name] = (moved_ino, moved_type),
                old_name not in dirs[src_dir],
                IF old_is_dir AND src_dir != dst_dir:
                    dirs[moved_ino][".."] = (dst_dir, Dir),
                // Parent link counts stale.
                // fsck: recomputes link counts from directory entries.
            },

            FS'
                // Crash after all persists complete.
        }
        POST_FSCK: INV-01..INV-10 hold

    LINUX_REF: fs/ext2/namei.c:318
}

/// =============================================================================
/// SECTION 5: SPECIAL FILE CREATION (mknod)
/// =============================================================================

/// ---------------------------------------------------------------------------
/// HOARE mknod(dir_ino, name, mode, dev)
/// ---------------------------------------------------------------------------
/// Creates a special file inode (char device, block device, or named pipe)
/// and links it into the parent directory. Delegates to create for inode
/// allocation, then encodes the device ID for device nodes.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/inode.rs:135-151
/// LINUX_REF: fs/ext2/namei.c:136 (ext2_mknod)

HOARE mknod(dir_ino: Ino, name: Name, mode: u16, dev: Option<DeviceId>)
    -> Result<Ino> {

    PRE:
        dir_ino in dom(FS.inodes)
        FS.inodes[dir_ino].alive
        FS.inodes[dir_ino].type_ = Dir
        name != "" AND |name| <= MAX_NAME_LEN
        name not in {".", ".."}
        name not in dom(FS.dirs[dir_ino])
        FS.sb.free_inodes > 0
        // dev.is_some() iff type_ in {CharDevice, BlockDevice}

    POST (Ok(new_ino)):
        LET type_ = IF dev matches CharDevice THEN CharDevice
                    ELSE IF dev matches BlockDevice THEN BlockDevice
                    ELSE NamedPipe

        // -- New inode allocated (inherits create postcondition) --
        new_ino not in dom(FS.inodes)
        new_ino in dom(FS'.inodes)

        FS'.inodes[new_ino].type_       = type_
        FS'.inodes[new_ino].mode        = mode
        FS'.inodes[new_ino].links_count = 1
        FS'.inodes[new_ino].alive       = true
        FS'.inodes[new_ino].size        = 0

        // -- Device ID encoded (for char/block devices) --
        // Abstract: device encoding stored in block_ptrs region.
        // Concrete: block_ptrs[0..2] encode major/minor per Linux convention.

        // -- Directory entry added --
        FS'.dirs[dir_ino][name] = (new_ino, type_)

        // -- Parent metadata updated --
        FS'.inodes[dir_ino].mtime = now()
        FS'.inodes[dir_ino].ctime = now()

        // -- Superblock counters --
        FS'.sb.free_inodes = FS.sb.free_inodes - 1

        // -- FRAME --
        FS'.inodes[dir_ino] \ {mtime, ctime} =
            FS.inodes[dir_ino] \ {mtime, ctime}
        forall name' != name:
            FS'.dirs[dir_ino][name'] = FS.dirs[dir_ino][name']
        forall j not in {dir_ino, new_ino}:
            FS'.inodes[j] = FS.inodes[j]
        FS'.data   = FS.data
        FS'.xattrs = FS.xattrs

    POST_ERR (Err(e)):
        e in {ENOTDIR, EINVAL, ENOSPC, EEXIST, EIO}
            // ENOTDIR: dir_ino is not a directory
            // EINVAL:  name invalid
            // ENOSPC:  no free inodes
            // EEXIST:  name already exists
            // EIO:     filesystem dead or I/O error
        FS' = FS

    CRASH:
        FS_recovered in {
            FS.durable,
                // Crash before any persist.

            FS.durable with {
                // Inode allocated but no directory entry.
                // Orphan inode; fsck reclaims.
                inodes[new_ino].alive = true,
                inodes[new_ino].links_count = 1,
                dirs[dir_ino] = FS.durable.dirs[dir_ino],
            },

            FS.durable with {
                // Inode created and linked, but device ID not yet encoded.
                // Special file exists with zero block_ptrs.
                dirs[dir_ino][name] = (new_ino, type_),
                inodes[new_ino].alive = true,
                // Device encoding absent; file appears as empty special node.
            },

            FS'
                // Crash after all persists complete.
        }
        POST_FSCK: INV-01..INV-10 hold

    LINUX_REF: fs/ext2/namei.c:136
}
