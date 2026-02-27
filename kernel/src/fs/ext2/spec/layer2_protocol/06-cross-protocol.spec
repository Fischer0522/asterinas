// SPDX-License-Identifier: MPL-2.0
//
// Layer 2: Implementation Protocol -- Cross-Protocol Analysis
//
// Deadlock freedom, crash composition, invariant coverage matrix,
// protocol coverage summary, and SATISFIES completeness check.
//
// References:
//   Layer 1: layer1_hoare/00-abstract-state.spec through 06-composition.spec
//   Layer 2: layer2_protocol/00-impl-state.spec through 05-xattr-fs.spec
//   Archive: state_machine/archive/15-cross-protocol.spec (old reference)

/// =============================================================================
/// SECTION 1: DEADLOCK FREEDOM ANALYSIS
/// =============================================================================

DEADLOCK_ANALYSIS {

    // =========================================================================
    // 1.1 Lock Resources
    // =========================================================================
    //
    // L1: inode.xattr       (RwMutex<Xattr>)        -- per-inode, Dir|Reg only
    // L2: inode.inner        (RwMutex<InodeInner>)   -- per-inode
    // L3: fs.super_block     (RwMutex<Dirty<SuperBlock>>) -- global
    // L4: block_group.desc   (RwMutex)               -- per-group
    // L5: block_group.block_bitmap (RwMutex)          -- per-group
    // L6: block_group.inode_bitmap (RwMutex)          -- per-group
    // L7: block_group.inode_cache  (RwMutex)          -- per-group
    //
    // CODE: 00-impl-state.spec SECTION 2: LOCK MODEL

    // =========================================================================
    // 1.2 Lock Ordering Rules
    // =========================================================================

    RULE R1: xattr_before_inner {
        ORDER: L1[i] < L2[i]
        DESCRIPTION:
            Xattr lock acquired and released before inner lock on same inode.
        PROTOCOLS:
            set_xattr   (05-xattr-fs.spec): XATTR_WRITE -> WRITE(inner)
            remove_xattr (05-xattr-fs.spec): XATTR_WRITE -> WRITE(inner)
        VERIFICATION:
            Both protocols acquire xattr.write(), read bid, drop xattr,
            then acquire inner.write(). Sequential, not nested.
            CODE: kernel/src/fs/ext2/inode.rs:379-395 (set_xattr)
            CODE: kernel/src/fs/ext2/inode.rs:401-418 (remove_xattr)
    }

    RULE R2: two_inode_ascending_order {
        ORDER: L2[min(a,b)] < L2[max(a,b)]
        DESCRIPTION:
            Two-inode operations acquire inner locks in ascending ino order.
        PROTOCOLS:
            rename_cross_dir (03-dir-ops.spec):
                write_lock_two_inodes(self, target) acquires min first.
        VERIFICATION:
            CODE: kernel/src/fs/ext2/inode.rs (write_lock_two_inodes)
            If self.ino < target.ino: lock self first.
            If target.ino < self.ino: lock target first.
    }

    RULE R3: parent_before_child {
        ORDER: L2[parent] < L2[child]
        DESCRIPTION:
            Directory operations hold parent lock while acquiring child lock.
            No circular dependency because child never locks parent.
        PROTOCOLS:
            rmdir    (03-dir-ops.spec): UPREAD(parent) -> READ/WRITE(child)
            unlink   (03-dir-ops.spec): READ(parent) -> WRITE(child)
            link     (03-dir-ops.spec): WRITE(old) then UPREAD(self) -> WRITE(self)
            create_mkdir (03-dir-ops.spec): UPREAD(parent) -> WRITE(child)
        VERIFICATION:
            In all cases, parent lock is acquired first. Child never
            acquires parent lock. No cycle possible.
    }

    RULE R4: inner_before_superblock {
        ORDER: L2[i] < L3
        DESCRIPTION:
            Inode inner lock acquired before superblock lock.
            Superblock accessed via persist_inode_and_sync -> sync_metadata.
        PROTOCOLS:
            All mutator protocols that call persist_inode_and_sync:
            set_mode, set_owner, set_group, resize, write_at, write_link,
            sync_all, sync_data, link, unlink, rmdir, rename, create, mkdir.
        VERIFICATION:
            persist_inode_and_sync is called while inner lock is held.
            sync_metadata acquires SB_WRITE internally.
            Inner lock (L2) always acquired before SB (L3).
    }

    RULE R5: superblock_before_group {
        ORDER: L3 < L4 < L5 < L6 < L7
        DESCRIPTION:
            Group-level locks acquired after superblock lock.
            Within a group, desc < block_bitmap < inode_bitmap < inode_cache.
        PROTOCOLS:
            Block/inode allocation paths (create_inode, get_or_alloc_block,
            free_inode, truncate_blocks).
        VERIFICATION:
            sync_metadata acquires SB_WRITE, then iterates groups.
            Allocation helpers acquire group locks in L4-L7 order.
    }

    // =========================================================================
    // 1.3 Deadlock Freedom Proof
    // =========================================================================

    PROOF: no_deadlock_cycle {
        -- Total lock order:
        --   L1[i] < L2[min_ino] < L2[max_ino] < L3 < L4[g] < L5[g] < L6[g] < L7[g]
        --
        -- This is a strict total order on lock acquisition.
        --
        -- By rules R1-R5, every protocol acquires locks in this order:
        --   R1: L1 before L2 (xattr before inner)
        --   R2: L2[min] before L2[max] (ascending ino)
        --   R3: L2[parent] before L2[child] (parent-child, consistent with
        --       ascending ino when parent.ino < child.ino; when parent.ino >
        --       child.ino, the parent lock is UPREAD/WRITE and child is
        --       acquired after, which is safe because child never locks parent)
        --   R4: L2 before L3 (inner before superblock)
        --   R5: L3 before L4-L7 (superblock before group)
        --
        -- No protocol acquires locks in reverse order.
        -- Therefore: no cycle in the lock dependency graph.
        -- Therefore: deadlock freedom holds.
        --
        -- EDGE CASE: R3 vs R2 when parent.ino > child.ino.
        --   In rmdir/unlink, parent holds UPREAD/WRITE, child acquired after.
        --   Child never attempts to lock parent. No back-edge possible.
        --   In rename_cross_dir, R2 takes precedence (ascending ino order).
    }
}

/// =============================================================================
/// SECTION 2: CRASH COMPOSITION ANALYSIS
/// =============================================================================
///
/// Ext2 has no journal. Crash recovery relies on fsck to repair structural
/// inconsistencies. This section analyzes crash behavior by tier and
/// references specific CRASH_WINDOWS from Layer 2 protocol specs.

CRASH_COMPOSITION {

    // =========================================================================
    // 2.1 Tier 1: Pure Reads (crash-safe by definition)
    // =========================================================================

    TIER_1 {
        DESCRIPTION:
            Pure read operations perform no durable mutation.
            Crash at any point yields FS_recovered = FS.durable.

        PROTOCOLS:
            ino, type_, size, mode, owner, group, atime, mtime, ctime,
            metadata, page_cache, open, fs, extension
                (01-metadata-ops.spec: all 13 read protocols)
            lookup, readdir_at
                (03-dir-ops.spec: 2 read protocols)
            read_link
                (04-symlink-sync.spec: 1 read protocol)
            get_xattr, list_xattr
                (05-xattr-fs.spec: 2 read protocols)
            fs_name, fs_root_inode, fs_sb, fs_event_subscriber_stats
                (05-xattr-fs.spec: 4 FS accessor protocols)
            read_at_buffered, read_at_direct
                (02-file-io.spec: 2 read protocols)

        CRASH_WINDOWS: none
        FORMAL: forall P in Tier_1, forall crash point:
            FS_recovered = FS.durable
    }

    // =========================================================================
    // 2.2 Tier 2: Single-Field Mutators (atomic persist or volatile)
    // =========================================================================

    TIER_2 {
        DESCRIPTION:
            Single-field mutators either persist atomically (one inode write)
            or are volatile-only (no persist step).

        PERSISTING_MUTATORS {
            PROTOCOLS: set_mode, set_owner, set_group
                (01-metadata-ops.spec)
            CRASH_BEHAVIOR:
                W1: crash before persist_inode_and_sync =>
                    FS_recovered = FS.durable (in-memory change lost)
                W2: crash during/after persist =>
                    FS_recovered = FS.durable with new field value
                FORMAL: FS_recovered in {FS.durable, FS.durable[I |-> new_value]}
        }

        VOLATILE_MUTATORS {
            PROTOCOLS: set_atime, set_mtime, set_ctime
                (01-metadata-ops.spec)
            CRASH_BEHAVIOR:
                No persist step. Crash always loses the update.
                FS_recovered = FS.durable
            NOTE: Matches Linux lazytime semantics for atime.
        }
    }

    // =========================================================================
    // 2.3 Tier 3: Multi-Step Single-Inode Operations
    // =========================================================================

    TIER_3 {
        DESCRIPTION:
            Multi-step operations on a single inode with intermediate crash
            windows. Data may be partially written; metadata may lag.

        write_at_crash {
            PROTOCOLS: write_at_buffered, write_at_direct
                (02-file-io.spec)
            WINDOWS:
                W1 (phase 1 committed, lock dropped):
                    Blocks allocated, size possibly grown, data zero/stale.
                    Inode descriptor in inode-table page cache, NOT on device.
                    RECOVERY: FS.durable (page cache lost on reboot).
                    RISK: background writeback may flush inode-table page,
                        exposing new size with uninitialized data.

                W2 (phase 2 in progress):
                    Partial data in page cache (buffered) or on device (direct).
                    RECOVERY: FS.durable or partial_write(FS.durable, ino,
                        offset, buf[0..k]).
                    NOTE: Direct path writes reach device immediately, so
                        partial data is more likely to survive crash.

                W3 (phase 3 in progress):
                    Timestamps updated, persist may be partial.
                    RECOVERY: FS.durable or FS' depending on persist completion.

            ROLLBACK: write_failed_cleanup restores old_size, frees blocks,
                trims page cache on phase 1/2 errors.
        }

        resize_crash {
            PROTOCOLS: resize_shrink, resize_grow
                (01-metadata-ops.spec)
            WINDOWS:
                SHRINK W1: page cache shrunk, blocks not freed.
                    RECOVERY: FS.durable.
                SHRINK W2: some blocks freed in bitmap, pointers not cleared.
                    RECOVERY: FS.durable with orphan blocks. fsck reclaims.
                SHRINK W3: persist partial.
                    RECOVERY: FS.durable or FS' with new size.

                GROW W1: page cache extended, size not persisted.
                    RECOVERY: FS.durable (extra pages harmless).
                GROW W2: persist partial.
                    RECOVERY: FS.durable or FS' with new size.
        }

        write_link_crash {
            PROTOCOLS: write_link_fast, write_link_slow
                (04-symlink-sync.spec)
            WINDOWS:
                FAST: single inode write (same as Tier 2).
                    RECOVERY: {FS.durable, FS'.durable}

                SLOW W1-W3: same three-phase pattern as write_at.
                    W1: blocks allocated, size set, data unwritten.
                    W2: partial target in page cache.
                    W3: persist partial.
                    RECOVERY: {FS.durable, FS_partial, FS'.durable}

            ROLLBACK: write_failed_cleanup on phase 1/2 errors (slow path).
        }

        sync_crash {
            PROTOCOLS: sync_all, sync_data
                (04-symlink-sync.spec)
            WINDOWS:
                W1: data page writeback partial.
                    RECOVERY: FS_recovered.data[ino] partially updated.
                W2: metadata persist partial.
                    RECOVERY: data on disk, metadata stale.
                W3: after persist, before device flush.
                    RECOVERY: writes in device buffer, may be lost.
                W4: during device flush.
                    RECOVERY: {FS_partial, FS'.durable}

            NOTE: sync is idempotent. No rollback needed.
        }

        xattr_crash {
            PROTOCOLS: set_xattr, remove_xattr
                (05-xattr-fs.spec)
            WINDOWS:
                W1: xattr block written to page cache, inode file_acl not updated.
                    RECOVERY: orphan xattr block. fsck reclaims via refcount.
                W2: inode persist partial.
                    RECOVERY: {FS.durable, FS'}

            NOTE: get_xattr and list_xattr are pure reads (no crash windows).
        }

        fallocate_crash {
            PROTOCOLS: fallocate_punch_hole, fallocate_allocate,
                fallocate_keep_size_and_unsupported
                (04-symlink-sync.spec)
            WINDOWS:
                PunchHoleKeepSize: page cache only. No crash windows.
                    RECOVERY: FS.durable (zeros lost).
                Allocate: delegates to resize. Same crash windows.
                AllocateKeepSize: no-op. No crash windows.
        }
    }

    // =========================================================================
    // 2.4 Tier 4: Multi-Inode Operations (widest crash windows)
    // =========================================================================

    TIER_4 {
        DESCRIPTION:
            Multi-inode operations modify two or more inodes and/or the
            superblock. These have the widest crash windows because multiple
            persist steps must complete for full consistency.

        create_crash {
            PROTOCOLS: create_non_dir (03-dir-ops.spec)
            WINDOWS:
                W1: After create_inode (step 5), before add_entry (step 8).
                    Inode allocated in bitmap, inode table written.
                    No directory entry. Orphan inode with links_count=1.
                    RECOVERY: fsck reclaims unreferenced inode.

                W2: During add_entry (step 8), partial persist.
                    Directory page cache written but commit_dir_metadata
                    may not have completed.
                    RECOVERY: entry may or may not be visible after reboot.

                W3: After add_entry completes. Fully consistent.

            ROLLBACK: add_entry failure triggers fs.free_inode(child_ino).
        }

        mkdir_crash {
            PROTOCOLS: create_mkdir (03-dir-ops.spec)
            WINDOWS:
                W1: After parent links_count incremented (step 7).
                    Not persisted. Reboot restores old link count. No leak.

                W2: After child inode allocated (step 9), before make_empty.
                    Child in bitmap but no data block. Orphan inode.
                    RECOVERY: fsck reclaims orphan.

                W3: After make_empty (step 11), before parent entry (step 12).
                    Child has "."/"..". Parent has no entry. Orphan directory.
                    RECOVERY: fsck reclaims orphan, adjusts link counts.

                W4: After entry written (step 12), before commit (step 14).
                    Entry in parent page cache but metadata not committed.
                    RECOVERY: may or may not survive depending on writeback.

                W5: After commit (step 14). Fully consistent.

            ROLLBACK: Four-stage rollback (ROLLBACK_A through ROLLBACK_D)
                covering each failure point.
        }

        link_crash {
            PROTOCOLS: link (03-dir-ops.spec)
            WINDOWS:
                W1: After link count incremented (step 9), before add_entry.
                    Link count in memory only. Reboot restores old count.
                    RECOVERY: FS.durable. No leak.

                W2: After add_entry (step 11), before persist (step 13).
                    Dir entry exists but link count not persisted.
                    RECOVERY: fsck detects link count mismatch, repairs.

                W3: After persist (step 13). Fully consistent.

            ROLLBACK: add_entry failure triggers links_count -= 1.
        }

        unlink_crash {
            PROTOCOLS: unlink (03-dir-ops.spec)
            WINDOWS:
                W1: After delete_entry (step 9), before child persist (step 14).
                    Dir entry removed but child link count not decremented.
                    Child has stale link count.
                    RECOVERY: fsck repairs link count from directory scan.

                W2: After persist (step 14). Fully consistent.
                    If links_count=0: inode marked for reclamation.

            ROLLBACK: none. Sequential operations; fsck repairs partial state.
        }

        rmdir_crash {
            PROTOCOLS: rmdir (03-dir-ops.spec)
            WINDOWS:
                W1: After parent entry deleted (step 14), before child freed (step 20).
                    Parent entry deleted but child not marked freed.
                    Orphan directory with links_count=2.
                    RECOVERY: fsck detects unreferenced directory, reclaims.

                W2: After child freed (step 20), before parent link adjust (step 23).
                    Child freed but parent links_count not decremented.
                    RECOVERY: fsck detects parent link count mismatch, repairs.

                W3: After parent link adjust (step 23). Fully consistent.

            ROLLBACK: none. Three-phase persist; fsck repairs partial state.
        }

        rename_crash {
            PROTOCOLS: rename_same_dir, rename_cross_dir (03-dir-ops.spec)
            WINDOWS:
                SAME_DIR:
                    W1: After add/set_link in target (step 10a/7b), before
                        delete old entry (step 22).
                        Both old and new entries exist. Duplicate references.
                        RECOVERY: fsck detects duplicate entries, repairs.

                    W2: After delete old entry (step 22), before link count
                        adjustments (step 23).
                        Entry moved but link counts stale.
                        RECOVERY: fsck repairs link counts from directory scan.

                    W3: After all persists (step 23). Fully consistent.

                CROSS_DIR:
                    W1: After add/set_link in target (step 11a/8b), before
                        delete in source (step 23).
                        Entry in both directories. Duplicate references.
                        RECOVERY: fsck detects duplicate, repairs link counts.

                    W2: After delete in source (step 23), before ".." update
                        (step 24).
                        Entry moved but ".." still points to old parent.
                        RECOVERY: fsck repairs ".." and link counts.

                    W3: During ".." update and link count persist (step 24).
                        ".." correct but parent link counts stale.
                        RECOVERY: fsck recomputes link counts from entries.

                    W4: After step 24 completes. Fully consistent.

            ROLLBACK: none. Multi-step sequence; fsck repairs partial state.
        }

        mknod_crash {
            PROTOCOLS: mknod (03-dir-ops.spec)
            WINDOWS:
                W1: Inherits W1-W3 from create_non_dir.
                    Inode allocated but no directory entry (orphan).
                    RECOVERY: fsck reclaims orphan inode.

                W2: After create completes, before set_device_id (step 3).
                    Inode exists in directory but has no device encoding.
                    Special file inode with zero block_ptrs.
                    RECOVERY: inode exists but device ID is zero.

                W3: After set_device_id completes. Fully consistent.

            ROLLBACK: create failure handled by create_non_dir rollback.
                set_device_id failure: no rollback of create.
        }

        fs_sync_crash {
            PROTOCOLS: fs_sync (05-xattr-fs.spec)
            WINDOWS:
                W1: During sync_all_inodes (step 1).
                    Some inodes persisted, others not.
                    RECOVERY: unpersisted inodes revert to FS.durable.

                W2: During sync_metadata group writes (step 2a-2c).
                    Some group descriptors written, others not.
                    Superblock counters may be stale.
                    RECOVERY: fsck recomputes free counts from bitmaps.

                W3: During superblock writes (step 2d-2f).
                    Primary superblock written but backups not (or vice versa).
                    RECOVERY: fsck uses most recent valid superblock copy.

                W4: During device sync (step 3).
                    Device write cache partially flushed.
                    RECOVERY: depends on device write ordering guarantees.

            ROLLBACK: none. Sync is idempotent; re-running completes work.
        }
    }
}

/// =============================================================================
/// SECTION 3: INVARIANT COVERAGE MATRIX
/// =============================================================================
///
/// Maps each invariant (INV-01 through INV-10 from 00-abstract-state.spec)
/// to the Layer 2 protocols that could violate it, and references the
/// SATISFIES_PROOF sections that prove preservation.

INVARIANT_COVERAGE_MATRIX {

    INV-01: inode_identity_immutable {
        THREATENED_BY: none
        PROTOCOLS_VERIFIED: all (by FRAME clauses -- no protocol modifies type_)
        SATISFIES_PROOF_REFS:
            -- Every protocol's SATISFIES_PROOF.FRAME confirms type_ not modified.
            -- See layer1_hoare/06-composition.spec INV-01 proof.
    }

    INV-02: root_inode_exists {
        THREATENED_BY: unlink, rmdir, rename (could theoretically free root)
        PROTOCOLS_VERIFIED:
            unlink   (03-dir-ops.spec): PRE requires child.type_ != Dir;
                root is Dir, so unlink cannot target it.
            rmdir    (03-dir-ops.spec): PRE requires name != "."/"..";
                root has no removable parent entry.
            rename   (03-dir-ops.spec): cannot rename "."/"..";
                root cannot be the moved inode in a freeing context.
        SATISFIES_PROOF_REFS:
            unlink.SATISFIES_PROOF.PRE (step 8: child.type_ != Dir)
            rmdir.SATISFIES_PROOF.PRE (step 2: name validation)
    }

    INV-03: dir_dot_entries {
        THREATENED_BY: create_mkdir, rmdir, rename_cross_dir
        PROTOCOLS_VERIFIED:
            create_mkdir (03-dir-ops.spec): make_empty (step 11) writes
                "." and ".." entries. SATISFIES_PROOF.POST.child confirms.
            rmdir        (03-dir-ops.spec): sets child alive=false.
                INV-03 quantifies over alive dirs only. Dead child excluded.
            rename_cross_dir (03-dir-ops.spec): step 24 updates ".." via
                set_link. SATISFIES_PROOF.POST.dotdot confirms.
        SATISFIES_PROOF_REFS:
            create_mkdir.SATISFIES_PROOF.POST.child
            rmdir.SATISFIES_PROOF.POST.child (alive=false)
            rename_cross_dir.SATISFIES_PROOF.POST.dotdot
    }

    INV-04: link_count_consistency {
        THREATENED_BY: create_non_dir, create_mkdir, link, unlink, rmdir,
            rename_same_dir, rename_cross_dir, mknod
        PROTOCOLS_VERIFIED:
            create_non_dir: links_count=1, one dir entry. Consistent.
            create_mkdir:   links_count=2, parent +1. Consistent.
            link:           links_count +1, one dir entry added. Consistent.
            unlink:         links_count -1, one dir entry removed. Consistent.
            rmdir:          child -2, parent -1. Matches entry removal. Consistent.
            rename_same_dir: net 0 for moved inode; replacement decremented.
            rename_cross_dir: ".." update adjusts parent counts.
            mknod:          delegates to create_non_dir. Consistent.
        SATISFIES_PROOF_REFS:
            create_non_dir.SATISFIES_PROOF.POST.inode (links_count=1)
            create_mkdir.SATISFIES_PROOF.POST.parent (links_count += 1)
            link.SATISFIES_PROOF.POST.links
            unlink.SATISFIES_PROOF.POST.links
            rmdir.SATISFIES_PROOF.POST.child + POST.parent
            rename_same_dir.SATISFIES_PROOF.POST.dir_links
            rename_cross_dir.SATISFIES_PROOF.POST.src_links + POST.dst_links
    }

    INV-05: freed_inode_marking {
        THREATENED_BY: unlink, rmdir, rename (replacement path)
        PROTOCOLS_VERIFIED:
            unlink (03-dir-ops.spec): steps 13-14 set dtime=now() and
                is_freed=true together when links_count reaches 0.
            rmdir  (03-dir-ops.spec): steps 18-19 set dtime=now() and
                is_freed=true for child with links_count=0.
            rename_same_dir (03-dir-ops.spec): steps 15a-16a set dtime
                and is_freed for replaced inode when links_count=0.
            rename_cross_dir (03-dir-ops.spec): steps 16a-17a same pattern.
        SATISFIES_PROOF_REFS:
            unlink.SATISFIES_PROOF.POST.freed
            rmdir.SATISFIES_PROOF.POST.child (steps 16-20)
            rename_same_dir.SATISFIES_PROOF.POST.replace
            rename_cross_dir.SATISFIES_PROOF.POST.replace
    }

    INV-06: block_exclusivity {
        THREATENED_BY: write_at, write_link (slow), resize (grow),
            create_mkdir, fallocate (Allocate)
        PROTOCOLS_VERIFIED:
            All block allocation goes through get_or_alloc_block which
            allocates from free block bitmap. A free block is by definition
            not referenced by any other inode.
            write_at_buffered/direct (02-file-io.spec): step 8/9 allocates
                from free bitmap.
            write_link_slow (04-symlink-sync.spec): step 10 allocates
                from free bitmap.
            create_mkdir (03-dir-ops.spec): make_empty allocates one block
                from free bitmap.
            resize (01-metadata-ops.spec): grow path allocates from free bitmap.
        SATISFIES_PROOF_REFS:
            write_at_buffered.SATISFIES_PROOF.FRAME
            write_at_direct.SATISFIES_PROOF.FRAME
            write_link_slow.SATISFIES_PROOF.FRAME
            create_mkdir.SATISFIES_PROOF.FRAME
    }

    INV-07: superblock_counter_consistency {
        THREATENED_BY: create_non_dir, create_mkdir, mknod (free_inodes),
            write_at, write_link_slow, resize (free_blocks),
            unlink, rmdir (deferred free_inodes),
            set_xattr (may allocate xattr block)
        PROTOCOLS_VERIFIED:
            All allocation/free paths update group descriptors.
            sync_metadata (called by persist_inode_and_sync) recomputes
            sb.free_blocks and sb.free_inodes from group descriptors.
            fs_sync (05-xattr-fs.spec): step 2 calls sync_metadata,
                recomputing all counters.
            sync_all (04-symlink-sync.spec): step 5 calls persist_inode_and_sync
                which calls sync_metadata.
        SATISFIES_PROOF_REFS:
            fs_sync.SATISFIES_PROOF.POST.sb_counters
            sync_all.SATISFIES_PROOF.POST.sb
        NOTE: Counters may be transiently stale between syncs.
            This is acceptable; fsck recomputes on recovery.
    }

    INV-08: fast_symlink_size_bound {
        THREATENED_BY: write_link_fast, write_link_slow
        PROTOCOLS_VERIFIED:
            write_link_fast (04-symlink-sync.spec): DISPATCH condition
                |target|+1 <= MAX_FAST_SYMLINK_LEN ensures size <= 59.
                Step 10 sets desc.size = target.len() <= 59.
            write_link_slow (04-symlink-sync.spec): DISPATCH condition
                |target|+1 > MAX_FAST_SYMLINK_LEN. After this write,
                blocks > 0, so is_fast_symlink() returns false.
                INV-08 quantifier excludes non-fast symlinks.
        SATISFIES_PROOF_REFS:
            write_link_fast.SATISFIES_PROOF.POST.size
            write_link_slow.SATISFIES_PROOF.POST.blocks
    }

    INV-09: ctime_monotonicity {
        THREATENED_BY: set_mode, set_owner, set_group, set_ctime,
            write_at, resize, create, mkdir, link, unlink, rmdir,
            rename, set_xattr, remove_xattr, write_link
        PROTOCOLS_VERIFIED:
            All mutator protocols set ctime = now() where now() is
            monotonic. ctime never decreases (except set_ctime which
            is the documented VFS exception).
        SATISFIES_PROOF_REFS:
            set_mode.SATISFIES_PROOF.POST (ctime = now())
            write_at_buffered.SATISFIES_PROOF.POST.timestamps (step 15)
            link.SATISFIES_PROOF.POST.ctime (step 8)
            unlink.SATISFIES_PROOF.POST.ctime (step 11)
            rename_same_dir.SATISFIES_PROOF.POST.ctime (step 19)
            set_xattr.SATISFIES_PROOF.POST.ctime (step 9)
        NOTE: set_ctime may set arbitrary value (VFS exception).
    }

    INV-10: dir_links_ge_2 {
        THREATENED_BY: create_mkdir, rmdir, rename (dir move)
        PROTOCOLS_VERIFIED:
            create_mkdir (03-dir-ops.spec): new dir gets links_count=2
                (step 9 create_inode for Dir). Parent gets +1 (step 7).
                Parent had links >= 2, now >= 3. New dir = 2. Both >= 2.
            rmdir (03-dir-ops.spec): child set to links_count=0 and
                alive=false (steps 16-19). INV-10 quantifies over alive
                dirs only; dead child excluded. Parent gets -1 (step 22).
                Parent had links >= 3 (at least one child's ".."), now >= 2.
            rename_same_dir (03-dir-ops.spec): dir move with no replacement:
                net link change = 0 (+1 then -1). With replacement:
                -1 for lost replaced dir's "..". Parent had >= 3, now >= 2.
            rename_cross_dir (03-dir-ops.spec): src_dir -1 (step 24).
                src_dir had >= 3 (this child's ".."), now >= 2.
                dst_dir +1 (step 9b) or unchanged (replacement). >= 2.
        SATISFIES_PROOF_REFS:
            create_mkdir.SATISFIES_PROOF.POST.child (links_count=2)
            create_mkdir.SATISFIES_PROOF.POST.parent (links_count += 1)
            rmdir.SATISFIES_PROOF.POST.child (links=0, alive=false)
            rmdir.SATISFIES_PROOF.POST.parent (links_count -= 1)
            rename_cross_dir.SATISFIES_PROOF.POST.src_links
            rename_cross_dir.SATISFIES_PROOF.POST.dst_links
    }
}

/// =============================================================================
/// SECTION 4: PROTOCOL COVERAGE SUMMARY
/// =============================================================================

PROTOCOL_COVERAGE {

    // =========================================================================
    // 4.1 Layer 1 HOARE Spec Count
    // =========================================================================

    LAYER_1_HOARE_SPECS {
        FILE 01-metadata-ops.spec:
            READ:   13 (ino, type_, size, mode, owner, group, atime, mtime,
                        ctime, metadata, page_cache, open, fs, extension)
            WRITE:   7 (set_mode, set_owner, set_group, set_atime, set_mtime,
                        set_ctime, resize)
            SUBTOTAL: 20

        FILE 02-file-io.spec:
            read_at, write_at
            SUBTOTAL: 2

        FILE 03-dir-ops.spec:
            create, mkdir, mknod, lookup, readdir_at,
            link, unlink, rmdir, rename
            SUBTOTAL: 9

        FILE 04-symlink-sync.spec:
            read_link, write_link, sync_all, sync_data, fallocate
            SUBTOTAL: 5

        FILE 05-xattr-fs.spec:
            set_xattr, get_xattr, list_xattr, remove_xattr,
            fs_name, fs_sync, fs_root_inode, fs_sb,
            fs_event_subscriber_stats
            SUBTOTAL: 9

        TOTAL LAYER 1 HOARE SPECS: 45
    }

    // =========================================================================
    // 4.2 Layer 2 PROTOCOL Count
    // =========================================================================

    LAYER_2_PROTOCOLS {
        FILE 01-metadata-ops.spec:
            READ:  13 (ino, type_, size, mode, owner, group, atime, mtime,
                       ctime, metadata, page_cache, open, fs, extension)
            WRITE:  8 (set_mode, set_owner, set_group, set_atime, set_mtime,
                       set_ctime, resize_shrink, resize_grow)
            SUBTOTAL: 21

        FILE 02-file-io.spec:
            read_at_buffered, read_at_direct,
            write_at_buffered, write_at_direct
            SUBTOTAL: 4

        FILE 03-dir-ops.spec:
            lookup, readdir_at, create_non_dir, create_mkdir,
            link, unlink, rmdir,
            rename_same_dir, rename_cross_dir, mknod
            SUBTOTAL: 10

        FILE 04-symlink-sync.spec:
            read_link, write_link_fast, write_link_slow,
            sync_all, sync_data,
            fallocate_punch_hole, fallocate_allocate,
            fallocate_keep_size_and_unsupported
            SUBTOTAL: 8

        FILE 05-xattr-fs.spec:
            get_xattr, set_xattr, list_xattr, remove_xattr,
            fs_name, fs_sync, fs_root_inode, fs_sb,
            fs_event_subscriber_stats
            SUBTOTAL: 9

        TOTAL LAYER 2 PROTOCOLS: 52
    }

    // =========================================================================
    // 4.3 DISPATCH Exhaustiveness (split protocols)
    // =========================================================================

    DISPATCH_EXHAUSTIVENESS {
        -- Layer 1 specs that map to multiple Layer 2 protocols via DISPATCH:

        read_at -> {read_at_buffered, read_at_direct}
            DISPATCH: O_DIRECT flag
            EXHAUSTIVE: flag is either set or not. Complete partition.

        write_at -> {write_at_buffered, write_at_direct}
            DISPATCH: O_DIRECT flag
            EXHAUSTIVE: flag is either set or not. Complete partition.

        resize -> {resize_shrink, resize_grow}
            DISPATCH: new_size < old_size vs new_size >= old_size
            EXHAUSTIVE: covers all cases (equal = no-op in grow path).

        write_link -> {write_link_fast, write_link_slow}
            DISPATCH: |target|+1 <= MAX_FAST_SYMLINK_LEN vs >
            EXHAUSTIVE: integer comparison. Complete partition.

        rename -> {rename_same_dir, rename_cross_dir}
            DISPATCH: src_dir.ino = dst_dir.ino vs !=
            EXHAUSTIVE: equality is decidable. Complete partition.

        fallocate -> {fallocate_punch_hole, fallocate_allocate,
                      fallocate_keep_size_and_unsupported}
            DISPATCH: mode enum
            EXHAUSTIVE: PunchHoleKeepSize, Allocate, AllocateKeepSize,
                and default (unsupported). All FallocateMode variants covered.

        create -> {create_non_dir, create_mkdir}
            DISPATCH: type_ = Dir vs type_ != Dir
            EXHAUSTIVE: type_ is either Dir or not. Complete partition.

        -- All other Layer 1 specs map 1:1 to Layer 2 protocols (DISPATCH: always).
    }

    // =========================================================================
    // 4.4 Rollback Coverage
    // =========================================================================

    ROLLBACK_COVERAGE {
        PROTOCOLS_WITH_EXPLICIT_ROLLBACK: 8 {
            write_at_buffered   (02-file-io.spec):  write_failed_cleanup
            write_at_direct     (02-file-io.spec):  write_failed_cleanup
            write_link_slow     (04-symlink-sync.spec): write_failed_cleanup (P1+P2)
            create_non_dir      (03-dir-ops.spec):  fs.free_inode on add_entry failure
            create_mkdir        (03-dir-ops.spec):  ROLLBACK_A through ROLLBACK_D
            link                (03-dir-ops.spec):  links_count -= 1 on add_entry failure
            set_xattr           (05-xattr-fs.spec): internal xattr rollback (phase 1)
            remove_xattr        (05-xattr-fs.spec): internal xattr rollback (phase 1)
        }

        PROTOCOLS_WITH_NO_ROLLBACK_NEEDED: 32 {
            -- All 22 Tier 1 read protocols (no mutation)
            -- 6 Tier 2 timestamp setters (volatile or single persist)
            -- sync_all, sync_data (idempotent)
            -- fallocate_punch_hole (page cache only)
            -- fallocate_keep_size_and_unsupported (no-op)
        }

        PROTOCOLS_RELYING_ON_FSCK: 12 {
            unlink              (03-dir-ops.spec):  no rollback; fsck repairs links
            rmdir               (03-dir-ops.spec):  no rollback; fsck repairs
            rename_same_dir     (03-dir-ops.spec):  no rollback; fsck repairs
            rename_cross_dir    (03-dir-ops.spec):  no rollback; fsck repairs
            mknod               (03-dir-ops.spec):  partial (set_device_id failure)
            resize_shrink       (01-metadata-ops.spec): partial block free
            resize_grow         (01-metadata-ops.spec): delegates to write_failed_cleanup
            write_link_fast     (04-symlink-sync.spec): persist failure leaves dirty
            fs_sync             (05-xattr-fs.spec): partial sync; re-run completes
            set_xattr           (05-xattr-fs.spec): phase 2 failure (orphan block)
            remove_xattr        (05-xattr-fs.spec): phase 2 failure (dangling ref)
            fallocate_allocate  (04-symlink-sync.spec): delegates to resize
        }
    }
}

/// =============================================================================
/// SECTION 5: SATISFIES COMPLETENESS CHECK
/// =============================================================================
///
/// Two-way verification:
///   (A) Every Layer 2 PROTOCOL has a valid SATISFIES target in Layer 1.
///   (B) Every Layer 1 HOARE spec has at least one Layer 2 PROTOCOL.

SATISFIES_COMPLETENESS {

    // =========================================================================
    // 5.1 Forward Map: Layer 2 PROTOCOL -> Layer 1 HOARE
    // =========================================================================
    // Every Layer 2 protocol must have a valid SATISFIES target.

    FORWARD_MAP {
        // --- 01-metadata-ops.spec (21 protocols) ---
        ino                 -> layer1::ino                  OK
        type_               -> layer1::type_                OK
        size                -> layer1::size                 OK
        mode                -> layer1::mode                 OK
        owner               -> layer1::owner                OK
        group               -> layer1::group                OK
        atime               -> layer1::atime                OK
        mtime               -> layer1::mtime                OK
        ctime               -> layer1::ctime                OK
        metadata            -> layer1::metadata             OK
        page_cache          -> layer1::page_cache           OK
        open                -> layer1::open                 OK
        fs                  -> layer1::fs                   OK
        extension           -> layer1::extension            OK
        set_mode            -> layer1::set_mode             OK
        set_owner           -> layer1::set_owner            OK
        set_group           -> layer1::set_group            OK
        set_atime           -> layer1::set_atime            OK
        set_mtime           -> layer1::set_mtime            OK
        set_ctime           -> layer1::set_ctime            OK
        resize_shrink       -> layer1::resize               OK
        resize_grow         -> layer1::resize               OK

        // --- 02-file-io.spec (4 protocols) ---
        read_at_buffered    -> layer1::read_at              OK
        read_at_direct      -> layer1::read_at              OK
        write_at_buffered   -> layer1::write_at             OK
        write_at_direct     -> layer1::write_at             OK

        // --- 03-dir-ops.spec (10 protocols) ---
        lookup              -> layer1::lookup               OK
        readdir_at          -> layer1::readdir_at           OK
        create_non_dir      -> layer1::create               OK
        create_mkdir        -> layer1::mkdir                 OK
        link                -> layer1::link                  OK
        unlink              -> layer1::unlink                OK
        rmdir               -> layer1::rmdir                 OK
        rename_same_dir     -> layer1::rename                OK
        rename_cross_dir    -> layer1::rename                OK
        mknod               -> layer1::mknod                 OK

        // --- 04-symlink-sync.spec (8 protocols) ---
        read_link           -> layer1::read_link             OK
        write_link_fast     -> layer1::write_link            OK
        write_link_slow     -> layer1::write_link            OK
        sync_all            -> layer1::sync_all              OK
        sync_data           -> layer1::sync_data             OK
        fallocate_punch_hole -> layer1::fallocate            OK
        fallocate_allocate  -> layer1::fallocate             OK
        fallocate_keep_size_and_unsupported
                            -> layer1::fallocate             OK

        // --- 05-xattr-fs.spec (9 protocols) ---
        get_xattr           -> layer1::get_xattr             OK
        set_xattr           -> layer1::set_xattr             OK
        list_xattr          -> layer1::list_xattr            OK
        remove_xattr        -> layer1::remove_xattr          OK
        fs_name             -> layer1::fs_name               OK
        fs_sync             -> layer1::fs_sync               OK
        fs_root_inode       -> layer1::fs_root_inode         OK
        fs_sb               -> layer1::fs_sb                 OK
        fs_event_subscriber_stats
                            -> layer1::fs_event_subscriber_stats OK

        TOTAL: 52 protocols, all with valid SATISFIES targets.
        STATUS: COMPLETE
    }

    // =========================================================================
    // 5.2 Reverse Map: Layer 1 HOARE -> Layer 2 PROTOCOL(s)
    // =========================================================================
    // Every Layer 1 HOARE spec must have at least one implementing protocol.

    REVERSE_MAP {
        // --- 01-metadata-ops (20 HOARE specs) ---
        layer1::ino         <- {ino}                        OK (1:1)
        layer1::type_       <- {type_}                      OK (1:1)
        layer1::size        <- {size}                       OK (1:1)
        layer1::mode        <- {mode}                       OK (1:1)
        layer1::owner       <- {owner}                      OK (1:1)
        layer1::group       <- {group}                      OK (1:1)
        layer1::atime       <- {atime}                      OK (1:1)
        layer1::mtime       <- {mtime}                      OK (1:1)
        layer1::ctime       <- {ctime}                      OK (1:1)
        layer1::metadata    <- {metadata}                   OK (1:1)
        layer1::page_cache  <- {page_cache}                 OK (1:1)
        layer1::open        <- {open}                       OK (1:1)
        layer1::fs          <- {fs}                         OK (1:1)
        layer1::extension   <- {extension}                  OK (1:1)
        layer1::set_mode    <- {set_mode}                   OK (1:1)
        layer1::set_owner   <- {set_owner}                  OK (1:1)
        layer1::set_group   <- {set_group}                  OK (1:1)
        layer1::set_atime   <- {set_atime}                  OK (1:1)
        layer1::set_mtime   <- {set_mtime}                  OK (1:1)
        layer1::set_ctime   <- {set_ctime}                  OK (1:1)

        // --- 01-metadata-ops (split) ---
        layer1::resize      <- {resize_shrink, resize_grow} OK (1:2)

        // --- 02-file-io (2 HOARE specs) ---
        layer1::read_at     <- {read_at_buffered,
                                read_at_direct}             OK (1:2)
        layer1::write_at    <- {write_at_buffered,
                                write_at_direct}            OK (1:2)

        // --- 03-dir-ops (9 HOARE specs) ---
        layer1::lookup      <- {lookup}                     OK (1:1)
        layer1::readdir_at  <- {readdir_at}                 OK (1:1)
        layer1::create      <- {create_non_dir}             OK (1:1)
        layer1::mkdir       <- {create_mkdir}               OK (1:1)
        layer1::mknod       <- {mknod}                      OK (1:1)
        layer1::link        <- {link}                       OK (1:1)
        layer1::unlink      <- {unlink}                     OK (1:1)
        layer1::rmdir       <- {rmdir}                      OK (1:1)
        layer1::rename      <- {rename_same_dir,
                                rename_cross_dir}           OK (1:2)

        // --- 04-symlink-sync (5 HOARE specs) ---
        layer1::read_link   <- {read_link}                  OK (1:1)
        layer1::write_link  <- {write_link_fast,
                                write_link_slow}            OK (1:2)
        layer1::sync_all    <- {sync_all}                   OK (1:1)
        layer1::sync_data   <- {sync_data}                  OK (1:1)
        layer1::fallocate   <- {fallocate_punch_hole,
                                fallocate_allocate,
                                fallocate_keep_size_and_unsupported}
                                                            OK (1:3)

        // --- 05-xattr-fs (9 HOARE specs) ---
        layer1::get_xattr   <- {get_xattr}                 OK (1:1)
        layer1::set_xattr   <- {set_xattr}                 OK (1:1)
        layer1::list_xattr  <- {list_xattr}                OK (1:1)
        layer1::remove_xattr <- {remove_xattr}             OK (1:1)
        layer1::fs_name     <- {fs_name}                    OK (1:1)
        layer1::fs_sync     <- {fs_sync}                    OK (1:1)
        layer1::fs_root_inode <- {fs_root_inode}            OK (1:1)
        layer1::fs_sb       <- {fs_sb}                      OK (1:1)
        layer1::fs_event_subscriber_stats
                            <- {fs_event_subscriber_stats}  OK (1:1)

        TOTAL: 45 HOARE specs, all with at least one implementing protocol.
        STATUS: COMPLETE
    }

    // =========================================================================
    // 5.3 Summary
    // =========================================================================

    SUMMARY {
        Layer 1 HOARE specs:     45
        Layer 2 PROTOCOLs:       52
        Split protocols:          7 (resize, read_at, write_at, write_link,
                                     rename, fallocate, create)
        1:1 mappings:            38
        1:N mappings:             7 (N in {2, 3})

        Forward completeness:    52/52 protocols have valid SATISFIES targets.
        Reverse completeness:    45/45 HOARE specs have implementing protocols.
        DISPATCH exhaustiveness: all 7 split protocols have exhaustive dispatch.

        Invariants verified:     10/10 (INV-01 through INV-10)
        Composition properties:  10/10 (COMP-1 through COMP-10)
        Deadlock freedom:        proven via total lock order (R1-R5)

        VERDICT: SATISFIES relation is complete and well-formed.
    }
}
