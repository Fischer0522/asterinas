// SPDX-License-Identifier: MPL-2.0
//
// Layer 2: Implementation Protocol — Concrete State Model
//
// Maps the abstract state (Layer 1) to Asterinas Ext2 implementation details:
// locks, Dirty<> wrappers, page cache, RwMutex modes, persist helpers.
//
// Every Layer 2 PROTOCOL references this model for lock sequences,
// crash windows, and rollback mechanisms.
//
// Reference: Layer 1 model at layer1_hoare/00-abstract-state.spec

/// =============================================================================
/// SECTION 1: CONCRETE STATE MODEL
/// =============================================================================

ConcreteState = {
    // Per-inode state
    inode: {
        ino         : u32,                      // immutable
        type_       : InodeType,                // immutable
        block_group : usize,                    // immutable
        inner       : RwMutex<InodeInner>,      // main data lock
        xattr       : Option<RwMutex<Xattr>>,   // xattr lock (Dir|File only)
        fs          : Weak<Ext2>,               // back-pointer
        extension   : Extension,                // VFS dentry cache
    },

    // InodeInner (guarded by inode.inner)
    inner: {
        desc        : Dirty<InodeDesc>,         // on-disk descriptor
        is_freed    : bool,
        page_cache  : PageCache,                // data pages (Reg/Dir/SymLink)
    },

    // Filesystem-level state
    fs: {
        super_block   : RwMutex<Dirty<SuperBlock>>,
        block_groups  : Vec<BlockGroup>,
        inode_cache   : BTreeMap<u32, Arc<Inode>>,
        block_device  : Arc<dyn BlockDevice>,
        root_inode    : Arc<Inode>,
        self_ref      : Weak<Ext2>,
    },
}

/// CODE: kernel/src/fs/ext2/inode.rs:46-54 (Inode struct)
/// CODE: kernel/src/fs/ext2/inode.rs:56-64 (InodeInner struct)
/// CODE: kernel/src/fs/ext2/fs.rs:26-51 (Ext2 struct)

/// =============================================================================
/// SECTION 2: LOCK MODEL
/// =============================================================================
///
/// Each inode has a single RwMutex<InodeInner> with three acquisition modes:
///
///   READ    — inner.read()    — shared, concurrent reads allowed
///   UPREAD  — inner.upread()  — shared read, upgradable to WRITE atomically
///   WRITE   — inner.write()   — exclusive, blocks all other access
///
/// Transitions:
///   UPREAD → WRITE   (via upread.upgrade())
///   WRITE  → UPREAD  (via write.downgrade())
///
/// Xattr has a separate RwMutex<Xattr>:
///   XATTR_READ  — xattr.read()
///   XATTR_WRITE — xattr.write()
///
/// Filesystem-level locks:
///   SB_READ  — super_block.read()
///   SB_WRITE — super_block.write()

LOCK_ORDER {
    // Global ordering (must be respected to prevent deadlock):
    //   L1: inode.xattr       (per-inode)
    //   L2: inode.inner       (per-inode, ascending ino for multi-inode)
    //   L3: fs.super_block    (global)
    //   L4: block_group.desc  (per-group)
    //   L5: block_group.block_bitmap  (per-group)
    //   L6: block_group.inode_bitmap  (per-group)
    //   L7: block_group.inode_cache   (per-group)
    //
    // Two-inode rule: acquire L2[min(ino_a, ino_b)] before L2[max(ino_a, ino_b)]
    // Xattr rule: acquire L1[i] before L2[i] (xattr before inner)
}

/// =============================================================================
/// SECTION 3: PERSIST HELPERS
/// =============================================================================

PERSIST_MODEL {
    // persist_inode_and_sync(inner, fs):
    //   1. Serialize InodeDesc → RawInode bytes
    //   2. Write RawInode to group's inode-table PageCache
    //   3. Mark desc as clean (Dirty::clean())
    //   NOTE: Does NOT flush to block device. Data reaches disk
    //         only after sync_all / FileSystem::sync.
    //
    // CODE: kernel/src/fs/ext2/inode.rs (InodeInner::persist_inode_and_sync)

    // sync_metadata(fs):
    //   1. Recompute sb.free_blocks from group descriptors
    //   2. Recompute sb.free_inodes from group descriptors
    //   3. Write superblock + group descriptors to device
    //
    // CODE: kernel/src/fs/ext2/fs.rs:753-760
}

/// =============================================================================
/// SECTION 4: ABSTRACTION MAPPING (Concrete → Abstract)
/// =============================================================================
///
/// Maps ConcreteState to AbstractFS for SATISFIES_PROOF obligations.

ABSTRACTION_MAP(C: ConcreteState) → AbstractFS {
    inodes[ino] = {
        type_       = C.inode[ino].type_,
        mode        = C.inner[ino].desc.perm.bits(),
        uid         = C.inner[ino].desc.uid,
        gid         = C.inner[ino].desc.gid,
        size        = C.inner[ino].desc.size,
        links_count = C.inner[ino].desc.links_count,
        blocks      = C.inner[ino].desc.blocks,
        atime       = C.inner[ino].desc.atime,
        mtime       = C.inner[ino].desc.mtime,
        ctime       = C.inner[ino].desc.ctime,
        dtime       = C.inner[ino].desc.dtime,
        alive       = ¬C.inner[ino].is_freed,
        file_acl    = C.inner[ino].desc.file_acl,
    },

    dirs[ino] = parse_dir_entries(C.inner[ino].page_cache)
        where C.inode[ino].type_ = Dir,

    data[ino] = C.inner[ino].page_cache.contents()[0..C.inner[ino].desc.size]
        where C.inode[ino].type_ ∈ {Reg, SymLink},

    xattrs[ino] = C.xattr[ino].entries()
        where C.inode[ino].xattr.is_some(),

    sb = {
        total_blocks   = C.fs.super_block.blocks_count,
        free_blocks    = C.fs.super_block.free_blocks_count,
        total_inodes   = C.fs.super_block.inodes_count,
        free_inodes    = C.fs.super_block.free_inodes_count,
        block_size     = 1024 << C.fs.super_block.log_block_size,
        blocks_per_group = C.fs.super_block.blocks_per_group,
        inodes_per_group = C.fs.super_block.inodes_per_group,
    },

    durable = ABSTRACTION_MAP(on_disk_state(C.fs.block_device)),
}

/// =============================================================================
/// SECTION 5: PROTOCOL TEMPLATE
/// =============================================================================
///
/// Every Layer 2 spec follows this format:
///
///   PROTOCOL method_name_variant {
///       SATISFIES: layer1::method_name
///       DISPATCH: condition for this variant (e.g., ¬O_DIRECT)
///       CODE: file:lines
///
///       LOCKS: acquisition sequence (e.g., WRITE → UPREAD → WRITE)
///       STEPS: numbered implementation steps
///       CRASH_WINDOWS: W1, W2, ... (points where crash leaves partial state)
///       ROLLBACK: cleanup mechanism on error
///
///       SATISFIES_PROOF {
///           PRE:       PROTOCOL.REQUIRE ⟹ HOARE.PRE
///           POST.field: step N ⟹ HOARE.POST.field
///           FRAME:     only self.inner modified ⟹ HOARE.FRAME
///           POST_ERR:  rollback ⟹ FS' = FS
///           CRASH:     ∀ Wi: reachable(Wi) ⊆ HOARE.CRASH
///       }
///   }
///
/// Rules:
///   1. Every PROTOCOL must name exactly one SATISFIES target in Layer 1
///   2. DISPATCH conditions across variants must be exhaustive
///   3. SATISFIES_PROOF must address every clause in the Layer 1 spec
///   4. CRASH_WINDOWS must be a subset of the Layer 1 CRASH set

/// =============================================================================
/// SECTION 6: ROLLBACK MECHANISMS
/// =============================================================================

ROLLBACK_CATALOG {
    write_failed_cleanup {
        DESCRIPTION: "Undo partial write_at on error"
        STEPS:
            1. Restore old_size to desc.size
            2. Free newly allocated blocks (old_blocks..new_blocks)
            3. Truncate page cache to old aligned size
        CODE: kernel/src/fs/ext2/inode.rs (write_failed_cleanup)
    }

    free_inode_on_create_fail {
        DESCRIPTION: "Reclaim allocated inode if create/mkdir fails after alloc"
        STEPS:
            1. Free the allocated inode number in bitmap
            2. Increment group free_inodes_count
        CODE: kernel/src/fs/ext2/fs.rs (free_inode)
    }

    delete_entry_on_link_fail {
        DESCRIPTION: "Remove dir entry if link count increment fails"
        STEPS:
            1. Call delete_entry to remove the just-added entry
        CODE: kernel/src/fs/ext2/inode.rs (link rollback path)
    }
}
