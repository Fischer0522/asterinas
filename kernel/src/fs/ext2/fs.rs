// SPDX-License-Identifier: MPL-2.0

use super::block_group::BlockGroup;
use super::inode::Inode;
use super::prelude::*;
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

impl Ext2 {
    /// Opens and loads an Ext2 filesystem from a block device (skeleton only).
    pub fn open(_block_device: Arc<dyn BlockDevice>) -> Result<Arc<Self>> {
        return_errno!(Errno::ENOSYS);
    }

    /// Returns the block device.
    pub fn block_device(&self) -> &dyn BlockDevice {
        self.block_device.as_ref()
    }

    /// Returns the block size in bytes.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Returns the inode size in bytes.
    pub fn inode_size(&self) -> usize {
        self.inode_size
    }

    /// Returns the number of inodes per group.
    pub fn inodes_per_group(&self) -> u32 {
        self.inodes_per_group
    }

    /// Returns the number of blocks per group.
    pub fn blocks_per_group(&self) -> u32 {
        self.blocks_per_group
    }

    /// Returns a read guard of the superblock.
    pub fn super_block(&self) -> RwMutexReadGuard<'_, Dirty<SuperBlock>> {
        self.super_block.read()
    }

    /// Returns the fs event subscriber stats.
    pub fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        &self.fs_event_subscriber_stats
    }

    /// Returns the root inode.
    pub fn root_inode(&self) -> Result<Arc<Inode>> {
        return_errno!(Errno::ENOSYS);
    }
}
