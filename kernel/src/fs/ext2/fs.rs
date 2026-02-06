// SPDX-License-Identifier: MPL-2.0

use super::block_group::{BlockGroup, RawGroupDesc};
use super::inode::{Inode, InodeDesc, RawInode};
use super::prelude::*;
use super::super_block::{RawSuperBlock, SuperBlock, SUPER_BLOCK_OFFSET};
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
        todo!()
        // Inode::from_desc(ino, desc, self.self_ref.clone())
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
        InodeDesc::try_from(&raw)
    }

    /// Writes an inode descriptor to disk.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1314 (ext2_get_inode)
    pub(super) fn write_inode_desc(&self, ino: u32, raw: &RawInode) -> Result<()> {
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

        // TODO: remove this read.
        let block_bid = self.inode_table_block(group_idx as usize, block_index as u32)?;
        let mut buf = vec![0u8; BLOCK_SIZE];
        if self
            .block_device
            .read_bytes(block_bid.to_offset(), &mut buf)
            .is_err()
        {
            return_errno!(Errno::EIO);
        }

        let inode_len = size_of::<RawInode>();
        if offset_in_block + inode_len > BLOCK_SIZE {
            return_errno!(Errno::EIO);
        }

        buf[offset_in_block..offset_in_block + inode_len].copy_from_slice(raw.as_bytes());

        if self
            .block_device
            .write_bytes(block_bid.to_offset(), &buf)
            .is_err()
        {
            return_errno!(Errno::EIO);
        }

        Ok(())
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

    /// Allocates up to `count` contiguous blocks.
    pub(super) fn alloc_blocks(&self, count: u32) -> Result<Range<u32>> {
        if count == 0 {
            return_errno!(Errno::EINVAL);
        }

        let (
            first_data_block,
            blocks_per_group,
            total_blocks,
            groups_count,
            sb_free_blocks,
            itb_per_group,
        ) = {
            let guard = self.super_block.read();
            (
                guard.first_data_block(),
                guard.blocks_per_group(),
                guard.total_blocks(),
                guard.block_groups_count() as usize,
                guard.free_blocks_count(),
                guard.itb_per_group(),
            )
        };
        if groups_count == 0 || self.block_groups.len() < groups_count {
            return_errno!(Errno::EIO);
        }
        if sb_free_blocks == 0 {
            return_errno!(Errno::ENOSPC);
        }

        let mut saw_corruption = false;
        for group_idx in 0..groups_count {
            let group = self
                .block_groups
                .get(group_idx)
                .ok_or_else(|| Error::new(Errno::EIO))?;
            if group.free_blocks_count() == 0 {
                continue;
            }

            let (range, corrupt) = self.try_alloc_in_group(
                first_data_block,
                blocks_per_group,
                total_blocks,
                groups_count,
                itb_per_group,
                sb_free_blocks,
                group,
                group_idx,
                count,
            )?;
            if corrupt {
                saw_corruption = true;
            }
            if let Some(range) = range {
                return Ok(range);
            }
        }

        if saw_corruption {
            return_errno!(Errno::EIO);
        }
        return_errno!(Errno::ENOSPC);
    }

    /// Frees a range of blocks starting at `start`.
    pub(super) fn free_blocks(&self, start: u32, count: u32) -> Result<()> {
        if count == 0 {
            return Ok(());
        }

        let (first_data_block, blocks_per_group, total_blocks, groups_count, itb_per_group, block_size) = {
            let guard = self.super_block.read();
            (
                guard.first_data_block(),
                guard.blocks_per_group(),
                guard.total_blocks(),
                guard.block_groups_count() as usize,
                guard.itb_per_group(),
                guard.block_size(),
            )
        };
        if !Self::data_block_valid(first_data_block, total_blocks, block_size, start, count) {
            return_errno!(Errno::EIO);
        }

        let mut current = start;
        let mut remaining = count;

        while remaining > 0 {
            let group_idx = ((current - first_data_block) / blocks_per_group) as usize;
            let group = self
                .block_groups
                .get(group_idx)
                .ok_or_else(|| Error::new(Errno::EIO))?;

            let group_first = Self::group_first_block_no(first_data_block, blocks_per_group, group_idx);
            let group_last = Self::group_last_block_no(
                first_data_block,
                blocks_per_group,
                total_blocks,
                groups_count,
                group_idx,
            );
            if group_last < group_first {
                return_errno!(Errno::EIO);
            }
            let group_size = group_last - group_first + 1;
            if group_size as usize > BLOCK_SIZE * 8 {
                return_errno!(Errno::EIO);
            }
            let bit = current.saturating_sub(group_first);
            if bit >= group_size {
                return_errno!(Errno::EIO);
            }
            let group_count = remaining.min(group_size.saturating_sub(bit));

            let mut bitmap = {
                let sb_guard = self.super_block.read();
                group.load_block_bitmap(self, &sb_guard)?
            };

            if self.range_overlaps_system_zone(itb_per_group, group, current, group_count) {
                return_errno!(Errno::EIO);
            }

            let mut freed = 0u32;
            let range_start = bit as u16;
            let range_end = (bit + group_count) as u16;

            for idx in range_start..range_end {
                if !bitmap.is_allocated(idx) {
                    warn!(
                        "ext2_free_blocks: bit already cleared for block {}",
                        current.saturating_add((idx - range_start) as u32)
                    );
                }
            }
            bitmap.free_consecutive(range_start..range_end);
            freed = group_count;

            if self
                .block_device
                .write_bytes(group.block_bitmap_bid().to_offset(), bitmap.as_bytes())
                .is_err()
            {
                return_errno!(Errno::EIO);
            }

            if freed > 0 {
                group.inc_free_blocks(freed as u16);
                let mut sb_write = self.super_block.write();
                sb_write.inc_free_blocks(freed);
            }

            current = current.saturating_add(group_count);
            remaining = remaining.saturating_sub(group_count);
        }

        Ok(())
    }

    /// Allocates a new inode number.
    pub(super) fn alloc_inode(&self, parent_ino: u32, inode_type: InodeType) -> Result<u32> {
        let (groups_count, inodes_per_group, total_inodes, first_ino, free_inodes) = {
            let sb_guard = self.super_block.read();
            (
                sb_guard.block_groups_count() as usize,
                sb_guard.inodes_per_group(),
                sb_guard.total_inodes(),
                sb_guard.first_ino(),
                sb_guard.free_inodes_count(),
            )
        };
        if groups_count == 0 || self.block_groups.len() < groups_count {
            return_errno!(Errno::EIO);
        }
        if parent_ino < ROOT_INO || parent_ino > total_inodes {
            return_errno!(Errno::EIO);
        }
        if free_inodes == 0 {
            return_errno!(Errno::ENOSPC);
        }

        let parent_group = ((parent_ino - 1) / inodes_per_group) as usize;
        for offset in 0..groups_count {
            let group_idx = (parent_group + offset) % groups_count;
            let group = self
                .block_groups
                .get(group_idx)
                .ok_or_else(|| Error::new(Errno::EIO))?;
            if group.free_inodes_count() == 0 {
                continue;
            }

            let mut bitmap = {
                let sb_guard = self.super_block.read();
                group.load_inode_bitmap(self, &sb_guard)?
            };
            let Some(inode_idx) = bitmap.alloc() else {
                continue;
            };

            let ino = (group_idx as u32)
                .saturating_mul(inodes_per_group)
                .saturating_add(inode_idx as u32)
                .saturating_add(1);
            if ino < first_ino || ino > total_inodes {
                return_errno!(Errno::EIO);
            }

            if self
                .block_device
                .write_bytes(group.inode_bitmap_bid().to_offset(), bitmap.as_bytes())
                .is_err()
            {
                return_errno!(Errno::EIO);
            }

            group.dec_free_inodes(1);
            if inode_type.is_directory() {
                group.inc_used_dirs();
            }
            let mut sb_write = self.super_block.write();
            sb_write.dec_free_inodes();

            return Ok(ino);
        }

        return_errno!(Errno::ENOSPC);
    }

    /// Frees an inode by number.
    pub(super) fn free_inode(&self, ino: u32) -> Result<()> {
        let (inodes_per_group, total_inodes, first_ino, groups_count) = {
            let sb_guard = self.super_block.read();
            (
                sb_guard.inodes_per_group(),
                sb_guard.total_inodes(),
                sb_guard.first_ino(),
                sb_guard.block_groups_count() as usize,
            )
        };
        if ino < first_ino || ino > total_inodes {
            return_errno!(Errno::EIO);
        }
        if groups_count == 0 || self.block_groups.len() < groups_count {
            return_errno!(Errno::EIO);
        }

        let desc = self.read_inode_desc(ino)?;
        let inode_type = desc.type_();
        let is_dir = inode_type.is_directory();

        let group_idx = ((ino - 1) / inodes_per_group) as usize;
        let bit = ((ino - 1) % inodes_per_group) as u16;
        let group = self
            .block_groups
            .get(group_idx)
            .ok_or_else(|| Error::new(Errno::EIO))?;

        let mut bitmap = {
            let sb_guard = self.super_block.read();
            group.load_inode_bitmap(self, &sb_guard)?
        };

        let mut freed = false;
        if !bitmap.is_allocated(bit) {
            freed = true;   
            warn!("ext2_free_inode: inode {} already freed", ino);
        }
        if !freed {
            bitmap.free(bit);
        }

        if self
            .block_device
            .write_bytes(group.inode_bitmap_bid().to_offset(), bitmap.as_bytes())
            .is_err()
        {
            return_errno!(Errno::EIO);
        }

        if !freed { 
            group.inc_free_inodes(1);
            if is_dir {
                group.dec_used_dirs();
            }
            let mut sb_write = self.super_block.write();
            sb_write.inc_free_inodes();
        }

        Ok(())
    }

    /// Writes back superblock and group descriptor table if dirty.
    pub fn sync_metadata(&self) -> Result<()> {
        let sb_dirty = self.super_block.read().is_dirty();
        let mut any_group_dirty = false;
        for group in &self.block_groups {
            if group.is_desc_dirty() {
                any_group_dirty = true;
                break;
            }
        }

        if !sb_dirty && !any_group_dirty {
            return Ok(());
        }

        let groups_count = {
            let sb_guard = self.super_block.read();
            sb_guard.block_groups_count() as usize
        };
        if groups_count == 0 || self.block_groups.len() < groups_count {
            return_errno!(Errno::EIO);
        }

        for group in &self.block_groups {
            group.sync_metadata(&self.group_descriptors_segment)?;
        }

        let desc_bytes = groups_count * size_of::<RawGroupDesc>();
        let mut desc_buf = vec![0u8; desc_bytes];
        if self
            .group_descriptors_segment
            .read_bytes(0, &mut desc_buf)
            .is_err()
        {
            return_errno!(Errno::EIO);
        }

        let mut sb_guard = self.super_block.write();
        let wtime = crate::time::SystemTime::now()
            .duration_since(&crate::time::SystemTime::UNIX_EPOCH)
            .map(UnixTime::from)
            .map_err(|_| Error::new(Errno::EIO))?;
        sb_guard.set_wtime(wtime);
        if self
            .block_device
            .write_bytes(sb_guard.group_descriptors_bid(0).to_offset(), &desc_buf)
            .is_err()
        {
            return_errno!(Errno::EIO);
        }

        let mut raw_sb = RawSuperBlock::from(&**sb_guard);
        if self
            .block_device
            .write_bytes(SUPER_BLOCK_OFFSET, raw_sb.as_bytes())
            .is_err()
        {
            return_errno!(Errno::EIO);
        }

        for idx in 1..groups_count {
            if !sb_guard.is_backup_group(idx) {
                continue;
            }
            raw_sb.block_group_idx = idx as u16;
            if self
                .block_device
                .write_bytes(sb_guard.bid(idx).to_offset(), raw_sb.as_bytes())
                .is_err()
            {
                return_errno!(Errno::EIO);
            }
            if self
                .block_device
                .write_bytes(sb_guard.group_descriptors_bid(idx).to_offset(), &desc_buf)
                .is_err()
            {
                return_errno!(Errno::EIO);
            }
        }

        sb_guard.clear_dirty();
        Ok(())
    }

    // TODO: Move this method into BlockGroup?
    fn try_alloc_in_group(
        &self,
        first_data_block: u32,
        blocks_per_group: u32,
        total_blocks: u32,
        groups_count: usize,
        itb_per_group: u32,
        sb_free_blocks: u32,
        group: &BlockGroup,
        group_idx: usize,
        count: u32,
    ) -> Result<(Option<Range<u32>>, bool)> {
        let group_first = Self::group_first_block_no(first_data_block, blocks_per_group, group_idx);
        let group_last = Self::group_last_block_no(
            first_data_block,
            blocks_per_group,
            total_blocks,
            groups_count,
            group_idx,
        );
        if group_last < group_first {
            return_errno!(Errno::EIO);
        }
        let group_size = group_last - group_first + 1;
        if group_size as usize > BLOCK_SIZE * 8 {
            return_errno!(Errno::EIO);
        }

        let mut saw_corruption = false;
        let mut bitmap = {
            let sb_guard = self.super_block.read();
            group.load_block_bitmap(self, &sb_guard)?
        };
        if group.free_blocks_count() > 0 {
            if let Some(range) = bitmap.alloc_consecutive(1) {
                bitmap.free_consecutive(range);
            } else {
                saw_corruption = true;
            }
        }

        let mut rejected = Vec::new();
        let mut req = count.min(group_size) as u16;
        while req > 0 {
            let Some(range) = bitmap.alloc_consecutive(req) else {
                req /= 2;
                continue;
            };
            let alloc_len = range.len() as u32;
            let run_start = range.start as u32;
            let ret_block = group_first.saturating_add(run_start);

            if self.range_overlaps_system_zone(itb_per_group, group, ret_block, alloc_len) {
                saw_corruption = true;
                rejected.push(range);
                continue;
            }
            if group.free_blocks_count() < alloc_len as u16 || sb_free_blocks < alloc_len {
                saw_corruption = true;
                rejected.push(range);
                continue;
            }

            for rejected_range in rejected.drain(..) {
                bitmap.free_consecutive(rejected_range);
            }

            if self
                .block_device
                .write_bytes(group.block_bitmap_bid().to_offset(), bitmap.as_bytes())
                .is_err()
            {
                return_errno!(Errno::EIO);
            }

            group.dec_free_blocks(alloc_len as u16);
            let mut sb_write = self.super_block.write();
            sb_write.dec_free_blocks(alloc_len);

            let range = ret_block..ret_block.saturating_add(alloc_len);
            return Ok((Some(range), saw_corruption));
        }

        Ok((None, saw_corruption))
    }

    fn range_overlaps_system_zone(
        &self,
        itb_per_group: u32,
        group: &BlockGroup,
        start: u32,
        count: u32,
    ) -> bool {
        let Some(end) = start.checked_add(count.saturating_sub(1)) else {
            return true;
        };
        let block_bitmap = group.block_bitmap_bid().to_raw() as u32;
        let inode_bitmap = group.inode_bitmap_bid().to_raw() as u32;
        let inode_table = group.inode_table_bid().to_raw() as u32;

        if Self::ranges_overlap(start, end, block_bitmap, 1) {
            return true;
        }
        if Self::ranges_overlap(start, end, inode_bitmap, 1) {
            return true;
        }
        if Self::ranges_overlap(start, end, inode_table, itb_per_group) {
            return true;
        }
        false
    }

    fn group_first_block_no(first_data_block: u32, blocks_per_group: u32, group_idx: usize) -> u32 {
        (group_idx as u32)
            .saturating_mul(blocks_per_group)
            .saturating_add(first_data_block)
    }

    fn group_last_block_no(
        first_data_block: u32,
        blocks_per_group: u32,
        total_blocks: u32,
        groups_count: usize,
        group_idx: usize,
    ) -> u32 {
        if group_idx as u32 == (groups_count as u32).saturating_sub(1) {
            total_blocks.saturating_sub(1)
        } else {
            Self::group_first_block_no(first_data_block, blocks_per_group, group_idx)
                .saturating_add(blocks_per_group)
                .saturating_sub(1)
        }
    }

    fn data_block_valid(
        first_data_block: u32,
        total_blocks: u32,
        block_size: usize,
        start_blk: u32,
        count: u32,
    ) -> bool {
        if count == 0 {
            return false;
        }

        let Some(end_blk) = start_blk.checked_add(count.saturating_sub(1)) else {
            return false;
        };

        if start_blk <= first_data_block || end_blk < start_blk || end_blk >= total_blocks {
            return false;
        }

        let sb_block = if block_size == SUPER_BLOCK_OFFSET { 1u32 } else { 0u32 };
        if start_blk <= sb_block && end_blk >= sb_block {
            return false;
        }

        true
    }

    fn ranges_overlap(start: u32, end: u32, zone_start: u32, zone_len: u32) -> bool {
        if zone_len == 0 {
            return false;
        }
        let Some(zone_end) = zone_start.checked_add(zone_len.saturating_sub(1)) else {
            return true;
        };
        !(end < zone_start || start > zone_end)
    }
}
