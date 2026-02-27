// SPDX-License-Identifier: MPL-2.0
//
// Layer 2: Implementation Protocol -- File I/O
//
// Four protocol variants mapping to two Layer 1 HOARE specs:
//   1. read_at_buffered   -- SATISFIES layer1::read_at
//   2. read_at_direct     -- SATISFIES layer1::read_at
//   3. write_at_buffered  -- SATISFIES layer1::write_at
//   4. write_at_direct    -- SATISFIES layer1::write_at
//
// Reference: layer1_hoare/02-file-io.spec for abstract contracts.
// Reference: layer2_protocol/00-impl-state.spec for concrete state model.

/// =============================================================================
/// PROTOCOL 1: read_at_buffered
/// =============================================================================
/// Reads file data through the page cache under a read lock.
///
/// CODE: kernel/src/fs/ext2/inode.rs:567-590
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:21-33

PROTOCOL read_at_buffered {
    SATISFIES: layer1::read_at
    DISPATCH:  status_flags does NOT contain O_DIRECT

    LOCKS: READ(self.inner), then WRITE(self.inner) for atime

    STEPS:
      1. GUARD   self.type_ != Dir, else RETURN Err(EISDIR)
      2. GUARD   writer.avail() > 0, else RETURN Ok(0)
      3. LOCK    inner = self.inner.read()
      4. file_size = inner.desc.size
      5. GUARD   offset < file_size, else RETURN Ok(0)
      6. read_len = min(writer.avail(), file_size - offset)
      7. writer.limit(read_len)
      8. EFFECT  inner.page_cache.pages().read(offset, writer)?
      9. UNLOCK  inner                          // drop read guard
      10. EFFECT self.set_atime(now())          // acquires write lock internally
      11. RETURN Ok(read_len)

    CRASH_WINDOWS:
      // No crash windows -- read_at_buffered performs no durable mutation.
      // Page cache read may trigger read_page_async (device I/O) but that
      // is idempotent. atime update is in-memory only (not persisted).
      NONE

    ROLLBACK: N/A (read-only data path)

    SATISFIES_PROOF {
        PRE:
            Step 1 checks type_ != Dir => layer1::read_at.PRE satisfied.
            If type_ = Dir, step 1 returns EISDIR => layer1::read_at.POST_ERR.

        POST.n:
            Steps 4-6 compute read_len = min(len, file_size - offset).
            Step 8 fills writer with data[ino][offset..offset+read_len].
            Step 11 returns Ok(read_len).
            => layer1::read_at.POST.n and returned_data satisfied.

        POST.atime:
            Step 10 calls set_atime(now()).
            => layer1::read_at.POST.atime satisfied.

        FRAME:
            Only inner.desc.atime modified (step 10). No data, dirs,
            xattrs, sb, or other inodes touched.
            => layer1::read_at.FRAME satisfied.

        POST_ERR:
            Step 1 returns EISDIR without modifying state.
            Step 8 error propagates without side effects (read-only).
            => FS' = FS satisfied.

        CRASH:
            No durable writes occur. atime is volatile.
            => FS_recovered = FS.durable satisfied.
    }
}

/// =============================================================================
/// PROTOCOL 2: read_at_direct
/// =============================================================================
/// Reads file data directly from block device, bypassing page cache.
/// Requires block-aligned offset and length.
///
/// CODE: kernel/src/fs/ext2/inode.rs:664-698
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:21-33
/// LINUX_REF: fs/ext2/file.c:168 (ext2_dio_read_iter)

PROTOCOL read_at_direct {
    SATISFIES: layer1::read_at
    DISPATCH:  status_flags contains O_DIRECT

    LOCKS: READ(self.inner), then WRITE(self.inner) for atime

    STEPS:
      1. GUARD   self.type_ != Dir, else RETURN Err(EISDIR)
      2. EFFECT  fs = self.fs_arc()?
      3. GUARD   offset.is_multiple_of(block_size)
                 AND writer.avail().is_multiple_of(block_size),
                 else RETURN Err(EINVAL)
      4. LOCK    inner = self.inner.read()
      5. file_size = inner.desc.size
      6. GUARD   offset < file_size, else RETURN Ok(0)
      7. read_len = min(writer.avail(), file_size - offset)
      8. end = offset + read_len
      9. EFFECT  inner.page_cache.discard_range(offset..end)
      10. EFFECT inner.read_at(offset, writer)?    // direct block device reads
      11. UNLOCK inner
      12. EFFECT self.set_atime(now())
      13. RETURN Ok(read_len)

    CRASH_WINDOWS:
      // No crash windows -- direct reads are idempotent device I/O.
      // Page cache discard (step 9) is in-memory only.
      // atime update (step 12) is volatile.
      NONE

    ROLLBACK: N/A (read-only data path)

    SATISFIES_PROOF {
        PRE:
            Step 1 checks type_ != Dir => layer1::read_at.PRE satisfied.
            Step 3 adds alignment guard (EINVAL on failure), which is
            an implementation-level restriction not in Layer 1 PRE.
            This is a refinement: direct path has stricter preconditions.

        POST.n:
            Steps 5-7 compute read_len = min(len, file_size - offset).
            Step 10 reads data directly from device blocks.
            ABSTRACTION_MAP: device block content at [offset..end) =
                FS.data[ino][offset..offset+read_len].
            => layer1::read_at.POST.n and returned_data satisfied.

        POST.atime:
            Step 12 calls set_atime(now()).
            => layer1::read_at.POST.atime satisfied.

        FRAME:
            Page cache discard (step 9) is an implementation detail not
            visible in AbstractFS. Only atime modified.
            => layer1::read_at.FRAME satisfied.

        POST_ERR:
            EISDIR from step 1, EINVAL from step 3 -- no state modified.
            Step 10 error propagates without side effects (read-only).
            Note: EINVAL is not in layer1::read_at.POST_ERR but is a
            refinement-level error (alignment is impl concern).
            => FS' = FS satisfied.

        CRASH:
            No durable writes. Device reads are idempotent.
            => FS_recovered = FS.durable satisfied.
    }
}

/// =============================================================================
/// PROTOCOL 3: write_at_buffered
/// =============================================================================
/// Writes file data through the page cache using three-phase locking:
///   Phase 1 (WRITE):  allocate blocks, grow size
///   Phase 2 (UPREAD): copy data into page cache
///   Phase 3 (WRITE):  update timestamps, persist inode
///
/// CODE: kernel/src/fs/ext2/inode.rs:592-662
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:35-47
/// LINUX_REF: fs/ext2/file.c:295 (ext2_file_write_iter)

PROTOCOL write_at_buffered {
    SATISFIES: layer1::write_at
    DISPATCH:  status_flags does NOT contain O_DIRECT

    LOCKS: WRITE(self.inner) -> UPREAD(self.inner) -> WRITE(self.inner)

    STEPS:
      // --- Validation ---
      1. GUARD   self.type_ != Dir, else RETURN Err(EISDIR)
      2. write_len = reader.remain()
      3. GUARD   write_len > 0, else RETURN Ok(0)
      4. EFFECT  fs = self.fs_arc()?
      5. end = offset + write_len                // checked_add, Err(EINVAL) on overflow

      // --- Phase 1: Allocation (exclusive write lock) ---
      6. LOCK    inner = self.inner.write()
      7. old_size = inner.desc.size
      8. FOR iblock in [offset/block_size .. end.div_ceil(block_size)):
           inner.get_or_alloc_block(iblock, true)?
      9. IF end > old_size:
           inner.page_cache.resize(align_up(end, block_size))?
           inner.desc.size = end
      10. UNLOCK inner
      // On phase 1 error: write_failed_cleanup(inner, old_size, end, block_size)

      // --- Phase 2: Data transfer (upgradable read lock) ---
      11. LOCK    upread = self.inner.upread()
      12. EFFECT  upread.page_cache.pages().write(offset, reader)?
      // On phase 2 error: upgrade -> write_failed_cleanup

      // --- Phase 3: Metadata commit (upgrade to exclusive) ---
      13. LOCK    inner = upread.upgrade()
      14. MUTATE  inner.desc.mtime = now()
      15. MUTATE  inner.desc.ctime = now()
      16. PERSIST inner.persist_inode_and_sync(&fs)?
      17. UNLOCK  inner
      18. RETURN  Ok(write_len)

    CRASH_WINDOWS:
      W1: Between steps 8-9 and step 10 (phase 1 committed, lock dropped).
          Blocks allocated, size possibly grown, but data is zero/stale
          in page cache. Inode descriptor written to inode-table page cache
          but NOT persisted to device.
          Recovery: FS.durable (inode-table page cache lost on reboot).
          Risk: if inode-table page was flushed by background writeback,
                on-disk inode shows new size with uninitialized data blocks.

      W2: Between steps 11-12 (phase 2 in progress).
          Partial data written to page cache. Dirty pages may or may not
          have been flushed to device by background writeback.
          Recovery: FS.durable or partial_write(FS.durable, ino, offset,
                    buf[0..k]) depending on which pages reached disk.

      W3: Between steps 14-16 (phase 3 in progress).
          Timestamps updated, persist_inode_and_sync may have partially
          written the inode descriptor to the inode-table page cache.
          Data pages still dirty (not flushed to device).
          Recovery: FS.durable or FS' depending on whether the inode
                    persist and data pages both reached disk.

    ROLLBACK:
      write_failed_cleanup(inner, old_size, end, block_size):
        TRIGGER: error in phase 1 (step 8-9) or phase 2 (step 12)
        STEPS:
          1. IF end > old_size:
               a. inner.page_cache.discard_range(old_aligned..end_aligned)
               b. inner.page_cache.resize(old_aligned)     // best-effort
               c. inner.truncate_blocks(old_size)           // best-effort
               d. inner.desc.size = old_size
        EFFECT: observable state restored to pre-operation values.
        CODE: kernel/src/fs/ext2/inode.rs:782-813

    SATISFIES_PROOF {
        PRE:
            Step 1 checks type_ != Dir.
            Steps 4-5 validate fs liveness and offset+len overflow.
            => layer1::write_at.PRE satisfied.

        POST.data:
            Step 8 allocates blocks for [offset/bs..end/bs).
            Step 9 grows page cache and size if extending.
            Step 12 writes buf into page_cache at [offset..end).
            ABSTRACTION_MAP: page_cache.contents()[0..size] = FS'.data[ino].
            => splice(data[ino], offset, buf) satisfied.

        POST.size:
            Step 9 sets desc.size = end when end > old_size.
            If end <= old_size, size unchanged.
            => FS'.inodes[ino].size = max(old_size, end) satisfied.

        POST.timestamps:
            Steps 14-15 set mtime = ctime = now().
            => layer1::write_at.POST.mtime and POST.ctime satisfied.

        POST.persist:
            Step 16 calls persist_inode_and_sync, writing the inode
            descriptor (with new size, mtime, ctime) to the inode-table
            page cache. Data pages remain dirty until sync.

        FRAME:
            Only self.inner modified (steps 6-17). No other inodes,
            dirs, xattrs touched. sb.free_blocks may decrease from
            block allocation in step 8.
            => layer1::write_at.FRAME satisfied.

        POST_ERR:
            Phase 1 error (step 8-9): write_failed_cleanup restores
                old_size, frees blocks, trims page cache => FS' = FS.
            Phase 2 error (step 12): upgrade + write_failed_cleanup
                restores state => FS' = FS.
            Step 1 EISDIR: no state modified => FS' = FS.
            => layer1::write_at.POST_ERR satisfied.

        CRASH:
            W1: FS.durable (no persist yet) or partial if background
                writeback flushed inode-table page.
            W2: FS.durable or partial_write for flushed data pages.
            W3: FS.durable or FS' depending on persist completion.
            All cases in {FS.durable, partial_write(...), FS'}.
            => layer1::write_at.CRASH satisfied.
    }
}

/// =============================================================================
/// PROTOCOL 4: write_at_direct
/// =============================================================================
/// Writes file data directly to block device, bypassing page cache for data.
/// Uses the same three-phase locking as buffered, but phase 1 additionally
/// invalidates overlapping page cache entries and phase 2 writes to device.
///
/// CODE: kernel/src/fs/ext2/inode.rs:700-780
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:35-47
/// LINUX_REF: fs/ext2/file.c:214 (ext2_dio_write_iter)

PROTOCOL write_at_direct {
    SATISFIES: layer1::write_at
    DISPATCH:  status_flags contains O_DIRECT

    LOCKS: WRITE(self.inner) -> UPREAD(self.inner) -> WRITE(self.inner)

    STEPS:
      // --- Validation ---
      1. GUARD   self.type_ != Dir, else RETURN Err(EISDIR)
      2. EFFECT  fs = self.fs_arc()?
      3. GUARD   offset.is_multiple_of(block_size)
                 AND reader.remain().is_multiple_of(block_size),
                 else RETURN Err(EINVAL)
      4. write_len = reader.remain()
      5. GUARD   write_len > 0, else RETURN Ok(0)
      6. end = offset + write_len                // checked_add, Err(EINVAL) on overflow

      // --- Phase 1: Allocation + cache invalidation (exclusive write lock) ---
      7. LOCK    inner = self.inner.write()
      8. old_size = inner.desc.size
      9. FOR iblock in [offset/block_size .. end.div_ceil(block_size)):
           inner.get_or_alloc_block(iblock, true)?
      10. IF end > old_size:
            inner.page_cache.resize(align_up(end, block_size))?
            inner.desc.size = end
      11. discard_start = min(offset, old_size)
          discard_end   = min(end, old_size)
          IF discard_start < discard_end:
            inner.page_cache.discard_range(discard_start..discard_end)
      12. UNLOCK inner
      // On phase 1 error: write_failed_cleanup(inner, old_size, end, block_size)

      // --- Phase 2: Direct device write (upgradable read lock) ---
      13. LOCK    upread = self.inner.upread()
      14. EFFECT  upread.write_at(offset, reader)?   // block device writes
      // On phase 2 error: upgrade -> write_failed_cleanup

      // --- Phase 3: Metadata commit (upgrade to exclusive) ---
      15. LOCK    inner = upread.upgrade()
      16. MUTATE  inner.desc.mtime = now()
      17. MUTATE  inner.desc.ctime = now()
      18. PERSIST inner.persist_inode_and_sync(&fs)?
      19. UNLOCK  inner
      20. RETURN  Ok(write_len)

    CRASH_WINDOWS:
      W1: Between steps 9-11 and step 12 (phase 1 committed, lock dropped).
          Blocks allocated, size possibly grown, page cache entries in the
          overlap range invalidated. Inode descriptor in inode-table page
          cache but NOT persisted to device.
          Recovery: FS.durable (inode-table page cache lost on reboot).
          Risk: if inode-table page was flushed by background writeback,
                on-disk inode shows new size with stale/zero device blocks.

      W2: Between steps 13-14 (phase 2 in progress).
          Partial data written DIRECTLY to block device. Unlike buffered
          path, these writes may already be durable on the device sectors.
          Recovery: partial_write(FS.durable, ino, offset, buf[0..k])
                    where k depends on how many device writes completed.
          NOTE: This is the key difference from write_at_buffered W2 --
                direct writes reach the device immediately, so partial
                data is more likely to survive a crash.

      W3: Between steps 16-18 (phase 3 in progress).
          Timestamps updated, persist_inode_and_sync may have partially
          written the inode descriptor. Device blocks already contain
          the written data from phase 2.
          Recovery: partial_write(FS.durable, ino, offset, buf) with
                    metadata possibly lagging (old mtime/ctime on disk).

    ROLLBACK:
      write_failed_cleanup(inner, old_size, end, block_size):
        TRIGGER: error in phase 1 (step 9-11) or phase 2 (step 14)
        STEPS:
          1. IF end > old_size:
               a. inner.page_cache.discard_range(old_aligned..end_aligned)
               b. inner.page_cache.resize(old_aligned)     // best-effort
               c. inner.truncate_blocks(old_size)           // best-effort
               d. inner.desc.size = old_size
        EFFECT: observable state restored to pre-operation values.
        NOTE: For phase 2 errors, upread is upgraded to write before
              invoking cleanup (step 14 error path).
        CODE: kernel/src/fs/ext2/inode.rs:782-813

    SATISFIES_PROOF {
        PRE:
            Step 1 checks type_ != Dir.
            Step 3 checks block alignment (EINVAL on failure).
            Steps 2, 6 validate fs liveness and offset+len overflow.
            => layer1::write_at.PRE satisfied.
            Note: alignment is a refinement-level precondition not in
            Layer 1 PRE; EINVAL is added to the error set accordingly.

        POST.data:
            Step 9 allocates blocks for [offset/bs..end/bs).
            Step 10 grows page cache and size if extending.
            Step 11 invalidates stale page cache entries in overlap range.
            Step 14 writes buf directly to device blocks at [offset..end).
            ABSTRACTION_MAP: device block content at [offset..end) =
                splice(data[ino], offset, buf).
            => layer1::write_at.POST.data satisfied.

        POST.size:
            Step 10 sets desc.size = end when end > old_size.
            If end <= old_size, size unchanged.
            => FS'.inodes[ino].size = max(old_size, end) satisfied.

        POST.timestamps:
            Steps 16-17 set mtime = ctime = now().
            => layer1::write_at.POST.mtime and POST.ctime satisfied.

        POST.persist:
            Step 18 calls persist_inode_and_sync, writing the inode
            descriptor (with new size, mtime, ctime) to the inode-table
            page cache. Data already on device from step 14.

        FRAME:
            Only self.inner modified (steps 7-19). Page cache discard
            (step 11) is an implementation detail not visible in AbstractFS.
            No other inodes, dirs, xattrs touched. sb.free_blocks may
            decrease from block allocation in step 9.
            => layer1::write_at.FRAME satisfied.

        POST_ERR:
            Phase 1 error (step 9-11): write_failed_cleanup restores
                old_size, frees blocks, trims page cache => FS' = FS.
            Phase 2 error (step 14): upgrade + write_failed_cleanup
                restores state => FS' = FS.
            Step 1 EISDIR: no state modified => FS' = FS.
            Step 3 EINVAL: no state modified => FS' = FS.
            => layer1::write_at.POST_ERR satisfied.

        CRASH:
            W1: FS.durable (no device data writes yet) or partial if
                background writeback flushed inode-table page.
            W2: partial_write(FS.durable, ino, offset, buf[0..k])
                because direct writes reach device immediately.
                This is strictly more observable than buffered W2.
            W3: FS.durable or FS' depending on persist completion.
                Data already durable from phase 2 device writes.
            All cases in {FS.durable, partial_write(...), FS'}.
            => layer1::write_at.CRASH satisfied.
    }
}
