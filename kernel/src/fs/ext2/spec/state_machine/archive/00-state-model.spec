// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Dual-Layer State Model
//
// This file defines the foundational state model used by all protocol specs.
// Every VFS method protocol references this model for pre/post state assertions.

/// =============================================================================
/// SECTION 1: DUAL-LAYER STATE MODEL
/// =============================================================================
///
/// Ext2 maintains two layers of state that can diverge between operations:
///
///   M  — In-memory state (inode cache, page cache, dirty descriptors)
///   D  — On-disk state (block device: superblock, group descriptors, bitmaps,
///         inode table, data blocks, xattr blocks)
///
/// Notation:
///   S = (M, D)        — full system state
///   S.M.inode[i]      — in-memory inode descriptor for ino=i
///   S.D.inode[i]      — on-disk inode descriptor for ino=i
///   S.M.page_cache[i] — page cache pages for ino=i
///   S.D.data_blocks[i]— on-disk data blocks for ino=i
///   S.M.sb            — in-memory superblock (Dirty<SuperBlock>)
///   S.D.sb            — on-disk superblock
///   S.M.group[g]      — in-memory block group descriptor for group g
///   S.D.group[g]      — on-disk block group descriptor for group g
///   S.M.xattr[i]      — in-memory xattr cache for ino=i
///   S.D.xattr_block[b]— on-disk xattr block at bid=b

/// =============================================================================
/// SECTION 2: CONSISTENCY RELATION
/// =============================================================================
///
/// CONSISTENT(S) ≡
///   ∀ inode i in S.M:
///     ¬S.M.inode[i].is_dirty ⟹ S.M.inode[i] = S.D.inode[i]
///   ∧ ∀ page p in S.M.page_cache[i]:
///     ¬p.is_dirty ⟹ p.data = S.D.data_blocks[i][p.index]
///   ∧ ¬S.M.sb.is_dirty ⟹ S.M.sb = S.D.sb
///   ∧ ∀ group g:
///     ¬S.M.group[g].is_dirty ⟹ S.M.group[g] = S.D.group[g]
///
/// After sync_all / sync_data / FileSystem::sync:
///   CONSISTENT(S') holds for the synced scope.

/// =============================================================================
/// SECTION 3: INODE STATE
/// =============================================================================
///
/// InodeState = {
///   ino:          u32,                  // immutable after creation
///   type_:        InodeType,            // immutable after creation
///   block_group:  usize,                // immutable after creation
///   desc:         Dirty<InodeDesc>,     // mutable under inner.write()
///   is_freed:     bool,                 // set when links_count reaches 0
///   page_cache:   PageCache,            // data pages
///   xattr:        Option<RwMutex<Xattr>>, // xattr cache (Dir|File only)
/// }
///
/// InodeDesc = {
///   size:         u64,
///   blocks:       u32,
///   block_ptrs:   [u32; 15],
///   perm:         FilePerm,
///   uid:          u32,
///   gid:          u32,
///   links_count:  u16,
///   atime:        Duration,
///   mtime:        Duration,
///   ctime:        Duration,
///   dtime:        Duration,
///   flags:        FileFlags,
///   file_acl:     u32,                  // xattr block bid
///   type_:        InodeType,
/// }

/// =============================================================================
/// SECTION 4: LOCK MODEL
/// =============================================================================
///
/// Each inode has a single RwMutex<InodeInner> with three modes:
///
///   READ    — inner.read()    — shared, concurrent reads allowed
///   UPREAD  — inner.upread()  — shared read that can upgrade to WRITE atomically
///   WRITE   — inner.write()   — exclusive, blocks all other access
///
/// Upgrade path: UPREAD → WRITE  (via upread.upgrade())
/// Downgrade path: WRITE → UPREAD (via write.downgrade())
///
/// Xattr has a separate RwMutex<Xattr>:
///   XATTR_READ  — xattr.read()
///   XATTR_WRITE — xattr.write()
///
/// Filesystem-level locks:
///   SB_READ  — super_block.read()
///   SB_WRITE — super_block.write()
///
/// CODE: kernel/src/fs/ext2/inode.rs:46-54 (Inode struct fields)
/// CODE: kernel/src/fs/ext2/fs.rs:26-51 (Ext2 struct fields)

/// =============================================================================
/// SECTION 5: GLOBAL INVARIANTS
/// =============================================================================

INVARIANT inode_number_stable {
    DESCRIPTION: "Inode number is immutable after creation"
    FORMAL: ∀ op, ∀ inode i: S'.M.inode[i].ino = S.M.inode[i].ino
    CODE: kernel/src/fs/ext2/inode.rs:86-88 (ino is a plain field, no setter)
}

INVARIANT type_stable {
    DESCRIPTION: "Inode type is immutable after creation"
    FORMAL: ∀ op, ∀ inode i: S'.M.inode[i].type_ = S.M.inode[i].type_
    CODE: kernel/src/fs/ext2/inode.rs:272-274 (inode_type reads immutable field)
}

INVARIANT links_count_nonneg {
    DESCRIPTION: "Link count never goes below zero (u16 with saturating_sub)"
    FORMAL: ∀ inode i: S.M.inode[i].desc.links_count ≥ 0
    CODE: kernel/src/fs/ext2/inode.rs:3595 (saturating_sub usage)
}

INVARIANT freed_inode_has_dtime {
    DESCRIPTION: "Freed inodes have dtime set and is_freed=true"
    FORMAL: ∀ inode i:
      S.M.inode[i].is_freed ⟹ S.M.inode[i].desc.dtime > 0
    CODE: kernel/src/fs/ext2/inode.rs:3599-3600 (unlink sets dtime+is_freed)
}

INVARIANT dir_links_ge_2 {
    DESCRIPTION: "Live directories have links_count ≥ 2 (self '.' + parent '..')"
    FORMAL: ∀ dir inode d:
      S.M.inode[d].type_ = Dir ∧ ¬S.M.inode[d].is_freed
      ⟹ S.M.inode[d].desc.links_count ≥ 2
    CODE: kernel/src/fs/ext2/fs.rs:600 (create_inode sets links_count=2 for Dir)
    LINUX_REF: fs/ext2/ialloc.c:540
}

INVARIANT max_link_count {
    DESCRIPTION: "Link count never exceeds MAX_LINK_COUNT (32000)"
    FORMAL: ∀ inode i: S.M.inode[i].desc.links_count ≤ 32000
    CODE: kernel/src/fs/ext2/inode.rs:30 (MAX_LINK_COUNT = 32000)
    LINUX_REF: fs/ext2/ext2.h:195 (EXT2_LINK_MAX)
}

INVARIANT superblock_free_counts {
    DESCRIPTION: "Superblock free counts equal sum of group free counts"
    FORMAL:
      S.M.sb.free_blocks_count = Σ_g S.M.group[g].free_blocks_count
      ∧ S.M.sb.free_inodes_count = Σ_g S.M.group[g].free_inodes_count
    CODE: kernel/src/fs/ext2/fs.rs:753-760 (sync_metadata recomputes)
    LINUX_REF: fs/ext2/super.c:1288-1289
}

INVARIANT fast_symlink_size_bound {
    DESCRIPTION: "Fast symlinks store payload in block_ptrs, size ≤ 60"
    FORMAL: ∀ symlink inode s:
      is_fast_symlink(s) ⟹ S.M.inode[s].desc.size ≤ MAX_FAST_SYMLINK_LEN
    CODE: kernel/src/fs/ext2/inode.rs:29 (MAX_FAST_SYMLINK_LEN = 60)
    LINUX_REF: fs/ext2/inode.c:48-55
}

INVARIANT page_cache_size_aligned {
    DESCRIPTION: "Page cache capacity is block-aligned to inode size"
    FORMAL: ∀ inode i:
      S.M.page_cache[i].capacity = align_up(S.M.inode[i].desc.size, BLOCK_SIZE)
    CODE: kernel/src/fs/ext2/inode.rs:1334 (InodeInner::new aligns)
}

INVARIANT xattr_only_for_dir_file {
    DESCRIPTION: "Xattr cache exists only for Dir and File inodes"
    FORMAL: ∀ inode i:
      S.M.inode[i].xattr.is_some() ⟺ S.M.inode[i].type_ ∈ {Dir, File}
    CODE: kernel/src/fs/ext2/inode.rs:72-78 (xattr init in Inode::new)
}

/// =============================================================================
/// SECTION 6: CONCURRENCY INVARIANTS
/// =============================================================================

CONCURRENCY_INVARIANT lock_ordering_two_inodes {
    DESCRIPTION: "Two-inode operations acquire locks in ascending ino order"
    FORMAL: ∀ op requiring locks on inodes (a, b) where a.ino < b.ino:
      lock(a) happens-before lock(b)
    CODE: kernel/src/fs/ext2/inode.rs (write_lock_two_inodes helper)
    LINUX_REF: fs/ext2/namei.c:318 (ext2_rename lock ordering)
}

CONCURRENCY_INVARIANT no_lock_held_across_io {
    DESCRIPTION: "Write locks are not held during page cache I/O; upread is used"
    FORMAL: ∀ page_cache_write op:
      lock_mode = UPREAD during data transfer
    CODE: kernel/src/fs/ext2/inode.rs:648-654 (write_at phase 2 under upread)
}

CONCURRENCY_INVARIANT upgrade_is_atomic {
    DESCRIPTION: "upread→write upgrade is atomic (no intermediate state visible)"
    FORMAL: ∀ upread guard g:
      g.upgrade() transitions directly from UPREAD to WRITE
    NOTE: Guaranteed by RwMutex implementation
}

/// =============================================================================
/// SECTION 7: PERSIST HELPER
/// =============================================================================
///
/// persist_inode_and_sync(fs) is the canonical commit step:
///   1. Serialize InodeDesc → RawInode
///   2. Write RawInode to group's inode table PageCache
///   3. Mark desc as clean
///
/// CODE: kernel/src/fs/ext2/inode.rs (InodeInner::persist_inode_and_sync)
///
/// This does NOT flush to disk — it writes to the inode-table page cache.
/// Actual disk persistence requires sync_all / FileSystem::sync.

/// =============================================================================
/// SECTION 8: ERROR MODEL
/// =============================================================================
///
/// All fallible operations return Result<T>.
/// On error, the protocol must ensure:
///   ENSURE_ERR: S'.M observable state = S.M observable state
///               (rollback any partial mutations)
///
/// Rollback mechanisms used:
///   - saturating_sub to undo link count increments
///   - free_inode / free_blocks to undo allocations
///   - write_failed_cleanup to undo page cache / size growth
///   - delete_entry to undo add_entry
///   - restore saved old_size / old_blocks / old_ptr0
