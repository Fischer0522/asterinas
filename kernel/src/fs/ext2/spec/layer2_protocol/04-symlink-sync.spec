// SPDX-License-Identifier: MPL-2.0
//
// Layer 2: Implementation Protocol -- Symlink, Sync, and Fallocate Operations
//
// Maps Layer 1 HOARE specs (04-symlink-sync.spec) to concrete Asterinas
// implementation: lock sequences, crash windows, rollback mechanisms.
//
// Reference: layer2_protocol/00-impl-state.spec for ConcreteState model.
// Reference: layer1_hoare/04-symlink-sync.spec for abstract specifications.

/// =============================================================================
/// PROTOCOL 1: read_link
/// =============================================================================
/// Pure read of symlink target. Two storage variants (fast/slow) but both
/// are read-only, so a single protocol covers both paths.
///
/// CODE: kernel/src/fs/ext2/inode.rs:424-462

PROTOCOL read_link {
    SATISFIES: layer1::read_link
    DISPATCH: always (single variant covers fast and slow)
    CODE: kernel/src/fs/ext2/inode.rs:424-462

    LOCKS: READ(self.inner)

    STEPS:
        1. GUARD   self.type_ == SymLink, else RETURN Err(EINVAL)
        2. EFFECT  fs = self.fs_arc()?
        3. EFFECT  block_size = fs.block_size()
        4. GUARD   block_size != 0, else RETURN Err(EIO)
        5. LOCK    inner = self.inner.read()
        6. EFFECT  link_size = inner.desc.size as usize

        // --- Fast symlink path ---
        IF inner.desc.is_fast_symlink(block_size):
            7a. read_len = min(link_size, MAX_FAST_SYMLINK_LEN - 1)
            8a. Serialize block_ptrs[] to LE bytes -> raw_bytes
            9a. target = String::from_utf8(raw_bytes[..read_len])?
            10a. UNLOCK inner (implicit drop)
            11a. RETURN Ok(target)

        // --- Slow symlink path ---
        7b. Allocate target buffer of link_size bytes
        8b. inner.page_cache.pages().read_bytes(0, &mut target)?
        9b. target = String::from_utf8(target)?
        10b. UNLOCK inner (implicit drop)
        11b. RETURN Ok(target)

    CRASH_WINDOWS: none (pure read, no writes)

    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:    type_ guard (step 1) + fs_arc liveness (step 2)
                => HOARE.PRE (SymLink, alive)
        POST:   fast path reads block_ptrs inline bytes = FS.data[ino]
                slow path reads page_cache[0..size] = FS.data[ino]
                => target = string(FS.data[ino])
        FRAME:  READ lock => no mutation => FS' = FS
        POST_ERR: no mutation on any error path => FS' = FS
        CRASH:  no writes => FS_recovered = FS.durable
    }
}

/// =============================================================================
/// PROTOCOL 2a: write_link_fast
/// =============================================================================
/// Fast symlink write: target (with NUL) fits in block_ptrs[] inline storage.
/// Single-phase: write block_ptrs + update size + persist, all under WRITE lock.
///
/// CODE: kernel/src/fs/ext2/inode.rs:489-508

PROTOCOL write_link_fast {
    SATISFIES: layer1::write_link
    DISPATCH: |target| + 1 <= MAX_FAST_SYMLINK_LEN
    CODE: kernel/src/fs/ext2/inode.rs:489-508

    LOCKS: WRITE(self.inner)

    STEPS:
        1. GUARD   self.type_ == SymLink, else RETURN Err(EINVAL)
        2. EFFECT  fs = self.fs_arc()?
        3. EFFECT  block_size = fs.block_size(); GUARD block_size != 0
        4. EFFECT  with_nul = target.len() + 1
        5. GUARD   with_nul <= block_size, else RETURN Err(ENAMETOOLONG)
        6. GUARD   with_nul <= MAX_FAST_SYMLINK_LEN (dispatch condition)
        7. LOCK    inner = self.inner.write()
        8. MUTATE  Copy target bytes into raw_bytes[0..target_len], zero-pad rest
        9. MUTATE  Deserialize raw_bytes into block_ptrs[] as LE u32 values
        10. MUTATE  inner.desc.size = target.len()
        11. MUTATE  inner.desc.blocks = 0
        12. PERSIST inner.persist_inode_and_sync(&fs)?
        13. UNLOCK  inner (implicit drop)
        14. RETURN  Ok(())

    CRASH_WINDOWS:
        W1: After steps 8-11 (in-memory mutation), before step 12 (persist)
            -- block_ptrs and size modified in memory only
            -- Crash here: reverts to FS.durable (old inode on disk)
        W2: During step 12 (persist_inode_and_sync)
            -- Inode being written to group inode-table page cache
            -- Crash here: FS_recovered in {FS.durable, FS'.durable}

    ROLLBACK:
        -- persist_inode_and_sync failure: in-memory state is dirty but
        -- no explicit rollback needed; caller sees Err, inode remains
        -- in modified-but-unpersisted state (will be retried or evicted)

    SATISFIES_PROOF {
        PRE:    type_ guard (step 1) => SymLink
                fs_arc (step 2) => alive
                with_nul <= block_size (step 5) => HOARE.PRE length bound
        POST.data:
                steps 8-9 write target bytes into block_ptrs[]
                ABSTRACTION_MAP: data[ino] for fast symlink = block_ptrs bytes
                => FS'.data[ino] = bytes(target)
        POST.size:
                step 10 sets desc.size = target.len()
                => FS'.inodes[ino].size = |target|
        POST.blocks:
                step 11 sets desc.blocks = 0
                => FS'.inodes[ino].blocks = 0
        FRAME:  only self.inner modified under WRITE lock
                no other inodes, dirs, or xattrs touched
        POST_ERR:
                EINVAL/ENAMETOOLONG return before any mutation
                persist failure: in-memory dirty but abstract FS' = FS
                (persist is the commit point; failure means not committed)
        CRASH:  W1 => FS.durable; W2 => {FS.durable, FS'.durable}
                subset of HOARE.CRASH fast path: {FS.durable, FS'.durable}
    }
}

/// =============================================================================
/// PROTOCOL 2b: write_link_slow
/// =============================================================================
/// Slow symlink write: target too large for inline block_ptrs, stored in data
/// blocks via page cache. Three-phase protocol with rollback on failure.
///
/// Phase 1 (WRITE): allocate blocks, resize page cache, update size
/// Phase 2 (UPREAD): write target bytes into page cache
/// Phase 3 (WRITE via upgrade): persist inode metadata
///
/// CODE: kernel/src/fs/ext2/inode.rs:510-565

PROTOCOL write_link_slow {
    SATISFIES: layer1::write_link
    DISPATCH: |target| + 1 > MAX_FAST_SYMLINK_LEN
    CODE: kernel/src/fs/ext2/inode.rs:510-565

    LOCKS: WRITE(self.inner) -> UPREAD(self.inner) -> WRITE(self.inner)

    STEPS:
        // --- Validation (before any lock) ---
        1. GUARD   self.type_ == SymLink, else RETURN Err(EINVAL)
        2. EFFECT  fs = self.fs_arc()?
        3. EFFECT  block_size = fs.block_size(); GUARD block_size != 0
        4. EFFECT  with_nul = target.len() + 1
        5. GUARD   with_nul <= block_size, else RETURN Err(ENAMETOOLONG)
        6. GUARD   with_nul > MAX_FAST_SYMLINK_LEN (dispatch condition)

        // --- Phase 1: Allocation (WRITE lock) ---
        7.  LOCK    inner = self.inner.write()
        8.  EFFECT  old_size = inner.desc.size as usize
        9.  EFFECT  end_block = target.len().div_ceil(block_size)
        10. FOR iblock in 0..end_block:
                inner.get_or_alloc_block(iblock, true)?
            // On error: goto ROLLBACK_P1
        11. MUTATE  inner.page_cache.resize(target.len().align_up(block_size))?
            // On error: goto ROLLBACK_P1
        12. MUTATE  inner.desc.size = target.len()
        13. UNLOCK  inner (drop)

        // --- Phase 2: Data write (UPREAD lock) ---
        14. LOCK    upread = self.inner.upread()
        15. EFFECT  upread.page_cache.pages().write_bytes(0, target.as_bytes())?
            // On error: upgrade -> ROLLBACK_P2

        // --- Phase 3: Persist (upgrade to WRITE) ---
        16. LOCK    inner = upread.upgrade()
        17. PERSIST inner.persist_inode_and_sync(&fs)?
        18. UNLOCK  inner (implicit drop)
        19. RETURN  Ok(())

    CRASH_WINDOWS:
        W1: During phase 1, after partial block allocation (steps 10-11)
            -- Some blocks allocated, page cache resized, size updated in memory
            -- Crash here: disk has old inode; allocated blocks are orphans
            -- fsck reclaims orphan blocks
            -- FS_recovered = FS.durable (with fsck cleanup)

        W2: After phase 1 completes (step 13), before phase 2 data write
            -- Blocks allocated, size set in memory, but data unwritten
            -- Crash here: disk has old inode; blocks are orphans
            -- FS_recovered = FS.durable

        W3: During phase 2 data write (step 15)
            -- Partial target bytes in page cache, not yet on disk
            -- Crash here: page cache lost, disk has old inode
            -- FS_recovered = FS.durable

        W4: During phase 3 persist (step 17)
            -- Inode being written to disk with new size/block pointers
            -- Data pages may or may not be flushed
            -- FS_recovered in {FS.durable, FS_partial, FS'.durable}

    ROLLBACK:
        ROLLBACK_P1 (phase 1 failure):
            write_failed_cleanup(&mut inner, old_size, target.len(), block_size)
            -- Discards page cache range [old_aligned..new_aligned)
            -- Resizes page cache back to old_aligned
            -- Truncates blocks back to old_size
            -- Restores desc.size = old_size
            CODE: kernel/src/fs/ext2/inode.rs:782-813

        ROLLBACK_P2 (phase 2 failure):
            inner = upread.upgrade()
            write_failed_cleanup(&mut inner, old_size, target.len(), block_size)
            -- Same cleanup as P1, but requires lock upgrade first
            CODE: kernel/src/fs/ext2/inode.rs:554-559

    SATISFIES_PROOF {
        PRE:    type_ guard (step 1) => SymLink
                fs_arc (step 2) => alive
                with_nul <= block_size (step 5) => HOARE.PRE length bound
        POST.data:
                step 15 writes target bytes at offset 0 in page cache
                ABSTRACTION_MAP: data[ino] = page_cache[0..size]
                => FS'.data[ino] = bytes(target)
        POST.size:
                step 12 sets desc.size = target.len()
                => FS'.inodes[ino].size = |target|
        POST.blocks:
                step 10 allocates ceil(|target|/block_size) blocks
                => FS'.inodes[ino].blocks >= blocks_for(|target|, block_size)
        FRAME:  only self.inner modified; no other inodes, dirs, xattrs touched
                type_, mode, uid, gid, links_count preserved (not written)
        POST_ERR:
                EINVAL/ENAMETOOLONG before any mutation => FS' = FS
                ROLLBACK_P1 restores old_size, frees blocks, discards cache
                ROLLBACK_P2 same via upgrade + cleanup
                => FS' = FS (observable state restored)
        CRASH:  W1-W3 => FS.durable (no persist reached disk)
                W4 => {FS.durable, FS_partial, FS'.durable}
                union = {FS.durable, FS_partial, FS'.durable}
                subset of HOARE.CRASH slow path
    }
}

/// =============================================================================
/// PROTOCOL 3: sync_all (fsync)
/// =============================================================================
/// Full fsync: data writeback + inode persist + device flush.
/// Three-step protocol with lock upgrade (UPREAD -> WRITE).
///
/// CODE: kernel/src/fs/ext2/inode.rs:1166-1184

PROTOCOL sync_all {
    SATISFIES: layer1::sync_all
    DISPATCH: always
    CODE: kernel/src/fs/ext2/inode.rs:1166-1184

    LOCKS: UPREAD(self.inner) -> WRITE(self.inner)

    STEPS:
        1. EFFECT  fs = self.fs_arc()?

        // Step 1: Flush dirty data pages (UPREAD lock)
        2. LOCK    upread = self.inner.upread()
        3. EFFECT  upread.sync_data()?
           // InodeInner::sync_data: evict_range(0..file_size)
           // Writes back dirty pages to block device page cache

        // Step 2: Persist inode metadata (upgrade to WRITE)
        4. LOCK    inner = upread.upgrade()
        5. PERSIST inner.persist_inode_and_sync(&fs)?
           // Serializes InodeDesc -> RawInode, writes to inode-table page cache
           // Calls sync_metadata: recomputes sb free counts, writes sb + gdescs

        6. UNLOCK  inner (implicit drop)

        // Step 3: Flush device write cache
        7. EFFECT  fs.block_device().sync()?
           // Issues device cache flush; after this, data is on stable storage

        8. RETURN  Ok(())

    CRASH_WINDOWS:
        W1: During step 3 (data page writeback)
            -- Dirty pages being written to block device page cache
            -- Crash here: some pages flushed, some not
            -- FS_recovered.data[ino] may be partially updated

        W2: During step 5 (persist_inode_and_sync)
            -- Inode metadata being written to inode-table page cache
            -- sync_metadata writing superblock + group descriptors
            -- Crash here: data pages on device, metadata partially written
            -- FS_recovered.inodes[ino] may be stale

        W3: Between step 6 and step 7 (after persist, before device flush)
            -- All writes issued to device write buffer but not flushed
            -- Crash here: device may lose buffered writes
            -- FS_recovered in {FS.durable, FS_partial}

        W4: During step 7 (device flush)
            -- Flush in progress; partial ordering of writes
            -- Crash here: FS_recovered in {FS_partial, FS'.durable}

    ROLLBACK: None -- sync operations are idempotent; no undo needed.

    SATISFIES_PROOF {
        PRE:    fs_arc (step 1) => alive
                no type_ restriction => HOARE.PRE (alive only)
        POST.durable:
                step 3 flushes data pages (evict_range)
                step 5 persists inode metadata (persist_inode_and_sync)
                step 7 flushes device cache
                => FS'.durable.inodes[ino] = FS'.inodes[ino]
                => FS'.durable.data[ino] = FS'.data[ino]
        POST.inmemory:
                sync does not modify in-memory inode or data content
                (only marks desc as clean via clear_dirty)
                => FS'.inodes[ino] = FS.inodes[ino], FS'.data[ino] = FS.data[ino]
        POST.sb:
                persist_inode_and_sync calls sync_metadata which recomputes
                free_blocks and free_inodes from group descriptors
                => FS'.sb counters = actual counts
        FRAME:  only self.inner accessed; no other inodes modified
        POST_ERR:
                EIO on fs_arc, sync_data, persist, or device sync
                no mutation on error => FS' = FS
        CRASH:  W1-W4 produce states in {FS.durable, FS_partial, FS'.durable}
                matches HOARE.CRASH set
    }
}

/// =============================================================================
/// PROTOCOL 4: sync_data (fdatasync)
/// =============================================================================
/// Data-only sync: data writeback + conditional metadata persist + device flush.
/// Uses WRITE lock (not UPREAD->WRITE like sync_all) because metadata persist
/// decision requires checking desc.is_dirty() under exclusive access.
///
/// CODE: kernel/src/fs/ext2/inode.rs:1214-1236

PROTOCOL sync_data {
    SATISFIES: layer1::sync_data
    DISPATCH: always
    CODE: kernel/src/fs/ext2/inode.rs:1214-1236

    LOCKS: WRITE(self.inner)

    STEPS:
        1. EFFECT  fs = self.fs_arc()?

        // Step 1: Flush dirty data pages + conditional metadata (WRITE lock)
        2. LOCK    inner = self.inner.write()
        3. EFFECT  inner.sync_data()?
           // InodeInner::sync_data: evict_range(0..file_size)
           // No-op if file_size == 0

        // Step 2: Persist metadata only if dirty
        4. IF inner.desc.is_dirty():
               PERSIST inner.persist_inode_and_sync(&fs)?

        5. UNLOCK  inner (implicit drop)

        // Step 3: Flush device write cache
        6. EFFECT  fs.block_device().sync()?

        7. RETURN  Ok(())

    CRASH_WINDOWS:
        W1: During step 3 (data page writeback)
            -- Dirty pages being written back via evict_range
            -- Crash here: partial data pages on disk
            -- FS_recovered.data[ino] partially updated

        W2: During step 4 (conditional metadata persist)
            -- Only entered if desc.is_dirty()
            -- Inode metadata being written to inode-table page cache
            -- Crash here: data flushed, metadata partially written
            -- FS_recovered.inodes[ino] may be stale

        W3: Between step 5 and step 6 (after persist, before device flush)
            -- Writes in device buffer but not flushed
            -- Crash here: device may lose buffered writes

        W4: During step 6 (device flush)
            -- Flush in progress
            -- Crash here: FS_recovered in {FS_partial, FS'.durable}

    ROLLBACK: None -- sync operations are idempotent; no undo needed.

    SATISFIES_PROOF {
        PRE:    fs_arc (step 1) => alive
                no type_ restriction => HOARE.PRE (alive only)
        POST.data_durable:
                step 3 flushes data pages (evict_range)
                step 6 flushes device cache
                => FS'.durable.data[ino] = FS'.data[ino]
        POST.metadata_durable:
                step 4 conditionally persists if desc.is_dirty()
                desc.is_dirty() is conservative proxy for I_DIRTY_DATASYNC
                => IF metadata_was_dirty: FS'.durable.inodes[ino] = FS'.inodes[ino]
        POST.inmemory:
                sync does not modify in-memory content
                => FS'.inodes[ino] = FS.inodes[ino], FS'.data[ino] = FS.data[ino]
        FRAME:  only self.inner accessed; no other inodes modified
        POST_ERR:
                EIO on fs_arc, sync_data, persist, or device sync
                no mutation on error => FS' = FS
        CRASH:  W1-W4 produce states in {FS.durable, FS_partial, FS'.durable}
                metadata may be stale in partial states (not persisted if clean)
                matches HOARE.CRASH set
    }
}

/// =============================================================================
/// PROTOCOL 5a: fallocate_punch_hole
/// =============================================================================
/// PunchHoleKeepSize mode: zero-fills a range in page cache without changing
/// file size. Page-cache-only operation; not durable until synced.
///
/// CODE: kernel/src/fs/ext2/inode.rs:1140-1148

PROTOCOL fallocate_punch_hole {
    SATISFIES: layer1::fallocate (CASE PunchHoleKeepSize)
    DISPATCH: mode = PunchHoleKeepSize
    CODE: kernel/src/fs/ext2/inode.rs:1140-1148

    LOCKS: READ(self.inner)

    STEPS:
        1. LOCK    inner = self.inner.read()
        2. EFFECT  file_size = inner.desc.size as usize
        3. GUARD   offset < file_size, else RETURN Ok(())  -- no-op beyond EOF
        4. EFFECT  end = min(file_size, offset + len)
        5. EFFECT  inner.page_cache.fill_zeros(offset..end)?
        6. UNLOCK  inner (implicit drop)
        7. RETURN  Ok(())

    CRASH_WINDOWS: none
        -- fill_zeros writes to page cache only (in-memory)
        -- Crash at any point: zeros lost, original data restored from disk

    ROLLBACK: N/A -- page cache zeros are idempotent; no persistent mutation.

    SATISFIES_PROOF {
        PRE:    READ lock acquired => alive
        POST:   offset >= file_size => early return Ok, FS' = FS (no-op case)
                offset < file_size => fill_zeros zeroes [offset..end) in page cache
                ABSTRACTION_MAP: data[ino] = page_cache[0..size]
                => FS'.data[ino][offset..end] = zeros
                size unchanged (READ lock, no desc mutation)
        FRAME:  READ lock => no inode metadata mutation; no other inodes touched
        POST_ERR: EIO from fill_zeros; no persistent mutation => FS' = FS
        CRASH:  page cache only => FS_recovered = FS.durable
    }
}

/// =============================================================================
/// PROTOCOL 5b: fallocate_allocate
/// =============================================================================
/// Allocate mode: extends file size to at least offset+len by delegating to
/// the resize protocol. No-op if file is already large enough.
///
/// CODE: kernel/src/fs/ext2/inode.rs:1149-1155

PROTOCOL fallocate_allocate {
    SATISFIES: layer1::fallocate (CASE Allocate)
    DISPATCH: mode = Allocate
    CODE: kernel/src/fs/ext2/inode.rs:1149-1155

    LOCKS: delegates to resize() -- see resize protocol for lock details

    STEPS:
        1. EFFECT  new_size = offset + len
        2. IF new_size > self.file_size():
               self.resize(new_size)?
               // Delegates entirely to resize protocol
        3. RETURN  Ok(())

    CRASH_WINDOWS:
        -- If new_size <= current size: no-op, no crash windows
        -- If new_size > current size: delegates to resize crash windows
        --   Partial block allocation possible during resize
        --   See resize protocol for detailed crash analysis

    ROLLBACK: delegates to resize rollback (write_failed_cleanup)

    SATISFIES_PROOF {
        PRE:    file_size() reads desc.size => alive
        POST:   new_size <= current => no-op, FS' = FS
                new_size > current => resize sets size = new_size,
                allocates blocks => FS'.inodes[ino].size = offset + len
        FRAME:  resize only modifies self.inner; no other inodes touched
        POST_ERR: resize rollback restores old state => FS' = FS
                  ENOSPC if block allocation fails
        CRASH:  no-op case => FS_recovered = FS.durable
                resize case => {FS.durable, FS_partial, FS'.durable}
                matches HOARE.CRASH Allocate case
    }
}

/// =============================================================================
/// PROTOCOL 5c: fallocate_keep_size_and_unsupported
/// =============================================================================
/// AllocateKeepSize: no-op (Asterinas does not pre-allocate without extending).
/// Unsupported modes: return EOPNOTSUPP.
///
/// CODE: kernel/src/fs/ext2/inode.rs:1156-1163

PROTOCOL fallocate_keep_size_and_unsupported {
    SATISFIES: layer1::fallocate (CASE AllocateKeepSize, DEFAULT)
    DISPATCH: mode in {AllocateKeepSize, unsupported modes}
    CODE: kernel/src/fs/ext2/inode.rs:1156-1163

    LOCKS: none

    STEPS:
        CASE mode = AllocateKeepSize:
            1. RETURN Ok(())    -- immediate no-op

        CASE mode = unsupported:
            1. RETURN Err(EOPNOTSUPP)

    CRASH_WINDOWS: none -- no state mutation in either case

    ROLLBACK: N/A

    SATISFIES_PROOF {
        PRE:    trivially satisfied (no locks, no reads)
        POST:   AllocateKeepSize => FS' = FS (no-op)
        FRAME:  no state accessed or modified
        POST_ERR: unsupported => EOPNOTSUPP, FS' = FS
        CRASH:  no writes => FS_recovered = FS.durable
    }
}
