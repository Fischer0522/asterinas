[PROMPT]
Provide `kernel/src/fs/ext2/fs.rs`. Output Rust code only. No unsafe. No panic/assert/unimplemented.
Use Rust OOP style: all functions must be methods in `impl Ext2`.

[RELY]
use super::prelude::*;
use super::block_group::BlockGroup;
use super::inode::Inode;
use super::super_block::SuperBlock;
use super::utils::Dirty;
use crate::fs::utils::FsEventSubscriberStats;

/// The root inode number (Linux EXT2_ROOT_INO).
pub const ROOT_INO: u32 = 2;

/// The Ext2 filesystem (core state holder).
#[derive(Debug)]
pub struct Ext2 {
    /// Backing block device.
    block_device: Arc<dyn BlockDevice>,
    /// Superblock with dirty tracking.
    super_block: RwMutex<Dirty<SuperBlock>>,
    /// Block group descriptors and caches.
    block_groups: Vec<BlockGroup>,
    /// Inodes per group.
    inodes_per_group: u32,
    /// Blocks per group.
    blocks_per_group: u32,
    /// Inode size in bytes.
    inode_size: usize,
    /// Block size in bytes.
    block_size: usize,
    /// Group descriptor table segment.
    group_descriptors_segment: USegment,
    /// FS event stats for VFS.
    fs_event_subscriber_stats: FsEventSubscriberStats,
    /// Weak self reference for inode back-pointers.
    self_ref: Weak<Ext2>,
}

[GUARANTEE]
impl Ext2 {
    /// Opens and loads an Ext2 filesystem from a block device (skeleton only).
    pub fn open(block_device: Arc<dyn BlockDevice>) -> Result<Arc<Self>>;

    /// Returns the block device.
    pub fn block_device(&self) -> &dyn BlockDevice;

    /// Returns the block size in bytes.
    pub fn block_size(&self) -> usize;

    /// Returns the inode size in bytes.
    pub fn inode_size(&self) -> usize;

    /// Returns the number of inodes per group.
    pub fn inodes_per_group(&self) -> u32;

    /// Returns the number of blocks per group.
    pub fn blocks_per_group(&self) -> u32;

    /// Returns a read guard of the superblock.
    pub fn super_block(&self) -> RwMutexReadGuard<'_, Dirty<SuperBlock>>;

    /// Returns the fs event subscriber stats.
    pub fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats;

    /// Returns the root inode.
    pub fn root_inode(&self) -> Result<Arc<Inode>>;
}

[SPECIFICATION]
Pre (open):
- `block_device` is a valid block device handle.

Post (open):
- In this skeleton phase, returns `Err(Errno::ENOSYS)`.
- Must not perform any I/O and must not mutate global state.

Pre (block_device, block_size, inode_size, inodes_per_group, blocks_per_group, super_block,
fs_event_subscriber_stats):
- `self` is a valid Ext2 instance created by future `open` (or test-only constructor).

Post (block_device):
- Returns the exact `BlockDevice` reference stored in `self`.

Post (block_size, inode_size, inodes_per_group, blocks_per_group):
- Returns the corresponding field value without modification.

Post (super_block):
- Returns a read guard to the superblock without modifying it.

Post (fs_event_subscriber_stats):
- Returns a reference to `self.fs_event_subscriber_stats`.

Pre (root_inode):
- `self` is a valid Ext2 instance.

Post (root_inode):
- In this skeleton phase, returns `Err(Errno::ENOSYS)`.
- Must not perform any I/O.

Invariant (Ext2):
- If initialized by future phases, `block_size` equals the filesystem block size
  and `inode_size` equals the on-disk inode size.
- `inodes_per_group > 0` and `blocks_per_group > 0` once initialized.
