// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Cross-Protocol Analysis
//
// This file analyzes interactions between protocols: deadlock freedom,
// crash composition, and invariant coverage across all tiers.
//
// Reference: 00-state-model.spec for state notation.

/// =============================================================================
/// SECTION 1: DEADLOCK FREEDOM ANALYSIS
/// =============================================================================

DEADLOCK_ANALYSIS {
    // Lock resources in the system:
    //   L1: inode.inner (RwMutex<InodeInner>) — per-inode
    //   L2: inode.xattr (RwMutex<Xattr>) — per-inode, Dir/File only
    //   L3: ext2.super_block (RwMutex<Dirty<SuperBlock>>) — global
    //   L4: block_group.desc (RwMutex) — per-group
    //   L5: block_group.block_bitmap (RwMutex) — per-group
    //   L6: block_group.inode_bitmap (RwMutex) — per-group
    //   L7: block_group.inode_cache (RwMutex) — per-group

    // === Lock ordering rules ===

    RULE two_inode_ordering {
        DESCRIPTION: "Two-inode ops lock in ascending ino order"
        PROTOCOLS: rename_cross_dir
        FORMAL: ∀ (a, b) with a.ino < b.ino:
          acquire(L1[a]) happens-before acquire(L1[b])
        CODE: kernel/src/fs/ext2/inode.rs (write_lock_two_inodes)
        VERIFIED: rename_cross_dir uses write_lock_two_inodes
    }

    RULE xattr_before_inner {
        DESCRIPTION: "Xattr lock acquired before inode inner lock"
        PROTOCOLS: set_xattr, remove_xattr
        FORMAL: ∀ inode i:
          acquire(L2[i]) happens-before acquire(L1[i])
        CODE: kernel/src/fs/ext2/inode.rs:379-395 (set_xattr)
        VERIFIED: xattr.write() acquired and dropped before inner.write()
    }

    RULE no_nested_inode_locks_single_op {
        DESCRIPTION: "Single-inode ops never hold two inode inner locks"
        PROTOCOLS: all Tier 1-3 protocols
        FORMAL: ∀ single-inode protocol P:
          P acquires at most one L1[i] at a time
        VERIFIED: by inspection of all Tier 1-3 protocol LOCKS sections
    }

    RULE parent_before_child {
        DESCRIPTION: "Directory ops lock parent before child"
        PROTOCOLS: rmdir, unlink, link
        FORMAL: ∀ (parent, child):
          acquire(L1[parent]) happens-before acquire(L1[child])
        NOTE: This is implicit — parent upread/write held while
              child lock acquired. No circular dependency because
              child never locks parent.
    }

    // === Deadlock freedom proof sketch ===
    //
    // Total lock order: L2[i] < L1[min_ino] < L1[max_ino] < L3 < L4..L7
    //
    // All protocols respect this order:
    //   - Xattr ops: L2 → L1 (xattr_before_inner)
    //   - Two-inode ops: L1[min] → L1[max] (two_inode_ordering)
    //   - Allocation ops: L1 → L3 (inner lock → sb lock via persist)
    //   - No protocol acquires locks in reverse order
    //
    // Therefore: no deadlock cycle is possible.
}

/// =============================================================================
/// SECTION 2: CRASH COMPOSITION ANALYSIS
/// =============================================================================

CRASH_COMPOSITION {
    // Ext2 does not use journaling. Crash recovery relies on fsck.
    // This section analyzes what states are reachable after crash
    // during each protocol tier.

    TIER_1_CRASH {
        DESCRIPTION: "Tier 1 (reads) are crash-safe by definition"
        FORMAL: ∀ Tier 1 protocol P, ∀ crash point:
          S_recovered = S_pre  (no state mutation)
    }

    TIER_2_CRASH {
        DESCRIPTION: "Tier 2 (single mutators) have atomic persist"
        FORMAL: ∀ Tier 2 protocol P with persist step:
          crash before persist ⟹ S_recovered = S_pre
          crash after persist  ⟹ S_recovered = S_post
        EXCEPTION: set_atime, set_mtime, set_ctime have no persist;
          crash always loses the timestamp update.
    }

    TIER_3_CRASH {
        DESCRIPTION: "Tier 3 (multi-step single-inode) have intermediate states"

        write_at_crash_windows {
            WINDOW_1: "After block allocation, before data write"
              EFFECT: Blocks allocated, size grown, data is zero/stale
              FSCK: Detects allocated blocks with uninitialized data
            WINDOW_2: "After data write, before metadata persist"
              EFFECT: Data in page cache (dirty), metadata not committed
              FSCK: On reboot, old on-disk size restored; data lost
            WINDOW_3: "After metadata persist, before device sync"
              EFFECT: Metadata in inode table page cache, not on device
              FSCK: Device write buffer may contain data
        }

        write_link_crash_windows {
            FAST_PATH: "Single persist — same as Tier 2"
            SLOW_PATH: "Same three windows as write_at"
        }

        sync_crash_windows {
            WINDOW_1: "After data writeback, before metadata persist"
              EFFECT: Data on disk, metadata stale
            WINDOW_2: "After metadata persist, before device flush"
              EFFECT: All in device write buffer
        }

        xattr_crash_windows {
            WINDOW_1: "After xattr block write, before inode file_acl update"
              EFFECT: Orphan xattr block; fsck detects via refcount
        }
    }

    TIER_4_CRASH {
        DESCRIPTION: "Tier 4 (multi-inode) have the widest crash windows"

        create_crash_windows {
            WINDOW_1: "After inode alloc, before dir entry"
              EFFECT: Orphan inode in bitmap; fsck reclaims
            WINDOW_2: "After dir entry, before parent persist"
              EFFECT: Entry in page cache, not committed
        }

        mkdir_crash_windows {
            WINDOW_1: "After child alloc, before make_empty"
              EFFECT: Orphan inode, no data block
            WINDOW_2: "After make_empty, before parent entry"
              EFFECT: Child dir exists but unreachable
            WINDOW_3: "After parent entry, before parent persist"
              EFFECT: Entry in page cache, not committed
        }

        unlink_crash_windows {
            WINDOW_1: "After delete_entry, before child link decrement"
              EFFECT: Entry gone, child link count stale; fsck repairs
        }

        rmdir_crash_windows {
            WINDOW_1: "After parent entry delete, before child freed"
              EFFECT: Orphan directory; fsck detects
            WINDOW_2: "After child freed, before parent link adjust"
              EFFECT: Parent link count stale; fsck repairs
        }

        rename_crash_windows {
            WINDOW_1: "After add/set_link in target, before delete in source"
              EFFECT: Duplicate entries; fsck detects
            WINDOW_2: "After delete, before dotdot update (cross-dir)"
              EFFECT: dotdot points to old parent; fsck repairs
            WINDOW_3: "After dotdot, before link count adjustments"
              EFFECT: Link counts stale; fsck repairs
        }
    }
}

/// =============================================================================
/// SECTION 3: INVARIANT COVERAGE MATRIX
/// =============================================================================

INVARIANT_COVERAGE {
    // Maps each global invariant to the protocols that could violate it
    // and the mechanism that preserves it.

    inode_number_stable {
        THREATENED_BY: none (ino is a plain field with no setter)
        PRESERVED_BY: Rust type system (no &mut access to ino)
    }

    type_stable {
        THREATENED_BY: none (type_ is a plain field with no setter)
        PRESERVED_BY: Rust type system
    }

    links_count_nonneg {
        THREATENED_BY: unlink, rmdir, rename (link count decrements)
        PRESERVED_BY: saturating_sub on all decrement paths
        CODE: kernel/src/fs/ext2/inode.rs:3595, 1036, 3710
    }

    freed_inode_has_dtime {
        THREATENED_BY: unlink, rmdir, rename (set is_freed)
        PRESERVED_BY: dtime = now() always set before is_freed = true
        CODE: kernel/src/fs/ext2/inode.rs:3599-3600, 1037-1038
    }

    dir_links_ge_2 {
        THREATENED_BY: mkdir (creates dir with links=2), rmdir (removes dir)
        PRESERVED_BY:
          mkdir: create_inode sets links_count=2 for Dir
          rmdir: sets links_count=0 and is_freed=true simultaneously
        CODE: kernel/src/fs/ext2/fs.rs:600, inode.rs:1036
    }

    max_link_count {
        THREATENED_BY: link (increments link count)
        PRESERVED_BY: guard check links_count < MAX_LINK_COUNT before increment
        CODE: kernel/src/fs/ext2/inode.rs:3520-3522
    }

    superblock_free_counts {
        THREATENED_BY: alloc_blocks, free_blocks, alloc_inode, free_inode
        PRESERVED_BY: sync_metadata recomputes from group descriptors
        CODE: kernel/src/fs/ext2/fs.rs:753-760
    }

    fast_symlink_size_bound {
        THREATENED_BY: write_link (writes symlink target)
        PRESERVED_BY: fast path only used when with_nul ≤ MAX_FAST_SYMLINK_LEN
        CODE: kernel/src/fs/ext2/inode.rs:489
    }

    page_cache_size_aligned {
        THREATENED_BY: write_at, resize, write_link (modify page cache size)
        PRESERVED_BY: all resize calls use align_up(size, block_size)
        CODE: kernel/src/fs/ext2/inode.rs:635, 199, 536
    }

    xattr_only_for_dir_file {
        THREATENED_BY: none (xattr field set at construction time)
        PRESERVED_BY: Inode::new only creates xattr for Dir|File
        CODE: kernel/src/fs/ext2/inode.rs:72-78
    }
}

/// =============================================================================
/// SECTION 4: PROTOCOL COVERAGE SUMMARY
/// =============================================================================

PROTOCOL_COVERAGE {
    // Total VFS methods specified: 45
    //   InodeIo:    2  (read_at, write_at — each with buffered + direct)
    //   VfsInode:  38  (13 reads + 8 mutators + 17 complex ops)
    //   FileSystem: 5  (name, sync, root_inode, sb, fs_event_subscriber_stats)

    TIER_1_PROTOCOLS: 13 {
        ino, type_, size, mode, owner, group,
        atime, mtime, ctime, metadata,
        page_cache, open, fs, extension,
        lookup, readdir_at
    }

    TIER_2_PROTOCOLS: 8 {
        set_mode, set_owner, set_group,
        set_atime, set_mtime, set_ctime,
        resize
    }

    TIER_3_PROTOCOLS: 12 {
        read_at_buffered, read_at_direct,
        write_at_buffered, write_at_direct,
        read_link, write_link,
        sync_all, sync_data,
        fallocate,
        get_xattr, list_xattr, set_xattr, remove_xattr
    }

    TIER_4_PROTOCOLS: 12 {
        create_non_dir, create_mkdir,
        link, unlink, rmdir,
        rename_same_dir, rename_cross_dir,
        mknod,
        fs_name, fs_sync, fs_root_inode, fs_sb,
        fs_event_subscriber_stats
    }

    // Rollback coverage:
    //   Protocols with explicit rollback: 8
    //     write_at (buffered+direct), write_link (slow),
    //     create_non_dir, create_mkdir, link,
    //     set_xattr (partial)
    //   Protocols with no rollback needed: 25
    //     All Tier 1, timestamp setters, sync ops, reads
    //   Protocols relying on fsck for crash repair: 12
    //     unlink, rmdir, rename (same+cross), mknod,
    //     fs_sync (partial)
}
