// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Tier 3: fallocate
//
// fallocate provides space pre-allocation and hole punching.
// Linux ext2 has no native fallocate; this is an Asterinas compatibility impl.
//
// Reference: 00-state-model.spec for state notation.

/// =============================================================================
/// PROTOCOL: fallocate
/// =============================================================================
/// Allocates or deallocates file space depending on mode.
///
/// CODE: kernel/src/fs/ext2/inode.rs:1131-1164
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:199-204
/// LINUX_REF: fs/ext2/file.c:313-328 (no native .fallocate)

PROTOCOL fallocate {
    TIER: 3
    SIGNATURE: fn fallocate(&self, mode: FallocMode, offset: usize, len: usize) -> Result<()>

    STEPS:
      MATCH mode:

      // --- PunchHoleKeepSize ---
      CASE PunchHoleKeepSize:
        1. LOCK   inner = self.inner.read()
        2. file_size = inner.desc.size
        3. GUARD  offset < file_size, else RETURN Ok(())
        4. end = min(file_size, offset + len)
        5. EFFECT inner.page_cache.fill_zeros(offset..end)?
        6. UNLOCK inner
        7. RETURN Ok(())

      // --- Allocate ---
      CASE Allocate:
        1. new_size = offset + len
        2. IF new_size > self.file_size():
             self.resize(new_size)?
             // Delegates to resize protocol (Tier 2)
        3. RETURN Ok(())

      // --- AllocateKeepSize ---
      CASE AllocateKeepSize:
        1. RETURN Ok(())  // no-op

      // --- Other modes ---
      DEFAULT:
        1. RETURN Err(EOPNOTSUPP)

    LOCKS:
      PunchHoleKeepSize: READ(self.inner)
      Allocate: delegates to resize() locks
      AllocateKeepSize: none

    CRASH_ANALYSIS:
      PunchHoleKeepSize:
        - fill_zeros writes to page cache only
        - Crash before sync: zeros lost, original data restored
      Allocate:
        - Delegates to resize; see resize crash analysis
      AllocateKeepSize:
        - No state change

    ROLLBACK:
      PunchHoleKeepSize: N/A (page cache zeros are idempotent)
      Allocate: delegates to resize rollback

    ENSURE:
      PunchHoleKeepSize:
        S'.M.page_cache[self][offset..end] = zeros
        S'.M.inode[self].desc.size = S.M.inode[self].desc.size  (unchanged)
      Allocate:
        S'.M.inode[self].desc.size ≥ offset + len
      AllocateKeepSize:
        S' = S
    ENSURE_ERR:
      EOPNOTSUPP for unsupported modes
}
