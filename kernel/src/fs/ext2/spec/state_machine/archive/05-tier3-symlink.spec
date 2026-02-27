// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Tier 3: Symlink Operations
//
// read_link and write_link handle two storage formats:
//   - Fast symlink: target stored inline in block_ptrs[] (≤60 bytes)
//   - Slow symlink: target stored in page cache / data blocks
//
// Reference: 00-state-model.spec for state notation.

/// =============================================================================
/// PROTOCOL: read_link
/// =============================================================================
/// Reads the symlink target string.
///
/// CODE: kernel/src/fs/ext2/inode.rs:424-462
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:183-185
/// LINUX_REF: fs/ext2/inode.c:1483-1487 (fast), fs/namei.c:6227 (slow)

PROTOCOL read_link {
    TIER: 3
    SIGNATURE: fn read_link(&self) -> Result<SymbolicLink>

    REQUIRE:
      self.type_ = SymLink

    STEPS:
      1. GUARD   self.type_ == SymLink, else RETURN Err(EINVAL)
      2. EFFECT  fs = self.fs_arc()?
      3. LOCK    inner = self.inner.read()
      4. link_size = inner.desc.size

      // --- Fast symlink path ---
      IF inner.desc.is_fast_symlink(block_size):
        5a. read_len = min(link_size, MAX_FAST_SYMLINK_LEN - 1)
        6a. Copy block_ptrs[] bytes → raw_bytes[..read_len]
        7a. target = String::from_utf8(raw_bytes)?
        8a. UNLOCK inner
        9a. RETURN Ok(SymbolicLink::Plain(target))

      // --- Slow symlink path ---
      5b. Allocate target buffer of link_size bytes
      6b. inner.page_cache.pages().read_bytes(0, &mut target)?
      7b. target = String::from_utf8(target)?
      8b. UNLOCK inner
      9b. RETURN Ok(SymbolicLink::Plain(target))

    LOCKS: READ(self.inner)
    CRASH: N/A (pure read)
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN contains the symlink target string
    ENSURE_ERR:
      EINVAL if not a symlink
      EIO    if fs dropped or UTF-8 decode fails
}

/// =============================================================================
/// PROTOCOL: write_link
/// =============================================================================
/// Writes the symlink target string. Two-phase for slow symlinks.
///
/// CODE: kernel/src/fs/ext2/inode.rs:468-565
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:187-189
/// LINUX_REF: fs/ext2/namei.c:165-191 (ext2_symlink)

PROTOCOL write_link {
    TIER: 3
    SIGNATURE: fn write_link(&self, target: &str) -> Result<()>

    REQUIRE:
      self.type_ = SymLink
      target.len() + 1 ≤ block_size

    STEPS:
      // --- Validation ---
      1. GUARD   self.type_ == SymLink, else RETURN Err(EINVAL)
      2. EFFECT  fs = self.fs_arc()?
      3. with_nul = target.len() + 1
      4. GUARD   with_nul ≤ block_size, else RETURN Err(ENAMETOOLONG)

      // --- Fast symlink path (with_nul ≤ 60) ---
      IF with_nul ≤ MAX_FAST_SYMLINK_LEN:
        5a. LOCK    inner = self.inner.write()
        6a. MUTATE  Copy target bytes → block_ptrs[] (LE encoding)
        7a. MUTATE  inner.desc.size = target.len()
        8a. MUTATE  inner.desc.blocks = 0
        9a. PERSIST inner.persist_inode_and_sync(&fs)?
        10a. UNLOCK inner
        11a. RETURN Ok(())

      // --- Slow symlink path (two-phase) ---
      // Phase 1: Allocation (write lock)
      5b. LOCK    inner = self.inner.write()
      6b. old_size = inner.desc.size
      7b. FOR iblock in 0..target.len().div_ceil(block_size):
            inner.get_or_alloc_block(iblock, true)?
      8b. inner.page_cache.resize(align_up(target.len(), block_size))?
      9b. inner.desc.size = target.len()
      10b. UNLOCK inner
      // On error: write_failed_cleanup(old_size, target.len(), block_size)

      // Phase 2: Data write (upread)
      11b. LOCK   upread = self.inner.upread()
      12b. EFFECT upread.page_cache.pages().write_bytes(0, target.as_bytes())?
      // On error: upgrade → write_failed_cleanup

      // Phase 3: Persist (upgrade)
      13b. LOCK    inner = upread.upgrade()
      14b. PERSIST inner.persist_inode_and_sync(&fs)?
      15b. UNLOCK  inner
      16b. RETURN  Ok(())

    LOCKS:
      Fast: WRITE(self.inner)
      Slow: WRITE(self.inner) → UPREAD(self.inner) → WRITE(self.inner)

    CRASH_ANALYSIS:
      Fast path:
        - Crash before persist: block_ptrs reverts on reboot
        - Crash after persist: committed
      Slow path:
        - Crash after phase 1: blocks allocated, size set, data unwritten
        - Crash during phase 2: partial data in page cache
        - Crash after phase 3: committed (data in page cache, not on disk)

    ROLLBACK:
      Slow path uses write_failed_cleanup on phase 1/2 failure:
        Discard page cache range, truncate blocks, restore old_size.
      CODE: kernel/src/fs/ext2/inode.rs:782-813

    ENSURE:
      S'.M.inode[self].desc.size = target.len()
      Fast: S'.M.inode[self].desc.block_ptrs contains target bytes
      Slow: S'.M.page_cache[self] contains target bytes at offset 0
    ENSURE_ERR:
      EINVAL        if not a symlink
      ENAMETOOLONG  if target+nul > block_size
      S'.M observable state = S.M observable state (via rollback)
}
