// SPDX-License-Identifier: MPL-2.0
//
// Protocol State Machine Verification — Tier 4: mknod
//
// mknod creates special file inodes (char device, block device, named pipe)
// by delegating to create() and optionally encoding a device ID.
//
// Reference: 00-state-model.spec for state notation.

/// =============================================================================
/// PROTOCOL: mknod
/// =============================================================================
/// Creates a special file inode and links it into this directory.
///
/// CODE: kernel/src/fs/ext2/impl_for_vfs/inode.rs:135-151
/// LINUX_REF: fs/ext2/namei.c:136-155 (ext2_mknod)

PROTOCOL mknod {
    TIER: 4
    SIGNATURE: fn mknod(&self, name: &str, mode: InodeMode, type_: MknodType) -> Result<Arc<dyn VfsInode>>

    REQUIRE:
      self.type_ = Dir
      name ∉ {"", ".", ".."}
      type_ ∈ {CharDevice, BlockDevice, NamedPipe}

    STEPS:
      // --- Map MknodType to InodeType + optional device_id ---
      1. MATCH type_:
           CharDevice(dev_id) → (InodeType::CharDevice, Some(dev_id))
           BlockDevice(dev_id) → (InodeType::BlockDevice, Some(dev_id))
           NamedPipe → (InodeType::NamedPipe, None)

      // --- Delegate to create (non-dir path) ---
      2. EFFECT new_inode = Inode::create(self, name, inode_type, mode.into())?
         // See create_non_dir protocol for full steps

      // --- Encode device ID if applicable ---
      3. IF device_id.is_some():
           EFFECT new_inode.set_device_id(device_id)?
           // Acquires WRITE(new_inode.inner)
           // Encodes dev_id in block_ptrs[0..2]
           // Updates ctime, persists
           // CODE: kernel/src/fs/ext2/inode.rs:121-135

      4. RETURN Ok(new_inode)

    LOCKS:
      Inherits from create_non_dir protocol
      + WRITE(new_inode.inner) for set_device_id

    CRASH_ANALYSIS:
      - Crash after create, before set_device_id:
        Inode exists in directory but has no device encoding.
        Special file inode with zero block_ptrs.
      - Crash after set_device_id: fully consistent.

    ROLLBACK:
      create failure: handled by create_non_dir rollback.
      set_device_id failure: inode exists but device ID not set.
      No explicit rollback of create on set_device_id failure.

    ENSURE:
      ∃ new inode:
        S'.M.inode[new].type_ = inode_type
        IF device_id: S'.M.inode[new].block_ptrs encodes dev_id
      S'.M.dir_entries[self] contains (name → new.ino)
    ENSURE_ERR:
      ENOTDIR if self not directory
      EINVAL  if name invalid
      ENOSPC  if no free inodes
}
