// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Tier 3: read_at (buffered + direct)
//
// read_at is a multi-step single-inode protocol with two paths:
//   - Buffered: page cache read under read lock
//   - Direct I/O: block-aligned device read with cache invalidation
//
// Reference: 00-state-model.spec for state notation.

/// =============================================================================
/// PROTOCOL: read_at (buffered)
/// =============================================================================
/// Reads file data through the page cache.
///
/// CODE: kernel/src/fs/ext2/inode.rs:567-590
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:21-33
/// LINUX_REF: fs/ext2/file.c:283 (ext2_file_read_iter)

PROTOCOL read_at_buffered {
    TIER: 3
    SIGNATURE: fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize>
    DISPATCH: status_flags does NOT contain O_DIRECT

    REQUIRE:
      self.type_ ≠ Dir

    STEPS:
      1. GUARD   self.type_ ≠ Dir, else RETURN Err(EISDIR)
      2. GUARD   writer.avail() > 0, else RETURN Ok(0)
      3. LOCK    inner = self.inner.read()
      4. file_size = inner.desc.size
      5. GUARD   offset < file_size, else RETURN Ok(0)
      6. read_len = min(writer.avail(), file_size - offset)
      7. writer.limit(read_len)
      8. EFFECT  inner.page_cache.pages().read(offset, writer)?
      9. UNLOCK  inner
      10. EFFECT self.set_atime(now())
      11. RETURN Ok(read_len)

    LOCKS: READ(self.inner), then WRITE(self.inner) for atime

    CRASH_ANALYSIS:
      - No persist step — atime update is in-memory only
      - Page cache read may trigger read_page_async (device I/O)
      - Crash during read: no state change

    ROLLBACK: N/A (read-only data path)

    ENSURE:
      writer contains file data from [offset, offset+read_len)
      S'.M.inode[self].desc.atime = now()
      All other state unchanged
    ENSURE_ERR:
      EISDIR if self is a directory
      S' = S (no side effects on error)
}

/// =============================================================================
/// PROTOCOL: read_at (direct I/O)
/// =============================================================================
/// Reads file data directly from block device, bypassing page cache.
///
/// CODE: kernel/src/fs/ext2/inode.rs:664-698
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:21-33
/// LINUX_REF: fs/ext2/file.c:168 (ext2_dio_read_iter)

PROTOCOL read_at_direct {
    TIER: 3
    SIGNATURE: fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize>
    DISPATCH: status_flags contains O_DIRECT

    REQUIRE:
      self.type_ ≠ Dir
      offset is block-aligned
      writer.avail() is block-aligned

    STEPS:
      1. GUARD   self.type_ ≠ Dir, else RETURN Err(EISDIR)
      2. EFFECT  fs = self.fs_arc()?
      3. GUARD   offset.is_multiple_of(block_size) ∧ avail.is_multiple_of(block_size),
                 else RETURN Err(EINVAL)
      4. LOCK    inner = self.inner.read()
      5. file_size = inner.desc.size
      6. GUARD   offset < file_size, else RETURN Ok(0)
      7. read_len = min(writer.avail(), file_size - offset)
      8. EFFECT  inner.page_cache.discard_range(offset..offset+read_len)
      9. EFFECT  inner.read_at(offset, writer)?   // direct block device reads
      10. UNLOCK inner
      11. EFFECT self.set_atime(now())
      12. RETURN Ok(read_len)

    LOCKS: READ(self.inner), then WRITE(self.inner) for atime

    CRASH_ANALYSIS:
      - Page cache discard (step 8) is in-memory only
      - Direct device reads are idempotent
      - Crash during read: no state change

    ROLLBACK: N/A (read-only data path)

    ENSURE:
      writer contains file data from device blocks [offset, offset+read_len)
      Page cache entries in [offset, offset+read_len) are invalidated
      S'.M.inode[self].desc.atime = now()
    ENSURE_ERR:
      EISDIR  if self is a directory
      EINVAL  if not block-aligned
      S' = S (no side effects on error)
}
