// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Tier 3: write_at (buffered + direct)
//
// write_at is the most complex single-inode protocol. It uses a two-phase
// approach: write lock for allocation, upread for data transfer, upgrade
// for metadata persist. Both buffered and direct paths share this pattern.
//
// Reference: 00-state-model.spec for state notation.

/// =============================================================================
/// PROTOCOL: write_at (buffered)
/// =============================================================================
/// Writes file data through the page cache with two-phase locking.
///
/// CODE: kernel/src/fs/ext2/inode.rs:592-662
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:35-47
/// LINUX_REF: fs/ext2/file.c:295 (ext2_file_write_iter)

PROTOCOL write_at_buffered {
    TIER: 3
    SIGNATURE: fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize>
    DISPATCH: status_flags does NOT contain O_DIRECT

    REQUIRE:
      self.type_ ≠ Dir

    STEPS:
      // --- Validation ---
      1. GUARD   self.type_ ≠ Dir, else RETURN Err(EISDIR)
      2. write_len = reader.remain()
      3. GUARD   write_len > 0, else RETURN Ok(0)
      4. EFFECT  fs = self.fs_arc()?
      5. end = offset + write_len

      // --- Phase 1: Allocation (write lock) ---
      6. LOCK    inner = self.inner.write()
      7. old_size = inner.desc.size
      8. FOR iblock in [offset/block_size .. end.div_ceil(block_size)):
           inner.get_or_alloc_block(iblock, true)?
      9. IF end > old_size:
           inner.page_cache.resize(align_up(end, block_size))?
           inner.desc.size = end
      10. UNLOCK  inner
      // On phase 1 error: write_failed_cleanup(old_size, end, block_size)

      // --- Phase 2: Data transfer (upread) ---
      11. LOCK    upread = self.inner.upread()
      12. EFFECT  upread.page_cache.pages().write(offset, reader)?
      // On phase 2 error: upgrade → write_failed_cleanup

      // --- Phase 3: Metadata commit (upgrade to write) ---
      13. LOCK    inner = upread.upgrade()
      14. MUTATE  inner.desc.mtime = now()
      15. MUTATE  inner.desc.ctime = now()
      16. PERSIST inner.persist_inode_and_sync(&fs)?
      17. UNLOCK  inner
      18. RETURN  Ok(write_len)

    LOCKS: WRITE(self.inner) → UPREAD(self.inner) → WRITE(self.inner)

    CRASH_ANALYSIS:
      - Crash after phase 1, before phase 2:
        Blocks allocated, size grown, but data is zero/stale.
        On reboot: inode shows new size with uninitialized data.
        fsck may detect inconsistency.
      - Crash during phase 2:
        Partial data in page cache (dirty pages).
        On reboot: old on-disk state restored.
      - Crash after phase 3:
        Fully committed. Data in page cache may not be on disk
        until sync_all/sync_data.

    ROLLBACK:
      write_failed_cleanup(inner, old_size, end, block_size):
        IF end > old_size:
          inner.page_cache.discard_range(old_aligned..end_aligned)
          inner.page_cache.resize(old_aligned)  // best-effort
          inner.truncate_blocks(old_size)        // best-effort
          inner.desc.size = old_size
      CODE: kernel/src/fs/ext2/inode.rs:782-813

    ENSURE:
      S'.M.inode[self].desc.size = max(S.M.inode[self].desc.size, offset + write_len)
      S'.M.page_cache[self] contains written data at [offset, offset+write_len)
      S'.M.inode[self].desc.mtime = now()
      S'.M.inode[self].desc.ctime = now()
      Blocks allocated for [offset/bs .. end.div_ceil(bs))
    ENSURE_ERR:
      EISDIR if self is a directory
      S'.M observable state = S.M observable state (via rollback)
}

/// =============================================================================
/// PROTOCOL: write_at (direct I/O)
/// =============================================================================
/// Writes file data directly to block device, bypassing page cache for data.
///
/// CODE: kernel/src/fs/ext2/inode.rs:700-780
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:35-47
/// LINUX_REF: fs/ext2/file.c:214 (ext2_dio_write_iter)

PROTOCOL write_at_direct {
    TIER: 3
    SIGNATURE: fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize>
    DISPATCH: status_flags contains O_DIRECT

    REQUIRE:
      self.type_ ≠ Dir
      offset is block-aligned
      reader.remain() is block-aligned

    STEPS:
      // --- Validation ---
      1. GUARD   self.type_ ≠ Dir, else RETURN Err(EISDIR)
      2. EFFECT  fs = self.fs_arc()?
      3. GUARD   offset.is_multiple_of(block_size) ∧ remain.is_multiple_of(block_size),
                 else RETURN Err(EINVAL)
      4. write_len = reader.remain()
      5. GUARD   write_len > 0, else RETURN Ok(0)
      6. end = offset + write_len

      // --- Phase 1: Allocation + cache invalidation (write lock) ---
      7. LOCK    inner = self.inner.write()
      8. old_size = inner.desc.size
      9. FOR iblock in [offset/bs .. end.div_ceil(bs)):
           inner.get_or_alloc_block(iblock, true)?
      10. IF end > old_size:
            inner.page_cache.resize(align_up(end, block_size))?
            inner.desc.size = end
      11. EFFECT  inner.page_cache.discard_range(
                    min(offset, old_size)..min(end, old_size))
      12. UNLOCK  inner
      // On phase 1 error: write_failed_cleanup

      // --- Phase 2: Direct device write (upread) ---
      13. LOCK    upread = self.inner.upread()
      14. EFFECT  upread.write_at(offset, reader)?  // block device writes
      // On phase 2 error: upgrade → write_failed_cleanup

      // --- Phase 3: Metadata commit (upgrade) ---
      15. LOCK    inner = upread.upgrade()
      16. MUTATE  inner.desc.mtime = now()
      17. MUTATE  inner.desc.ctime = now()
      18. PERSIST inner.persist_inode_and_sync(&fs)?
      19. UNLOCK  inner
      20. RETURN  Ok(write_len)

    LOCKS: WRITE(self.inner) → UPREAD(self.inner) → WRITE(self.inner)

    CRASH_ANALYSIS:
      - Crash after phase 1: blocks allocated, cache invalidated,
        but data not written. Stale/zero data visible.
      - Crash during phase 2: partial device writes possible.
        Unlike buffered path, data goes directly to device.
      - Crash after phase 3: fully committed to device.

    ROLLBACK: Same as buffered write_at — write_failed_cleanup.

    ENSURE:
      S'.M.inode[self].desc.size = max(old_size, end)
      Device blocks contain written data at [offset, end)
      Page cache entries in overlap range are invalidated
      S'.M.inode[self].desc.mtime = now()
      S'.M.inode[self].desc.ctime = now()
    ENSURE_ERR:
      EISDIR  if directory
      EINVAL  if not block-aligned
      S'.M observable state = S.M observable state (via rollback)
}
