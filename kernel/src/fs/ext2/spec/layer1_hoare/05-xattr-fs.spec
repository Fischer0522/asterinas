// SPDX-License-Identifier: MPL-2.0
//
// Layer 1: Crash Hoare Logic -- Extended Attributes & FileSystem Trait
//
// Pure abstract specs for xattr operations (get, set, list, remove)
// and FileSystem trait methods (name, sync, root_inode, sb, event_stats).
//
// No locks, no page cache, no Rust types. References only AbstractFS
// from 00-abstract-state.spec.
//
// Notation:
//   FS       -- pre-state
//   FS'      -- post-state
//   FS.durable -- last synced persistent state
//   {P} C {Q} down {R} -- Crash Hoare Logic quadruple

/// =============================================================================
/// SECTION 1: EXTENDED ATTRIBUTE OPERATIONS
/// =============================================================================

/// ---------------------------------------------------------------------------
/// HOARE 1: get_xattr
/// ---------------------------------------------------------------------------
/// Reads one extended-attribute value into a caller-supplied buffer.
///
/// CODE: kernel/src/fs/ext2/inode.rs:339-349
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:225-228
/// LINUX_REF: fs/ext2/xattr.c:195-275 (ext2_xattr_get)

HOARE get_xattr(ino, name, buf) {
    PRE:
        ino ∈ dom(FS.inodes)
        FS.inodes[ino].alive
        FS.inodes[ino].type_ ∈ {Dir, Reg}

    POST (Ok(size)):
        -- Pure read: no state change
        FS' = FS
        name ∈ dom(FS.xattrs[ino])
        buf[0..size] = FS.xattrs[ino][name]
        size = |FS.xattrs[ino][name]|
        FRAME: everything unchanged

    POST_ERR (Err(e)):
        FS' = FS
        e ∈ {
            EOPNOTSUPP  -- inode type does not support xattrs
            ENODATA     -- attribute `name` not found in FS.xattrs[ino]
            ERANGE      -- |buf| < |FS.xattrs[ino][name]|
        }

    CRASH:
        FS_recovered = FS.durable
        -- Pure read; no writes to recover.
}

/// ---------------------------------------------------------------------------
/// HOARE 2: set_xattr
/// ---------------------------------------------------------------------------
/// Creates or replaces one extended attribute.
///
/// CODE: kernel/src/fs/ext2/inode.rs:373-396
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:215-223
/// LINUX_REF: fs/ext2/xattr.c:405-651 (ext2_xattr_set)

HOARE set_xattr(ino, name, value, flags) {
    PRE:
        ino ∈ dom(FS.inodes)
        FS.inodes[ino].alive
        FS.inodes[ino].type_ ∈ {Dir, Reg}

    POST (Ok(())):
        -- Xattr entry created or replaced
        FS'.xattrs[ino][name] = value

        -- Inode file_acl updated to new xattr block
        FS'.inodes[ino].file_acl = new_bid   -- may differ from FS if block allocated

        -- Timestamp update
        FS'.inodes[ino].ctime ≥ FS.inodes[ino].ctime

        FRAME: FS'.inodes[j] = FS.inodes[j]  for j ≠ ino
               FS'.xattrs[j] = FS.xattrs[j]  for j ≠ ino
               FS'.dirs = FS.dirs
               FS'.data = FS.data

    POST_ERR (Err(e)):
        FS' = FS
        e ∈ {
            EOPNOTSUPP  -- inode type does not support xattrs
            EEXIST      -- flags = CREATE ∧ name ∈ dom(FS.xattrs[ino])
            ENODATA     -- flags = REPLACE ∧ name ∉ dom(FS.xattrs[ino])
            ENOSPC      -- no free block for xattr data
        }

    CRASH:
        -- Two-phase write: xattr block then inode descriptor.
        -- Crash may land between the two phases.
        FS_recovered ∈ {FS.durable, FS'.durable_projection}
        -- Where FS'.durable_projection is FS' after the next sync.
        -- Intermediate: xattr block written but inode file_acl not updated
        --   => orphan xattr block, reclaimable by fsck.
}

/// ---------------------------------------------------------------------------
/// HOARE 3: list_xattr
/// ---------------------------------------------------------------------------
/// Lists extended-attribute names in one namespace into a caller buffer.
///
/// CODE: kernel/src/fs/ext2/inode.rs:354-368
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:230-233
/// LINUX_REF: fs/ext2/xattr.c:287-364 (ext2_xattr_list)

HOARE list_xattr(ino, namespace, buf) {
    PRE:
        ino ∈ dom(FS.inodes)
        FS.inodes[ino].alive
        FS.inodes[ino].type_ ∈ {Dir, Reg}

    POST (Ok(total_size)):
        -- Pure read: no state change
        FS' = FS
        -- buf contains null-separated xattr names in `namespace`
        let names = { n : (n, _) ∈ FS.xattrs[ino] ∧ n.namespace = namespace }
        buf[0..total_size] = null_join(names)
        total_size = |null_join(names)|
        FRAME: everything unchanged

    POST_ERR (Err(e)):
        FS' = FS
        e ∈ {
            EOPNOTSUPP  -- inode type does not support xattrs
            ERANGE      -- |buf| < total_size needed
        }

    CRASH:
        FS_recovered = FS.durable
        -- Pure read; no writes to recover.
}

/// ---------------------------------------------------------------------------
/// HOARE 4: remove_xattr
/// ---------------------------------------------------------------------------
/// Removes one extended attribute from an inode.
///
/// CODE: kernel/src/fs/ext2/inode.rs:401-418
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:235-238
/// LINUX_REF: fs/ext2/xattr.c:405-651 (ext2_xattr_set with value == NULL)

HOARE remove_xattr(ino, name) {
    PRE:
        ino ∈ dom(FS.inodes)
        FS.inodes[ino].alive
        FS.inodes[ino].type_ ∈ {Dir, Reg}

    POST (Ok(())):
        -- Attribute removed
        FS'.xattrs[ino] = FS.xattrs[ino] \ {name}

        -- Inode file_acl updated (may become 0 if block freed)
        FS'.inodes[ino].file_acl = new_bid

        FRAME: FS'.inodes[j] = FS.inodes[j]  for j ≠ ino
               FS'.xattrs[j] = FS.xattrs[j]  for j ≠ ino
               FS'.dirs = FS.dirs
               FS'.data = FS.data

    POST_ERR (Err(e)):
        FS' = FS
        e ∈ {
            EOPNOTSUPP  -- inode type does not support xattrs
            ENODATA     -- attribute `name` not found in FS.xattrs[ino]
        }

    CRASH:
        -- Two-phase write: xattr block then inode descriptor.
        FS_recovered ∈ {FS.durable, FS'.durable_projection}
        -- Intermediate: xattr block modified but inode file_acl stale
        --   => orphan or dangling xattr block, reclaimable by fsck.
}

/// =============================================================================
/// SECTION 2: FILESYSTEM TRAIT OPERATIONS
/// =============================================================================

/// ---------------------------------------------------------------------------
/// HOARE 5: fs_name
/// ---------------------------------------------------------------------------
/// Returns the filesystem type name string.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:12-15
/// LINUX_REF: fs/ext2/super.c:1698 (ext2_fs_type)

HOARE fs_name() {
    PRE:
        true   -- no precondition

    POST (ret):
        FS' = FS
        ret = "ext2"
        FRAME: everything unchanged

    CRASH:
        FS_recovered = FS.durable
        -- Pure accessor; no writes.
}

/// ---------------------------------------------------------------------------
/// HOARE 6: fs_sync
/// ---------------------------------------------------------------------------
/// Full filesystem sync: flushes all inodes, metadata, and device cache.
/// Establishes a new durable checkpoint.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:17-23
/// LINUX_REF: fs/ext2/super.c:1308 (ext2_sync_fs)

HOARE fs_sync() {
    PRE:
        true   -- always callable

    POST (Ok(())):
        -- All in-memory state becomes durable
        FS'.durable = FS'

        -- Superblock free counters recomputed from group descriptors
        FS'.sb.free_blocks = actual_free_blocks(FS')
        FS'.sb.free_inodes = actual_free_inodes(FS')

        FRAME: FS'.inodes = FS.inodes
               FS'.dirs   = FS.dirs
               FS'.data   = FS.data
               FS'.xattrs = FS.xattrs
               -- Only sb counters and durable snapshot change

    POST_ERR (Err(e)):
        -- Partial sync may have occurred; some inodes durable, others not.
        -- Abstract model: FS' = FS (best-effort; no partial visibility)
        FS' = FS
        e ∈ { EIO }

    CRASH:
        -- Crash during sync: partial flush possible.
        -- Some inodes synced, metadata partially written.
        FS_recovered ∈ { FS_partial :
            FS.durable ⊑ FS_partial ⊑ FS'
            -- FS_partial is "between" old durable and fully synced state.
            -- fsck repairs structural inconsistencies.
        }
}

/// ---------------------------------------------------------------------------
/// HOARE 7: fs_root_inode
/// ---------------------------------------------------------------------------
/// Returns the cached root inode (ino = ROOT_INO = 2).
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:25-28
/// LINUX_REF: fs/ext2/super.c:877 (ext2_fill_super root inode setup)

HOARE fs_root_inode() {
    PRE:
        true   -- always callable; root inode is immutable after mount

    POST (ret):
        FS' = FS
        ret.ino = ROOT_INO
        FS.inodes[ROOT_INO].type_ = Dir
        FS.inodes[ROOT_INO].alive
        FRAME: everything unchanged

    CRASH:
        FS_recovered = FS.durable
        -- Pure accessor; no writes.
}

/// ---------------------------------------------------------------------------
/// HOARE 8: fs_sb
/// ---------------------------------------------------------------------------
/// Returns a snapshot of the superblock as a VFS SuperBlock struct.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:30-48
/// LINUX_REF: fs/ext2/super.c:1446 (ext2_statfs)

HOARE fs_sb() {
    PRE:
        true   -- always callable

    POST (ret):
        FS' = FS
        ret.magic   = MAGIC_NUM
        ret.bsize   = FS.sb.block_size
        ret.blocks  = FS.sb.total_blocks
        ret.bfree   = FS.sb.free_blocks
        ret.bavail  = saturating_sub(FS.sb.free_blocks, reserved_blocks)
        ret.files   = FS.sb.total_inodes
        ret.ffree   = FS.sb.free_inodes
        ret.namelen = MAX_NAME_LEN
        FRAME: everything unchanged

    CRASH:
        FS_recovered = FS.durable
        -- Pure read under SB_READ lock; no writes.
}

/// ---------------------------------------------------------------------------
/// HOARE 9: fs_event_subscriber_stats
/// ---------------------------------------------------------------------------
/// Returns the filesystem event subscriber statistics.
/// Implementation detail; abstract spec is minimal.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:50-53

HOARE fs_event_subscriber_stats() {
    PRE:
        true   -- always callable; field is immutable after mount

    POST (ret):
        FS' = FS
        -- ret is the FsEventSubscriberStats reference (opaque)
        FRAME: everything unchanged

    CRASH:
        FS_recovered = FS.durable
        -- Pure accessor; no writes.
}
