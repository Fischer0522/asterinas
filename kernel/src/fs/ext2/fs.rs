// SPDX-License-Identifier: MPL-2.0

use super::block_group::{BlockGroup, RawGroupDesc};
use super::inode::{Inode, InodeDesc, RawInode};
use super::prelude::*;
use super::super_block::SuperBlock;
use super::utils::Dirty;
use crate::fs::utils::FsEventSubscriberStats;
use core::mem::size_of;

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

    /// Reads an inode and constructs its in-memory representation.
    /// TODO: Add inode caching.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1387 (ext2_iget)
    pub(super) fn read_inode(&self, ino: u32) -> Result<Arc<Inode>> {
        let desc = self.read_inode_desc(ino)?;
        Inode::from_desc(ino, desc, self.self_ref.clone())
    }

    /// Returns the inode table block ID for the given group.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1314 (ext2_get_inode)
    pub(super) fn inode_table_block(&self, group_idx: usize, table_block_index: u32) -> Result<Bid> {
        let group = self
            .block_groups
            .get(group_idx)
            .ok_or_else(|| Error::new(Errno::EIO))?;
        Ok(group.inode_table_bid() + table_block_index as u64)
    }

    /// Reads an inode descriptor from disk.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1314 (ext2_get_inode)
    pub(super) fn read_inode_desc(&self, ino: u32) -> Result<InodeDesc> {
        let sb = self.super_block.read();

        if (ino != ROOT_INO && ino < sb.first_ino()) || ino > sb.total_inodes() {
            return_errno!(Errno::EINVAL);
        }

        let inodes_per_group = sb.inodes_per_group();
        let group_idx = (ino - 1) / inodes_per_group;
        let index_in_group = (ino - 1) % inodes_per_group;

        let inode_size = sb.inode_size();
        let block_size = sb.block_size();
        let offset_bytes = (index_in_group as usize).saturating_mul(inode_size);
        let block_index = offset_bytes / block_size;
        let offset_in_block = offset_bytes % block_size;

        let block_bid = self.inode_table_block(group_idx as usize, block_index as u32)?;
        let mut buf = vec![0u8; BLOCK_SIZE];
        if self
            .block_device
            .read_bytes(block_bid.to_offset(), &mut buf)
            .is_err()
        {
            return_errno!(Errno::EIO);
        }

        if offset_in_block + size_of::<RawInode>() > BLOCK_SIZE {
            return_errno!(Errno::EIO);
        }

        let mut reader = VmReader::from(buf.as_slice());
        let raw = reader
            .skip(offset_in_block)
            .read_val::<RawInode>()
            .map_err(|_| Error::new(Errno::EIO))?;
        Ok(InodeDesc { raw })
    }

    /// Loads the group descriptor table into a segment.
    ///
    /// Linux: /root/linux/fs/ext2/super.c:695 (ext2_check_descriptors)
    pub(super) fn load_group_desc_table(&self, sb: &SuperBlock) -> Result<USegment> {
        let groups_count = sb.block_groups_count() as usize;
        let desc_bytes = groups_count * size_of::<RawGroupDesc>();
        let npages = desc_bytes.div_ceil(BLOCK_SIZE);

        let segment = FrameAllocOptions::new().zeroed(false).alloc_segment(npages)?;
        let bio_segment = BioSegment::new_from_segment(segment.clone().into(), BioDirection::FromDevice);
        match self.block_device.read_blocks(sb.group_descriptors_bid(0), bio_segment)? {
            BioStatus::Complete => {}
            err_status => {
                return Err(Error::from(err_status));
            }
        }
        let segment: USegment = segment.into();
        self.check_group_desc_table(sb, &segment)?;
        Ok(segment)
    }

    /// Validates the group descriptor table.
    ///
    /// Linux: /root/linux/fs/ext2/super.c:695 (ext2_check_descriptors)
    pub(super) fn check_group_desc_table(&self, sb: &SuperBlock, group_descs: &USegment) -> Result<()> {
        let groups_count = sb.block_groups_count() as usize;
        let itb_per_group = sb.itb_per_group();

        for group_idx in 0..groups_count {
            let offset = group_idx * size_of::<RawGroupDesc>();
            let desc = group_descs.read_val::<RawGroupDesc>(offset)?;

            let first_block = sb.group_first_block_no(group_idx);
            let last_block = sb.group_last_block_no(group_idx);

            let block_bitmap = desc.block_bitmap;
            let inode_bitmap = desc.inode_bitmap;
            let inode_table = desc.inode_table;

            if block_bitmap < first_block || block_bitmap > last_block {
                return_errno!(Errno::EINVAL);
            }
            if inode_bitmap < first_block || inode_bitmap > last_block {
                return_errno!(Errno::EINVAL);
            }
            let table_last = inode_table.saturating_add(itb_per_group.saturating_sub(1));
            if inode_table < first_block || table_last > last_block {
                return_errno!(Errno::EINVAL);
            }
        }
        Ok(())
    }

    pub(super) fn load_block_groups(
        &self,
        sb: &SuperBlock,
        group_descs: &USegment,
    ) -> Result<Vec<BlockGroup>> {
        let groups_count = sb.block_groups_count() as usize;
        let mut groups = Vec::with_capacity(groups_count);
        for idx in 0..groups_count {
            let group = BlockGroup::load(group_descs, idx)?;
            groups.push(group);
        }
        Ok(groups)
    }
}
