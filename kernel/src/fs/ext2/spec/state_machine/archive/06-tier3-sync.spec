// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Tier 3: sync_all, sync_data
//
// Sync operations flush in-memory state to disk. They are multi-step
// protocols that coordinate page cache writeback, inode metadata persist,
// and device cache flush.
//
// Reference: 00-state-model.spec for state notation.

/// =============================================================================
/// PROTOCOL: sync_all (fsync)
/// =============================================================================
/// Full fsync: data pages + inode metadata + device flush.
///
/// CODE: kernel/src/fs/ext2/inode.rs:1166-1184
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:191-193
/// LINUX_REF: fs/buffer.c:646 (generic_buffers_fsync)

PROTOCOL sync_all {
    TIER: 3
    SIGNATURE: fn sync_all(&self) -> Result<()>

    STEPS:
      1. EFFECT  fs = self.fs_arc()?

      // Step 1: Flush dirty data pages (upread)
      2. LOCK    upread = self.inner.upread()
      3. EFFECT  upread.sync_data()?
      // LINUX_REF: mm/filemap.c:777 (file_write_and_wait_range)

      // Step 2: Persist inode metadata (upgrade to write)
      4. LOCK    inner = upread.upgrade()
      5. PERSIST inner.persist_inode_and_sync(&fs)?
      // LINUX_REF: fs/buffer.c:619 (sync_inode_metadata)

      6. UNLOCK  inner

      // Step 3: Flush device write cache
      7. EFFECT  fs.block_device().sync()?
      // LINUX_REF: fs/buffer.c:654 (blkdev_issue_flush)

      8. RETURN  Ok(())

    LOCKS: UPREAD(self.inner) → WRITE(self.inner)

    CRASH_ANALYSIS:
      - Crash before step 3: data pages written back but metadata
        may not be on disk yet
      - Crash before step 7: metadata written to page cache but
        device cache not flushed — data may be in device write buffer
      - Crash after step 7: fully durable on disk

    ROLLBACK: None — sync operations are idempotent.

    ENSURE:
      S'.D.inode[self] = S'.M.inode[self]  (metadata synced)
      S'.D.data_blocks[self] = S'.M.page_cache[self]  (data synced)
      Device write cache flushed
      CONSISTENT(S') holds for this inode
    ENSURE_ERR:
      EIO if fs dropped or device sync fails
}

/// =============================================================================
/// PROTOCOL: sync_data (fdatasync)
/// =============================================================================
/// Data-only sync: data pages + conditional metadata + device flush.
///
/// CODE: kernel/src/fs/ext2/inode.rs:1214-1236
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:195-197
/// LINUX_REF: fs/buffer.c:609 (file_write_and_wait_range)

PROTOCOL sync_data {
    TIER: 3
    SIGNATURE: fn sync_data(&self) -> Result<()>

    STEPS:
      1. EFFECT  fs = self.fs_arc()?

      // Step 1: Flush dirty data pages + conditional metadata (write lock)
      2. LOCK    inner = self.inner.write()
      3. EFFECT  inner.sync_data()?
      // LINUX_REF: fs/buffer.c:609

      // Step 2: Persist metadata only if dirty (conservative I_DIRTY_DATASYNC)
      4. IF inner.desc.is_dirty():
           PERSIST inner.persist_inode_and_sync(&fs)?
      // LINUX_REF: fs/buffer.c:616-619

      5. UNLOCK  inner

      // Step 3: Flush device write cache
      6. EFFECT  fs.block_device().sync()?
      // LINUX_REF: fs/buffer.c:654

      7. RETURN  Ok(())

    LOCKS: WRITE(self.inner)

    CRASH_ANALYSIS:
      - Same as sync_all but metadata persist is conditional
      - If desc is clean, only data pages and device cache are flushed
      - Crash before device sync: data in device write buffer

    ROLLBACK: None — sync operations are idempotent.

    ENSURE:
      S'.D.data_blocks[self] = S'.M.page_cache[self]
      IF S.M.inode[self].desc.is_dirty:
        S'.D.inode[self] = S'.M.inode[self]
      Device write cache flushed
    ENSURE_ERR:
      EIO if fs dropped or device sync fails
}
