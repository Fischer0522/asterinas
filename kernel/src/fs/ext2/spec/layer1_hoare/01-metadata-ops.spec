// SPDX-License-Identifier: MPL-2.0
//
// Layer 1: Crash Hoare Logic — Metadata Operations
//
// 21 HOARE specs for inode metadata reads and mutators.
// Pure abstract state only — no locks, no page cache, no Dirty<>, no Rust types.
//
// Reference: 00-abstract-state.spec for AbstractFS model and notation.
//
// Notation:
//   FS       — pre-state  (AbstractFS before operation)
//   FS'      — post-state (AbstractFS after operation)
//   FS.durable — last synced persistent state
//   {P} C {Q} down {R} — Crash Hoare Logic quadruple
//   I        — the inode being operated on (identified by ino)

/// =============================================================================
/// SECTION 1: PURE READS (13 methods)
/// =============================================================================
///
/// All reads share these properties:
///   - PRE is top (always callable on a live inode)
///   - POST is pure: FS' = FS (no mutation)
///   - CRASH is trivial: FS_recovered = FS.durable (no in-flight mutation)

/// ---------------------------------------------------------------------------
/// HOARE ino
/// ---------------------------------------------------------------------------
/// Returns the inode number. Immutable field, no lock needed.

HOARE ino(I) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(ret)):
        ret = I
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        -- infallible; no error path

    CRASH:
        FS_recovered = FS.durable
        -- No in-flight mutation; crash is a no-op w.r.t. this operation.

    LINUX_REF: fs/ext2/inode.c (inode->i_ino)
}

/// ---------------------------------------------------------------------------
/// HOARE type_
/// ---------------------------------------------------------------------------
/// Returns the inode type. Immutable after creation.

HOARE type_(I) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(ret)):
        ret = FS.inodes[I].type_
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        -- infallible; no error path

    CRASH:
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/inode.c (inode->i_mode & S_IFMT)
}

/// ---------------------------------------------------------------------------
/// HOARE size
/// ---------------------------------------------------------------------------
/// Returns the file size in bytes.

HOARE size(I) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(ret)):
        ret = FS.inodes[I].size
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        -- infallible; no error path

    CRASH:
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/inode.c (inode->i_size)
}

/// ---------------------------------------------------------------------------
/// HOARE mode
/// ---------------------------------------------------------------------------
/// Returns the permission mode bits.

HOARE mode(I) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(ret)):
        ret = FS.inodes[I].mode
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        -- infallible in practice; Result wrapper for VFS trait uniformity
        FS' = FS

    CRASH:
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/inode.c (inode->i_mode & 0777)
}

/// ---------------------------------------------------------------------------
/// HOARE owner
/// ---------------------------------------------------------------------------
/// Returns the owner UID.

HOARE owner(I) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(ret)):
        ret = FS.inodes[I].uid
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        -- infallible in practice
        FS' = FS

    CRASH:
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/inode.c (inode->i_uid)
}

/// ---------------------------------------------------------------------------
/// HOARE group
/// ---------------------------------------------------------------------------
/// Returns the group GID.

HOARE group(I) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(ret)):
        ret = FS.inodes[I].gid
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        -- infallible in practice
        FS' = FS

    CRASH:
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/inode.c (inode->i_gid)
}

/// ---------------------------------------------------------------------------
/// HOARE atime
/// ---------------------------------------------------------------------------
/// Returns the last access time.

HOARE atime(I) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(ret)):
        ret = FS.inodes[I].atime
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        -- infallible; no error path

    CRASH:
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/inode.c (inode->i_atime)
}

/// ---------------------------------------------------------------------------
/// HOARE mtime
/// ---------------------------------------------------------------------------
/// Returns the last modification time.

HOARE mtime(I) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(ret)):
        ret = FS.inodes[I].mtime
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        -- infallible; no error path

    CRASH:
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/inode.c (inode->i_mtime)
}

/// ---------------------------------------------------------------------------
/// HOARE ctime
/// ---------------------------------------------------------------------------
/// Returns the last status change time.

HOARE ctime(I) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(ret)):
        ret = FS.inodes[I].ctime
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        -- infallible; no error path

    CRASH:
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/inode.c (inode->i_ctime)
}

/// ---------------------------------------------------------------------------
/// HOARE metadata
/// ---------------------------------------------------------------------------
/// Returns a consistent snapshot of all inode metadata fields.

HOARE metadata(I) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(ret)):
        ret.ino   = I
        ret.type_ = FS.inodes[I].type_
        ret.size  = FS.inodes[I].size
        ret.mode  = FS.inodes[I].mode
        ret.uid   = FS.inodes[I].uid
        ret.gid   = FS.inodes[I].gid
        ret.atime = FS.inodes[I].atime
        ret.mtime = FS.inodes[I].mtime
        ret.ctime = FS.inodes[I].ctime
        ret.nlinks = FS.inodes[I].links_count
        ret.blocks = FS.inodes[I].blocks
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        -- infallible; no error path

    CRASH:
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/inode.c:1493 (ext2_iget)
}

/// ---------------------------------------------------------------------------
/// HOARE page_cache
/// ---------------------------------------------------------------------------
/// Returns the backing VMO for this inode's data pages.
/// Every ext2 inode has a page cache (Reg, Dir, SymLink all use one).

HOARE page_cache(I) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(ret)):
        ret = Some(vmo)
        -- vmo backs FS.data[I] (for Reg/SymLink) or FS.dirs[I] (for Dir)
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        -- infallible; always returns Some

    CRASH:
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/inode.c (inode->i_mapping)
}

/// ---------------------------------------------------------------------------
/// HOARE open
/// ---------------------------------------------------------------------------
/// Ext2 does not provide custom FileIo. Always returns None.

HOARE open(I, access_mode, status_flags) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(ret)):
        ret = None
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        -- infallible; no error path

    CRASH:
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/file.c (ext2_file_operations — no custom open)
}

/// ---------------------------------------------------------------------------
/// HOARE fs
/// ---------------------------------------------------------------------------
/// Returns the owning filesystem instance.

HOARE fs(I) {
    PRE:
        I ∈ dom(FS.inodes)
        -- The filesystem must still be alive (not dropped).

    POST (Ok(ret)):
        ret = FS.sb  -- abstract: the filesystem containing I
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        -- Panics if filesystem is dropped; no Err path at abstract level.

    CRASH:
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/super.c (inode->i_sb)
}

/// ---------------------------------------------------------------------------
/// HOARE extension
/// ---------------------------------------------------------------------------
/// Returns the VFS extension (dentry cache). Immutable field.

HOARE extension(I) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(ret)):
        -- ret is the extension object; opaque at abstract level
        FS' = FS
        FRAME: all fields unchanged

    POST_ERR (Err(e)):
        -- infallible; no error path

    CRASH:
        FS_recovered = FS.durable

    LINUX_REF: N/A (Asterinas VFS extension, no Linux equivalent)
}

/// =============================================================================
/// SECTION 2: MUTATORS (8 methods)
/// =============================================================================
///
/// Mutators modify inode metadata fields. Two categories:
///   A. Persisting mutators (set_mode, set_owner, set_group):
///      Mutate field + ctime, then persist to inode table page cache.
///      Crash before persist loses the update.
///   B. Non-persisting mutators (set_atime, set_mtime, set_ctime):
///      Mutate field in memory only. Crash always loses the update.
///   C. Complex mutator (resize):
///      Multi-phase with block alloc/free and multiple crash windows.

/// ---------------------------------------------------------------------------
/// HOARE set_mode
/// ---------------------------------------------------------------------------
/// Sets the permission mode bits. Persists immediately.

HOARE set_mode(I, new_mode) {
    PRE:
        I ∈ dom(FS.inodes)
        FS.inodes[I].alive

    POST (Ok(())):
        FS'.inodes[I].mode = new_mode
        FS'.inodes[I].ctime ≥ FS.inodes[I].ctime
        FRAME: all fields of FS'.inodes[I] unchanged except {mode, ctime}
               all other inodes, dirs, data, xattrs, sb unchanged

    POST_ERR (Err(e)):
        FS' = FS
        e ∈ { EIO }
        -- EIO: filesystem dropped (fs_arc() fails)

    CRASH:
        FS_recovered ∈ {
            FS.durable,                          -- crash before persist
            FS.durable[I ↦ {mode=new_mode,       -- crash after persist
                            ctime=FS'.inodes[I].ctime,
                            ...rest from FS.durable}]
        }
        -- Partial inode-table write: fsck repairs from on-disk state.

    LINUX_REF: fs/ext2/inode.c:1589 (__ext2_write_inode)
}

/// ---------------------------------------------------------------------------
/// HOARE set_owner
/// ---------------------------------------------------------------------------
/// Sets the owner UID. Persists immediately.

HOARE set_owner(I, new_uid) {
    PRE:
        I ∈ dom(FS.inodes)
        FS.inodes[I].alive

    POST (Ok(())):
        FS'.inodes[I].uid = new_uid
        FS'.inodes[I].ctime ≥ FS.inodes[I].ctime
        FRAME: all fields of FS'.inodes[I] unchanged except {uid, ctime}
               all other inodes, dirs, data, xattrs, sb unchanged

    POST_ERR (Err(e)):
        FS' = FS
        e ∈ { EIO }

    CRASH:
        FS_recovered ∈ {
            FS.durable,                          -- crash before persist
            FS.durable[I ↦ {uid=new_uid,         -- crash after persist
                            ctime=FS'.inodes[I].ctime,
                            ...rest from FS.durable}]
        }

    LINUX_REF: fs/ext2/inode.c:1589 (__ext2_write_inode)
}

/// ---------------------------------------------------------------------------
/// HOARE set_group
/// ---------------------------------------------------------------------------
/// Sets the group GID. Persists immediately.

HOARE set_group(I, new_gid) {
    PRE:
        I ∈ dom(FS.inodes)
        FS.inodes[I].alive

    POST (Ok(())):
        FS'.inodes[I].gid = new_gid
        FS'.inodes[I].ctime ≥ FS.inodes[I].ctime
        FRAME: all fields of FS'.inodes[I] unchanged except {gid, ctime}
               all other inodes, dirs, data, xattrs, sb unchanged

    POST_ERR (Err(e)):
        FS' = FS
        e ∈ { EIO }

    CRASH:
        FS_recovered ∈ {
            FS.durable,                          -- crash before persist
            FS.durable[I ↦ {gid=new_gid,         -- crash after persist
                            ctime=FS'.inodes[I].ctime,
                            ...rest from FS.durable}]
        }

    LINUX_REF: fs/ext2/inode.c:1589 (__ext2_write_inode)
}

/// ---------------------------------------------------------------------------
/// HOARE set_atime
/// ---------------------------------------------------------------------------
/// Sets the access time. In-memory only, no persist.

HOARE set_atime(I, new_time) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(())):
        FS'.inodes[I].atime = new_time
        FRAME: all fields of FS'.inodes[I] unchanged except {atime}
               all other inodes, dirs, data, xattrs, sb unchanged

    POST_ERR (Err(e)):
        -- infallible; no error path

    CRASH:
        FS_recovered = FS.durable
        -- No persist step; crash always reverts atime to durable value.
        -- Matches Linux lazytime semantics.

    LINUX_REF: fs/ext2/inode.c (generic atime update)
}

/// ---------------------------------------------------------------------------
/// HOARE set_mtime
/// ---------------------------------------------------------------------------
/// Sets the modification time. In-memory only, no persist.

HOARE set_mtime(I, new_time) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(())):
        FS'.inodes[I].mtime = new_time
        FRAME: all fields of FS'.inodes[I] unchanged except {mtime}
               all other inodes, dirs, data, xattrs, sb unchanged

    POST_ERR (Err(e)):
        -- infallible; no error path

    CRASH:
        FS_recovered = FS.durable
        -- No persist step; crash always reverts mtime to durable value.

    LINUX_REF: fs/ext2/inode.c (generic mtime update)
}

/// ---------------------------------------------------------------------------
/// HOARE set_ctime
/// ---------------------------------------------------------------------------
/// Sets the status change time. In-memory only, no persist.

HOARE set_ctime(I, new_time) {
    PRE:
        I ∈ dom(FS.inodes)

    POST (Ok(())):
        FS'.inodes[I].ctime = new_time
        FRAME: all fields of FS'.inodes[I] unchanged except {ctime}
               all other inodes, dirs, data, xattrs, sb unchanged

    POST_ERR (Err(e)):
        -- infallible; no error path

    CRASH:
        FS_recovered = FS.durable
        -- No persist step; crash always reverts ctime to durable value.

    LINUX_REF: fs/ext2/inode.c (generic ctime update)
    NOTE: INV-09 (ctime_monotonicity) exception -- VFS may set arbitrary value.
}

/// ---------------------------------------------------------------------------
/// HOARE resize
/// ---------------------------------------------------------------------------
/// Resizes the file (truncate or extend). Most complex metadata mutator.
///
/// Shrink: zeros tail of last partial block, discards pages beyond new size,
///         frees blocks, updates size/mtime/ctime, persists.
/// Grow:   extends page cache, updates size/mtime/ctime, persists.
///         New region reads as zeros (sparse).

HOARE resize(I, new_size) {
    PRE:
        I ∈ dom(FS.inodes)
        FS.inodes[I].alive
        FS.inodes[I].type_ ∈ { Reg, Dir, SymLink }
        -- Fast symlinks with existing content cannot be resized:
        ¬(is_fast_symlink(FS, I) ∧ FS.inodes[I].size > 0)

    POST (Ok(())):
        let old_size = FS.inodes[I].size

        -- Size updated:
        FS'.inodes[I].size = new_size

        -- Timestamps updated (only if size actually changed):
        IF new_size ≠ old_size:
            FS'.inodes[I].mtime ≥ FS.inodes[I].mtime
            FS'.inodes[I].ctime ≥ FS.inodes[I].ctime

        -- Shrink: data truncated, freed blocks returned
        IF new_size < old_size:
            FS'.data[I] = FS.data[I][0..new_size]
            FS'.inodes[I].blocks ≤ FS.inodes[I].blocks
            FS'.sb.free_blocks ≥ FS.sb.free_blocks

        -- Grow: data extended with zeros
        IF new_size > old_size:
            FS'.data[I][0..old_size] = FS.data[I][0..old_size]
            FS'.data[I][old_size..new_size] = zeros

        -- No-op if sizes equal:
        IF new_size = old_size:
            FS' = FS

        FRAME: all fields of FS'.inodes[I] unchanged except
                   {size, blocks, mtime, ctime}
               all other inodes, dirs, xattrs unchanged
               FS'.sb.free_blocks may change (shrink frees blocks)

    POST_ERR (Err(e)):
        FS' = FS
        e ∈ { EINVAL, EPERM, EIO }
        -- EINVAL: type not in {Reg, Dir, SymLink}, or fast symlink with size>0
        -- EPERM:  APPEND_ONLY or IMMUTABLE flags set
        -- EIO:    filesystem dropped or block_size=0

    CRASH:
        let old_size = FS.inodes[I].size
        let bs = FS.sb.block_size

        -- Shrink crash windows:
        IF new_size < old_size:
            FS_recovered ∈ {
                FS.durable,
                    -- W1: crash before persist. All in-memory changes lost.

                FS.durable[I ↦ {size=new_size, mtime=t, ctime=t,
                                blocks=b, ...rest from FS.durable}]
                    WHERE b ∈ [blocks_for(new_size, bs)
                               .. FS.durable.inodes[I].blocks]
                    -- W2: crash during/after truncate_blocks but before
                    --     full persist. Some blocks freed, some orphaned.
                    --     fsck reclaims orphan blocks and recomputes counts.
            }

        -- Grow crash windows:
        IF new_size > old_size:
            FS_recovered ∈ {
                FS.durable,
                    -- W1: crash before persist. Page cache extension lost.

                FS.durable[I ↦ {size=new_size, mtime=t, ctime=t,
                                ...rest from FS.durable}]
                    -- W2: crash after persist. New size committed.
                    --     Extended region reads as zeros (sparse).
            }

    LINUX_REF: fs/ext2/inode.c:1275 (ext2_setsize)
}
