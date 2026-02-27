// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Tier 4: FileSystem trait
//
// The FileSystem trait methods operate at the filesystem level,
// not on individual inodes. They coordinate global state.
//
// Reference: 00-state-model.spec for state notation.

/// =============================================================================
/// PROTOCOL: FileSystem::name()
/// =============================================================================
/// Returns the filesystem type name.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:12-15
/// LINUX_REF: fs/ext2/super.c:1698 (ext2_fs_type)

PROTOCOL fs_name {
    TIER: 1
    SIGNATURE: fn name(&self) -> &'static str

    STEPS:
      1. RETURN "ext2"

    LOCKS: none
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN = "ext2"
}

/// =============================================================================
/// PROTOCOL: FileSystem::sync()
/// =============================================================================
/// Full filesystem sync: all inodes + metadata + device flush.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:17-23
/// LINUX_REF: fs/ext2/super.c:1308 (ext2_sync_fs)

PROTOCOL fs_sync {
    TIER: 3
    SIGNATURE: fn sync(&self) -> Result<()>

    STEPS:
      // Step 1: Sync all cached inodes
      1. EFFECT self.sync_all_inodes()?
         // Iterates all block group inode caches
         // For each cached inode: calls inode.sync_all()

      // Step 2: Sync filesystem metadata
      2. EFFECT self.sync_metadata()?
         // Recomputes free counts from group descriptors
         // Writes group descriptor table to device
         // Writes superblock (primary + backups) to device
         // CODE: kernel/src/fs/ext2/fs.rs:709-799

      // Step 3: Flush device write cache
      3. EFFECT self.block_device().sync()?

      4. RETURN Ok(())

    LOCKS:
      Per-inode locks during sync_all_inodes
      SB_WRITE during sync_metadata
      Per-group locks during sync_metadata

    CRASH_ANALYSIS:
      - Crash during step 1: some inodes synced, others not
      - Crash during step 2: metadata partially written
      - Crash after step 3: fully durable

    ROLLBACK: None — sync is idempotent.

    ENSURE:
      CONSISTENT(S') for all inodes and metadata
      S'.D.sb.free_blocks = Σ_g S'.D.group[g].free_blocks
      S'.D.sb.free_inodes = Σ_g S'.D.group[g].free_inodes
    ENSURE_ERR:
      EIO if device sync fails
}

/// =============================================================================
/// PROTOCOL: FileSystem::root_inode()
/// =============================================================================
/// Returns the cached root inode (ino=2).
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:25-28
/// LINUX_REF: fs/ext2/super.c:877 (ext2_fill_super root inode setup)

PROTOCOL fs_root_inode {
    TIER: 1
    SIGNATURE: fn root_inode(&self) -> Arc<dyn VfsInode>

    STEPS:
      1. RETURN self.root_inode.clone()

    LOCKS: none (root_inode is immutable after mount)
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN.ino = 2 (ROOT_INO)
}

/// =============================================================================
/// PROTOCOL: FileSystem::sb()
/// =============================================================================
/// Returns a snapshot of the superblock as a VFS SuperBlock struct.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:30-48
/// LINUX_REF: fs/ext2/super.c:1446 (ext2_statfs)

PROTOCOL fs_sb {
    TIER: 1
    SIGNATURE: fn sb(&self) -> SuperBlock

    STEPS:
      1. LOCK   sb_guard = self.super_block.read()
      2. Construct SuperBlock from ext2 superblock fields:
           magic   = MAGIC_NUM
           bsize   = sb.block_size()
           blocks  = sb.total_blocks()
           bfree   = sb.free_blocks_count()
           bavail  = free_blocks - reserved_blocks (saturating)
           files   = sb.total_inodes()
           ffree   = sb.free_inodes_count()
           namelen = NAME_MAX
           frsize  = sb.fragment_size()
      3. UNLOCK sb_guard
      4. RETURN superblock

    LOCKS: SB_READ
    CRASH: N/A (pure read)
    ROLLBACK: N/A

    ENSURE:
      S' = S
      RETURN reflects current in-memory superblock state
}

/// =============================================================================
/// PROTOCOL: FileSystem::fs_event_subscriber_stats()
/// =============================================================================
/// Returns the filesystem event subscriber statistics.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/fs.rs:50-53

PROTOCOL fs_event_subscriber_stats {
    TIER: 1
    SIGNATURE: fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats

    STEPS:
      1. RETURN &self.fs_event_subscriber_stats

    LOCKS: none (field is immutable after mount)
    CRASH: N/A
    ROLLBACK: N/A

    ENSURE:
      S' = S
}
