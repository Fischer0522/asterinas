// SPDX-License-Identifier: MPL-2.0

use core::mem::size_of;

use aster_virtio::device::socket::error;

use super::{
    block_group::{BlockGroup, RawGroupDesc},
    inode::{FilePerm, Inode, InodeDesc, RawInode},
    prelude::*,
    super_block::{RawSuperBlock, SUPER_BLOCK_OFFSET, SuperBlock},
    utils::{Dirty, now},
};
use crate::fs::{ext2::inode::InodeInner, utils::FsEventSubscriberStats};

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
    pub fn open(device: Arc<dyn BlockDevice>) -> Result<Arc<Self>> {
        let super_block = {
            let raw_super_block = device.read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)?;
            SuperBlock::try_from(raw_super_block)?
        };
        let block_size = super_block.block_size();
        assert_eq!(
            block_size, BLOCK_SIZE,
            "currently only 4096-byte block size"
        );

        let group_descriptors_segment: USegment = {
            let segment = FrameAllocOptions::new().zeroed(false).alloc_segment(1)?;
            let bio_segment =
                BioSegment::new_from_segment(segment.clone().into(), BioDirection::FromDevice);
            match device.read_blocks(super_block.group_descriptors_bid(0), bio_segment)? {
                BioStatus::Complete => {}
                err_status => {
                    ostd::early_println!(
                        "Ext2: Failed to read group descriptor table: {:?}",
                        err_status
                    );
                    return Err(Error::from(err_status));
                }
            }
            segment.into()
        };
        let block_group_descriptors_segment =
            Self::load_group_desc_table(device.as_ref(), &super_block)?;
        Ext2::check_group_desc_table(&super_block, &block_group_descriptors_segment)?;

        let block_groups = Self::load_block_groups(&super_block, &block_group_descriptors_segment)?;
        let inodes_per_group = super_block.inodes_per_group();

        //TODO: load root inode, aligning with Linux's ext2_fill_super

        let ext2 = Arc::new_cyclic(|weak_self| Ext2 {
            block_device: device,
            super_block: RwMutex::new(Dirty::new(super_block)),
            block_groups,
            inodes_per_group,
            blocks_per_group: super_block.blocks_per_group(),
            inode_size: super_block.inode_size(),
            block_size,
            group_descriptors_segment,
            fs_event_subscriber_stats: FsEventSubscriberStats::new(),
            self_ref: weak_self.clone(),
        });

        Ok(ext2)
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
    /// TODO: refactor this function into BlockGroup.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1387 (ext2_iget)
    pub(super) fn read_inode(&self, ino: u32) -> Result<Arc<Inode>> {
        let desc = self.read_inode_desc(ino)?;
        let desc = Dirty::new(desc);

        let inodes_per_group = self.super_block.read().inodes_per_group();
        let block_group_idx = ((ino - 1) / inodes_per_group) as usize;

        if self.self_ref.upgrade().is_none() {
            return_errno!(Errno::EIO);
        }

        let inode = Inode::new(
            ino,
            desc.type_(),
            desc,
            block_group_idx,
            self.self_ref.clone(),
        );
        Ok(inode)
    }

    /// Returns the inode table block ID for the given group.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1314 (ext2_get_inode)
    pub(super) fn inode_table_block(
        &self,
        group_idx: usize,
        table_block_index: u32,
    ) -> Result<Bid> {
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
        let mut buf = vec![0u8; block_size];
        if self
            .block_device
            .read_bytes(block_bid.to_offset(), &mut buf)
            .is_err()
        {
            return_errno!(Errno::EIO);
        }

        if offset_in_block + size_of::<RawInode>() > block_size {
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

        // TODO: remove this read when inode cache and page cache is enabled.
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
    pub(super) fn load_group_desc_table(
        block_device: &dyn BlockDevice,
        sb: &SuperBlock,
    ) -> Result<USegment> {
        let groups_count = sb.block_groups_count() as usize;
        let desc_bytes = groups_count * size_of::<RawGroupDesc>();
        let npages = desc_bytes.div_ceil(BLOCK_SIZE);

        let segment = FrameAllocOptions::new()
            .zeroed(false)
            .alloc_segment(npages)?;
        let bio_segment =
            BioSegment::new_from_segment(segment.clone().into(), BioDirection::FromDevice);
        match block_device.read_blocks(sb.group_descriptors_bid(0), bio_segment)? {
            BioStatus::Complete => {}
            err_status => {
                ostd::early_println!(
                    "Ext2: Failed to read group descriptor table: {:?}",
                    err_status
                );
                return Err(Error::from(err_status));
            }
        }
        let segment: USegment = segment.into();
        Self::check_group_desc_table(sb, &segment)?;
        Ok(segment)
    }

    /// Validates the group descriptor table.
    ///
    /// Linux: /root/linux/fs/ext2/super.c:695 (ext2_check_descriptors)
    pub(super) fn check_group_desc_table(sb: &SuperBlock, group_descs: &USegment) -> Result<()> {
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
                error!("Ext2: Block bitmap out of range");
                return_errno!(Errno::EINVAL);
            }
            if inode_bitmap < first_block || inode_bitmap > last_block {
                error!("Ext2: Inode bitmap out of range");
                return_errno!(Errno::EINVAL);
            }
            let table_last = inode_table.saturating_add(itb_per_group.saturating_sub(1));
            if inode_table < first_block || table_last > last_block {
                error!("Ext2: Inode table out of range");
                return_errno!(Errno::EINVAL);
            }
        }
        Ok(())
    }

    pub(super) fn load_block_groups(
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

        let (
            first_data_block,
            blocks_per_group,
            total_blocks,
            groups_count,
            itb_per_group,
            block_size,
        ) = {
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

            let group_first =
                Self::group_first_block_no(first_data_block, blocks_per_group, group_idx);
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

    /// Allocates and initializes a new inode.
    ///
    /// Linux: /root/linux/fs/ext2/ialloc.c:419 (ext2_new_inode)
    pub(super) fn create_inode(
        &self,
        parent_ino: u32,
        inode_type: InodeType,
        perm: FilePerm,
    ) -> Result<Arc<Inode>> {
        if inode_type == InodeType::Unknown {
            return_errno!(Errno::EINVAL);
        }

        let ino = self.alloc_inode(parent_ino, inode_type)?;
        // TODO: reduce this extra I/O operation after implementing inode cache.
        // SPEC: initialize a valid on-disk inode before publishing it.
        let mode = (inode_type as u16) | (perm.bits() & 0o07777);
        let links_count = if inode_type.is_directory() { 2 } else { 1 };
        let raw = RawInode {
            mode,
            uid: 0,
            size_lo: 0,
            atime: 0,
            ctime: 0,
            mtime: 0,
            dtime: 0,
            gid: 0,
            links_count,
            blocks: 0,
            flags: 0,
            osd1: 0,
            block: [0; 15],
            generation: 0,
            file_acl: 0,
            size_high: 0,
            faddr: 0,
            frag: 0,
            fsize: 0,
            pad1: 0,
            uid_high: 0,
            gid_high: 0,
            reserved2: 0,
        };

        if let Err(err) = self.write_inode_desc(ino, &raw) {
            // SPEC: cleanup inode allocation if descriptor initialization failed.
            let _ = self.free_inode(ino);
            return Err(err);
        }

        match self.read_inode(ino) {
            Ok(inode) => Ok(inode),
            Err(err) => {
                // SPEC: rollback allocated inode on publish failure.
                let _ = self.free_inode(ino);
                Err(err)
            }
        }
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
        // Group descriptor table is stored in whole filesystem blocks on disk.
        // `write_bytes` requires sector-aligned length, so flush a block-aligned span.
        let desc_disk_bytes = desc_bytes.div_ceil(BLOCK_SIZE) * BLOCK_SIZE;
        let mut desc_buf = vec![0u8; desc_disk_bytes];
        if self
            .group_descriptors_segment
            .read_bytes(0, &mut desc_buf)
            .is_err()
        {
            return_errno!(Errno::EIO);
        }
        let mut sb_guard = self.super_block.write();
        sb_guard.set_wtime(now());
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

        let sb_block = if block_size == SUPER_BLOCK_OFFSET {
            1u32
        } else {
            0u32
        };
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

#[cfg(ktest)]
mod test {

    use aster_block::bio::BioStatus;
    use ostd::{mm::VmIo, prelude::*};

    use super::*;
    use crate::fs::{
        ext2::test::{
            ErrorBioDisk, Ext2MemoryDisk, build_group_desc_segment, make_valid_group_desc,
            make_valid_raw_super_block, make_valid_super_block,
        },
        utils::FsEventSubscriberStats,
    };

    fn make_test_ext2(sb: SuperBlock, block_device: Arc<dyn BlockDevice>) -> Ext2 {
        let group_descriptors_segment: USegment = FrameAllocOptions::new()
            .zeroed(true)
            .alloc_segment(1)
            .unwrap()
            .into();

        Ext2 {
            block_device,
            super_block: RwMutex::new(Dirty::new(sb)),
            block_groups: Vec::new(),
            inodes_per_group: sb.inodes_per_group(),
            blocks_per_group: sb.blocks_per_group(),
            inode_size: sb.inode_size(),
            block_size: sb.block_size(),
            group_descriptors_segment,
            fs_event_subscriber_stats: FsEventSubscriberStats::new(),
            self_ref: Weak::new(),
        }
    }

    fn set_bit_lsb0(buf: &mut [u8], bit: usize) {
        let byte = bit / 8;
        let bit_in_byte = bit % 8;
        buf[byte] |= 1u8 << bit_in_byte;
    }

    fn make_raw_inode(mode: u16, links_count: u16, dtime: u32) -> RawInode {
        RawInode {
            mode,
            uid: 0,
            size_lo: 0,
            atime: 0,
            ctime: 0,
            mtime: 0,
            dtime,
            gid: 0,
            links_count,
            blocks: 0,
            flags: 0,
            osd1: 0,
            block: [0; 15],
            generation: 0,
            file_acl: 0,
            size_high: 0,
            faddr: 0,
            frag: 0,
            fsize: 0,
            pad1: 0,
            uid_high: 0,
            gid_high: 0,
            reserved2: 0,
        }
    }

    fn initialize_disk_for_open(sb: &SuperBlock, descs: &[RawGroupDesc], disk: &Ext2MemoryDisk) {
        let raw_sb = RawSuperBlock::from(sb);
        disk.write_super_block(&raw_sb);
        disk.write_group_desc_table(sb, descs);
    }

    fn write_raw_inode_to_disk(
        sb: &SuperBlock,
        descs: &[RawGroupDesc],
        ino: u32,
        raw: &RawInode,
        disk: &Ext2MemoryDisk,
    ) {
        let inodes_per_group = sb.inodes_per_group();
        let group_idx = ((ino - 1) / inodes_per_group) as usize;
        let index_in_group = (ino - 1) % inodes_per_group;

        let inode_size = sb.inode_size();
        let block_size = sb.block_size();
        let offset_bytes = (index_in_group as usize).saturating_mul(inode_size);
        let block_index = offset_bytes / block_size;
        let offset_in_block = offset_bytes % block_size;

        let table_block = descs[group_idx].inode_table + block_index as u32;
        let table_bid = Bid::new(table_block as u64);
        disk.segment()
            .write_val(table_bid.to_offset() + offset_in_block, raw)
            .unwrap();
    }

    fn read_raw_inode_from_disk(
        sb: &SuperBlock,
        descs: &[RawGroupDesc],
        ino: u32,
        disk: &Ext2MemoryDisk,
    ) -> RawInode {
        let inodes_per_group = sb.inodes_per_group();
        let group_idx = ((ino - 1) / inodes_per_group) as usize;
        let index_in_group = (ino - 1) % inodes_per_group;

        let inode_size = sb.inode_size();
        let block_size = sb.block_size();
        let offset_bytes = (index_in_group as usize).saturating_mul(inode_size);
        let block_index = offset_bytes / block_size;
        let offset_in_block = offset_bytes % block_size;

        let table_block = descs[group_idx].inode_table + block_index as u32;
        let table_bid = Bid::new(table_block as u64);
        disk.segment()
            .read_val::<RawInode>(table_bid.to_offset() + offset_in_block)
            .unwrap()
    }

    fn make_test_ext2_for_inode_alloc(
        sb_free_inodes: u32,
        group_free_inodes: u16,
        fill_all_inode_bits: bool,
    ) -> (Ext2, SuperBlock, Vec<RawGroupDesc>) {
        let mut sb = make_valid_super_block(1);
        for _ in 0..sb_free_inodes {
            sb.inc_free_inodes();
        }

        let mut descs = vec![make_valid_group_desc(&sb, 0)];
        descs[0].free_inodes_count = group_free_inodes;
        let group_descs = build_group_desc_segment(&sb, &descs);

        let disk = Arc::new(Ext2MemoryDisk::new(128));
        initialize_disk_for_open(&sb, &descs, &disk);

        let mut inode_bitmap = [0u8; BLOCK_SIZE];
        let first_ino = sb.first_ino() as usize;
        // Reserve inode numbers [1, first_ino).
        for bit in 0..first_ino.saturating_sub(1) {
            set_bit_lsb0(&mut inode_bitmap, bit);
        }
        if fill_all_inode_bits {
            for bit in 0..(sb.inodes_per_group() as usize) {
                set_bit_lsb0(&mut inode_bitmap, bit);
            }
        }

        disk.segment()
            .write_bytes(
                Bid::new(descs[0].inode_bitmap as u64).to_offset(),
                &inode_bitmap,
            )
            .unwrap();

        let mut ext2 = make_test_ext2(sb, disk as Arc<dyn BlockDevice>);
        ext2.block_groups =
            Ext2::load_block_groups(&ext2.super_block.read(), &group_descs).unwrap();

        let sb = **ext2.super_block.read();
        (ext2, sb, descs)
    }

    fn make_test_ext2_for_block_alloc(
        sb_free_blocks: u32,
        group_free_blocks: u16,
        fill_all_data_bits: bool,
    ) -> Ext2 {
        // Build a single-group Ext2 test fixture with controllable free-block counters.
        let mut raw_sb = make_valid_raw_super_block(1);
        raw_sb.free_blocks_count = sb_free_blocks;
        let sb = SuperBlock::try_from(raw_sb).unwrap();

        let mut descs = vec![make_valid_group_desc(&sb, 0)];
        descs[0].free_blocks_count = group_free_blocks;
        let group_descs = build_group_desc_segment(&sb, &descs);

        let disk = Arc::new(Ext2MemoryDisk::new(128));
        initialize_disk_for_open(&sb, &descs, &disk);

        let mut bitmap_block = [0u8; BLOCK_SIZE];
        let itb = sb.itb_per_group() as usize;
        // Reserve system-zone bits: block bitmap, inode bitmap, and inode-table blocks.
        for bit in 0..(2 + itb) {
            set_bit_lsb0(&mut bitmap_block, bit);
        }

        if fill_all_data_bits {
            // Force ENOSPC/EIO paths by marking all group bits as allocated.
            let first = sb.group_first_block_no(0);
            let last = sb.group_last_block_no(0);
            let group_size = (last - first + 1) as usize;
            for bit in 0..group_size {
                set_bit_lsb0(&mut bitmap_block, bit);
            }
        }

        disk.segment()
            .write_bytes(
                Bid::new(descs[0].block_bitmap as u64).to_offset(),
                &bitmap_block,
            )
            .unwrap();

        let mut ext2 = make_test_ext2(sb, disk as Arc<dyn BlockDevice>);
        ext2.block_groups =
            Ext2::load_block_groups(&ext2.super_block.read(), &group_descs).unwrap();
        ext2
    }

    #[ktest]
    fn sync_metadata_flushes_primary_and_backup_copies() {
        let sb = make_valid_super_block(3);
        let mut descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();
        descs[0].free_blocks_count = 10;

        let disk = Arc::new(Ext2MemoryDisk::new(512));
        initialize_disk_for_open(&sb, &descs, &disk);
        let ext2 = Ext2::open(disk.clone()).unwrap();

        let expected_free_inodes = {
            let mut sb_guard = ext2.super_block.write();
            sb_guard.inc_free_inodes();
            sb_guard.free_inodes_count()
        };

        ext2.block_groups[0].inc_free_blocks(3);
        assert!(ext2.block_groups[0].is_desc_dirty());

        ext2.sync_metadata().unwrap();

        assert!(!ext2.super_block.read().is_dirty());
        assert!(!ext2.block_groups[0].is_desc_dirty());

        let groups_count = sb.block_groups_count() as usize;
        let desc_bytes = groups_count * size_of::<RawGroupDesc>();
        let primary_desc_offset = sb.group_descriptors_bid(0).to_offset();

        let mut primary_desc = vec![0u8; desc_bytes];
        disk.segment()
            .read_bytes(primary_desc_offset, &mut primary_desc)
            .unwrap();
        let first_desc = disk
            .segment()
            .read_val::<RawGroupDesc>(primary_desc_offset)
            .unwrap();
        assert_eq!(first_desc.free_blocks_count, 13);

        let primary_sb = disk
            .segment()
            .read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)
            .unwrap();
        assert_eq!(primary_sb.free_inodes_count, expected_free_inodes);

        for idx in 1..groups_count {
            if !sb.is_backup_group(idx) {
                continue;
            }

            let backup_sb = disk
                .segment()
                .read_val::<RawSuperBlock>(sb.bid(idx).to_offset())
                .unwrap();
            assert_eq!(backup_sb.block_group_idx, idx as u16);

            let mut primary_cmp = primary_sb;
            let mut backup_cmp = backup_sb;
            primary_cmp.block_group_idx = 0;
            backup_cmp.block_group_idx = 0;
            assert_eq!(backup_cmp.as_bytes(), primary_cmp.as_bytes());

            let mut backup_desc = vec![0u8; desc_bytes];
            disk.segment()
                .read_bytes(sb.group_descriptors_bid(idx).to_offset(), &mut backup_desc)
                .unwrap();
            assert_eq!(backup_desc, primary_desc);
        }
    }

    #[ktest]
    fn block_alloc_free_ok() {
        // Happy path: allocate a contiguous run and then free it back.
        let ext2 = make_test_ext2_for_block_alloc(31, 31, false);

        let before_sb_free = ext2.super_block.read().free_blocks_count();
        let before_group_free = ext2.block_groups[0].free_blocks_count();

        let range = ext2.alloc_blocks(8).unwrap();
        let alloc_len = range.end - range.start;
        assert!(alloc_len >= 1 && alloc_len <= 8);

        {
            let sb = ext2.super_block.read();
            // Allocation must stay inside data zone and within one block group.
            assert!(sb.data_block_valid(range.start, alloc_len));
            let start_group = (range.start - sb.first_data_block()) / sb.blocks_per_group();
            let end_group = (range.end - 1 - sb.first_data_block()) / sb.blocks_per_group();
            assert_eq!(start_group, end_group);
        }

        assert_eq!(
            ext2.block_groups[0].free_blocks_count(),
            before_group_free - alloc_len as u16
        );
        assert_eq!(
            ext2.super_block.read().free_blocks_count(),
            before_sb_free - alloc_len
        );

        ext2.free_blocks(range.start, alloc_len).unwrap();
        assert_eq!(ext2.block_groups[0].free_blocks_count(), before_group_free);
        assert_eq!(ext2.super_block.read().free_blocks_count(), before_sb_free);
    }

    #[ktest]
    fn block_alloc_free_error_cases() {
        // No-space and invalid-request checks.
        let ext2_nospc = make_test_ext2_for_block_alloc(0, 0, false);
        assert_eq!(
            ext2_nospc.alloc_blocks(1).unwrap_err().error(),
            Errno::ENOSPC
        );
        assert_eq!(
            ext2_nospc.alloc_blocks(0).unwrap_err().error(),
            Errno::EINVAL
        );

        // Inconsistent counters/bitmap shape should surface as EIO on allocation.
        let ext2_corrupt = make_test_ext2_for_block_alloc(1, 1, true);
        assert_eq!(
            ext2_corrupt.alloc_blocks(1).unwrap_err().error(),
            Errno::EIO
        );

        // Free-path boundary and system-zone guards.
        let ext2_free = make_test_ext2_for_block_alloc(31, 31, false);
        assert!(ext2_free.free_blocks(10, 0).is_ok());
        assert_eq!(ext2_free.free_blocks(1, 1).unwrap_err().error(), Errno::EIO);

        let inode_bitmap_bid = ext2_free.block_groups[0].inode_bitmap_bid().to_raw() as u32;
        assert_eq!(
            ext2_free
                .free_blocks(inode_bitmap_bid, 1)
                .unwrap_err()
                .error(),
            Errno::EIO
        );
    }

    #[ktest]
    fn inode_alloc_free_ok() {
        // Allocate one directory inode and verify bitmap/counter transitions.
        let (ext2, sb, descs) = make_test_ext2_for_inode_alloc(16, 16, false);

        let before_sb_free = ext2.super_block.read().free_inodes_count();
        let before_group_free = ext2.block_groups[0].free_inodes_count();
        let before_used_dirs = ext2.block_groups[0].used_dirs_count();

        let ino = ext2.alloc_inode(ROOT_INO, InodeType::Dir).unwrap();
        assert!(ino >= sb.first_ino() && ino <= sb.total_inodes());

        let bit = ((ino - 1) % sb.inodes_per_group()) as u16;
        let bitmap = {
            let sb_guard = ext2.super_block.read();
            ext2.block_groups[0]
                .load_inode_bitmap(&ext2, &sb_guard)
                .unwrap()
        };
        assert!(bitmap.is_allocated(bit));

        assert_eq!(
            ext2.super_block.read().free_inodes_count(),
            before_sb_free - 1
        );
        assert_eq!(
            ext2.block_groups[0].free_inodes_count(),
            before_group_free - 1
        );
        assert_eq!(ext2.block_groups[0].used_dirs_count(), before_used_dirs + 1);

        // Free path uses read_inode_desc; write a valid on-disk directory inode first.
        let raw_dir = make_raw_inode(0o040755, 1, 0);
        let disk = ext2.block_device.downcast_ref::<Ext2MemoryDisk>().unwrap();
        write_raw_inode_to_disk(&sb, &descs, ino, &raw_dir, disk);

        ext2.free_inode(ino).unwrap();
        assert_eq!(ext2.super_block.read().free_inodes_count(), before_sb_free);
        assert_eq!(ext2.block_groups[0].free_inodes_count(), before_group_free);
        assert_eq!(ext2.block_groups[0].used_dirs_count(), before_used_dirs);
    }

    #[ktest]
    fn inode_alloc_free_error_cases() {
        // No free inode counter means ENOSPC without bitmap scan.
        let (ext2_nospc, sb_nospc, _descs) = make_test_ext2_for_inode_alloc(0, 0, false);
        assert_eq!(
            ext2_nospc
                .alloc_inode(ROOT_INO, InodeType::File)
                .unwrap_err()
                .error(),
            Errno::ENOSPC
        );
        assert_eq!(
            ext2_nospc
                .alloc_inode(sb_nospc.total_inodes() + 1, InodeType::File)
                .unwrap_err()
                .error(),
            Errno::EIO
        );

        // All inode bitmap bits set -> no allocatable inode.
        let (ext2_full, _sb_full, _descs) = make_test_ext2_for_inode_alloc(8, 8, true);
        assert_eq!(
            ext2_full
                .alloc_inode(ROOT_INO, InodeType::File)
                .unwrap_err()
                .error(),
            Errno::ENOSPC
        );

        let (ext2_free, sb_free, descs_free) = make_test_ext2_for_inode_alloc(8, 8, false);
        assert_eq!(
            ext2_free
                .free_inode(sb_free.first_ino() - 1)
                .unwrap_err()
                .error(),
            Errno::EIO
        );

        // Already-free inode: should return Ok and keep counters unchanged.
        let target_ino = sb_free.first_ino();
        let raw_file = make_raw_inode(0o100644, 1, 0);
        let disk = ext2_free
            .block_device
            .downcast_ref::<Ext2MemoryDisk>()
            .unwrap();
        write_raw_inode_to_disk(&sb_free, &descs_free, target_ino, &raw_file, disk);

        let before_sb = ext2_free.super_block.read().free_inodes_count();
        let before_group = ext2_free.block_groups[0].free_inodes_count();
        ext2_free.free_inode(target_ino).unwrap();
        assert_eq!(ext2_free.super_block.read().free_inodes_count(), before_sb);
        assert_eq!(ext2_free.block_groups[0].free_inodes_count(), before_group);
    }

    #[ktest]
    fn create_inode_initializes_descriptor() {
        let mut raw_sb = make_valid_raw_super_block(1);
        raw_sb.free_inodes_count = 16;
        let sb = SuperBlock::try_from(raw_sb).unwrap();

        let mut descs = vec![make_valid_group_desc(&sb, 0)];
        descs[0].free_inodes_count = 16;

        let disk = Arc::new(Ext2MemoryDisk::new(128));
        initialize_disk_for_open(&sb, &descs, &disk);

        let mut inode_bitmap = [0u8; BLOCK_SIZE];
        for bit in 0..(sb.first_ino() as usize).saturating_sub(1) {
            set_bit_lsb0(&mut inode_bitmap, bit);
        }
        disk.segment()
            .write_bytes(
                Bid::new(descs[0].inode_bitmap as u64).to_offset(),
                &inode_bitmap,
            )
            .unwrap();

        let ext2 = Ext2::open(disk.clone() as Arc<dyn BlockDevice>).unwrap();
        let inode = ext2
            .create_inode(
                ROOT_INO,
                InodeType::Dir,
                crate::fs::ext2::inode::FilePerm::from_bits_truncate(0o755),
            )
            .unwrap();
        let ino = inode.ino();

        let raw = read_raw_inode_from_disk(&sb, &descs, ino, disk.as_ref());
        assert_eq!(raw.mode, 0o040755);
        assert_eq!(raw.links_count, 2);
        assert_eq!(raw.size_lo, 0);
        assert_eq!(raw.blocks, 0);
        assert_eq!(raw.block, [0; 15]);

        assert_eq!(
            ext2.create_inode(
                ROOT_INO,
                InodeType::Unknown,
                crate::fs::ext2::inode::FilePerm::from_bits_truncate(0o644)
            )
            .unwrap_err()
            .error(),
            Errno::EINVAL
        );
    }

    #[ktest]
    fn group_bounds_ok() {
        let sb = make_valid_super_block(3);

        assert_eq!(sb.group_first_block_no(0), 1);
        assert_eq!(sb.group_first_block_no(1), 1 + sb.blocks_per_group());
        assert_eq!(
            sb.group_last_block_no(0),
            sb.group_first_block_no(0) + sb.blocks_per_group() - 1
        );

        let last_group = sb.block_groups_count() as usize - 1;
        assert_eq!(sb.group_last_block_no(last_group), sb.total_blocks() - 1);
    }

    #[ktest]
    fn reject_bad_bitmap() {
        let sb = make_valid_super_block(2);
        let mut descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();

        descs[0].block_bitmap = sb.group_first_block_no(0).saturating_sub(1);

        let group_descs = build_group_desc_segment(&sb, &descs);
        let ext2 = make_test_ext2(sb, Arc::new(Ext2MemoryDisk::new(64)));

        let err = Ext2::check_group_desc_table(&sb, &group_descs).unwrap_err();
        assert_eq!(err.error(), Errno::EINVAL);
    }

    #[ktest]
    fn reject_bad_inode_table() {
        let sb = make_valid_super_block(2);
        let mut descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();

        let first = sb.group_first_block_no(0);
        let last = sb.group_last_block_no(0);
        let itb = sb.itb_per_group();
        descs[0].inode_table = last.saturating_sub(itb.saturating_sub(2));
        assert!(descs[0].inode_table >= first);

        let group_descs = build_group_desc_segment(&sb, &descs);
        let ext2 = make_test_ext2(sb, Arc::new(Ext2MemoryDisk::new(64)));

        let err = Ext2::check_group_desc_table(&sb, &group_descs).unwrap_err();
        assert_eq!(err.error(), Errno::EINVAL);
    }

    #[ktest]
    fn load_descriptors_ok() {
        let sb = make_valid_super_block(3);
        let descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();
        let disk = Ext2MemoryDisk::new(64);
        disk.write_group_desc_table(&sb, &descs);

        let loaded = Ext2::load_group_desc_table(&disk, &sb).unwrap();
        let first_desc = loaded.read_val::<RawGroupDesc>(0).unwrap();

        assert_eq!(first_desc.block_bitmap, descs[0].block_bitmap);
        assert_eq!(first_desc.inode_bitmap, descs[0].inode_bitmap);
        assert_eq!(first_desc.inode_table, descs[0].inode_table);
    }

    #[ktest]
    fn load_descriptors_io_error() {
        let sb = make_valid_super_block(1);
        let disk = ErrorBioDisk::new(BioStatus::IoError, 64 * BLOCK_SIZE / SECTOR_SIZE);

        let err = Ext2::load_group_desc_table(&disk, &sb).unwrap_err();
        assert_eq!(err.error(), Errno::EIO);
    }

    #[ktest]
    fn load_descriptors_bad_descriptor() {
        let sb = make_valid_super_block(2);
        let mut descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();
        descs[1].inode_bitmap = sb.group_last_block_no(1).saturating_add(1);

        let disk = Ext2MemoryDisk::new(64);
        disk.write_group_desc_table(&sb, &descs);

        let ext2 = make_test_ext2(sb, Arc::new(disk));
        let disk = ext2.block_device;
        let err = Ext2::load_group_desc_table(disk.as_ref(), &sb).unwrap_err();
        assert_eq!(err.error(), Errno::EINVAL);
    }

    #[ktest]
    fn read_inode_desc_ok() {
        let sb = make_valid_super_block(2);
        let descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();
        let group_descs = build_group_desc_segment(&sb, &descs);

        let raw = make_raw_inode(0o040755, 2, 0);
        let disk = Ext2MemoryDisk::new(128);
        write_raw_inode_to_disk(&sb, &descs, ROOT_INO, &raw, &disk);

        let mut ext2 = make_test_ext2(sb, Arc::new(disk));
        ext2.block_groups = Ext2::load_block_groups(&sb, &group_descs).unwrap();

        let bid = ext2.inode_table_block(1, 3).unwrap();
        let base = Bid::new(descs[1].inode_table as u64);
        assert_eq!(bid, base + 3);

        let desc = ext2.read_inode_desc(ROOT_INO).unwrap();
        assert_eq!(desc.type_(), InodeType::Dir);
    }

    #[ktest]
    fn read_inode_desc_error() {
        let sb = make_valid_super_block(2);
        let descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();
        let group_descs = build_group_desc_segment(&sb, &descs);

        let mut ext2 = make_test_ext2(sb, Arc::new(Ext2MemoryDisk::new(128)));
        ext2.block_groups = Ext2::load_block_groups(&sb, &group_descs).unwrap();

        let group_err = ext2.inode_table_block(2, 0).unwrap_err();
        assert_eq!(group_err.error(), Errno::EIO);

        let invalid_low = ext2.read_inode_desc(1).unwrap_err();
        assert_eq!(invalid_low.error(), Errno::EINVAL);

        let invalid_high = ext2
            .read_inode_desc(sb.total_inodes().saturating_add(1))
            .unwrap_err();
        assert_eq!(invalid_high.error(), Errno::EINVAL);

        let io_disk = ErrorBioDisk::new(BioStatus::IoError, 128 * BLOCK_SIZE / SECTOR_SIZE);
        let mut ext2_io = make_test_ext2(sb, Arc::new(io_disk));
        ext2_io.block_groups = Ext2::load_block_groups(&sb, &group_descs).unwrap();
        let io_err = ext2_io.read_inode_desc(ROOT_INO).unwrap_err();
        assert_eq!(io_err.error(), Errno::EIO);

        let ino = sb.first_ino();
        let raw = make_raw_inode(0, 0, 1);
        let parse_disk = Ext2MemoryDisk::new(128);
        write_raw_inode_to_disk(&sb, &descs, ino, &raw, &parse_disk);

        let mut ext2_parse = make_test_ext2(sb, Arc::new(parse_disk));
        ext2_parse.block_groups = Ext2::load_block_groups(&sb, &group_descs).unwrap();
        let parse_err = ext2_parse.read_inode_desc(ino).unwrap_err();
        assert_eq!(parse_err.error(), Errno::ESTALE);
    }

    #[ktest]
    fn read_inode_ok() {
        let sb = make_valid_super_block(2);
        let descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();

        let raw = make_raw_inode(0o040755, 2, 0);
        let disk = Ext2MemoryDisk::new(128);
        initialize_disk_for_open(&sb, &descs, &disk);
        write_raw_inode_to_disk(&sb, &descs, ROOT_INO, &raw, &disk);

        let ext2 = Ext2::open(Arc::new(disk)).unwrap();

        assert!(ext2.read_inode(ROOT_INO).is_ok());
    }

    #[ktest]
    fn read_inode_error() {
        let sb = make_valid_super_block(2);
        let descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();

        let invalid_disk = Ext2MemoryDisk::new(128);
        initialize_disk_for_open(&sb, &descs, &invalid_disk);
        let ext2_invalid = Ext2::open(Arc::new(invalid_disk)).unwrap();
        let invalid_err = ext2_invalid.read_inode(1).unwrap_err();
        assert_eq!(invalid_err.error(), Errno::EINVAL);

        let raw_deleted = make_raw_inode(0, 0, 1);
        let deleted_ino = sb.first_ino();
        let deleted_disk = Ext2MemoryDisk::new(128);
        initialize_disk_for_open(&sb, &descs, &deleted_disk);
        write_raw_inode_to_disk(&sb, &descs, deleted_ino, &raw_deleted, &deleted_disk);

        let ext2_deleted = Ext2::open(Arc::new(deleted_disk)).unwrap();
        let deleted_err = ext2_deleted.read_inode(deleted_ino).unwrap_err();
        assert_eq!(deleted_err.error(), Errno::ESTALE);

        let raw_ok = make_raw_inode(0o040755, 2, 0);
        let unwired_disk = Ext2MemoryDisk::new(128);
        initialize_disk_for_open(&sb, &descs, &unwired_disk);
        write_raw_inode_to_disk(&sb, &descs, ROOT_INO, &raw_ok, &unwired_disk);

        let mut ext2_unwired = make_test_ext2(sb, Arc::new(unwired_disk));
        let group_descs = build_group_desc_segment(&sb, &descs);
        ext2_unwired.block_groups = Ext2::load_block_groups(&sb, &group_descs).unwrap();
        let unwired_err = ext2_unwired.read_inode(ROOT_INO).unwrap_err();
        assert_eq!(unwired_err.error(), Errno::EIO);
    }

    #[ktest]
    fn load_block_bitmap_ok() {
        let sb = make_valid_super_block(2);
        let descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();
        let group_descs = build_group_desc_segment(&sb, &descs);
        let group = BlockGroup::load(&group_descs, 0).unwrap();

        let mut bitmap_block = [0u8; BLOCK_SIZE];
        let itb = sb.itb_per_group() as usize;
        for bit in 0..(2 + itb) {
            set_bit_lsb0(&mut bitmap_block, bit);
        }

        let disk = Ext2MemoryDisk::new(128);
        disk.segment()
            .write_bytes(group.block_bitmap_bid().to_offset(), &bitmap_block)
            .unwrap();
        let ext2 = make_test_ext2(sb, Arc::new(disk));

        let bitmap = group.load_block_bitmap(&ext2, &sb).unwrap();
        assert!(bitmap.is_allocated(0));
        assert!(bitmap.is_allocated(1));
        assert!(bitmap.is_allocated((itb + 1) as u16));
    }

    #[ktest]
    fn load_block_bitmap_bad_inode_table_bits() {
        let sb = make_valid_super_block(2);
        let descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();
        let group_descs = build_group_desc_segment(&sb, &descs);
        let group = BlockGroup::load(&group_descs, 0).unwrap();

        let mut bitmap_block = [0u8; BLOCK_SIZE];
        set_bit_lsb0(&mut bitmap_block, 0);
        set_bit_lsb0(&mut bitmap_block, 1);

        let disk = Ext2MemoryDisk::new(128);
        disk.segment()
            .write_bytes(group.block_bitmap_bid().to_offset(), &bitmap_block)
            .unwrap();
        let ext2 = make_test_ext2(sb, Arc::new(disk));

        let err = group.load_block_bitmap(&ext2, &sb).unwrap_err();
        assert_eq!(err.error(), Errno::EINVAL);
    }

    #[ktest]
    fn load_inode_bitmap_ok() {
        let sb = make_valid_super_block(2);
        let descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();
        let group_descs = build_group_desc_segment(&sb, &descs);
        let group = BlockGroup::load(&group_descs, 0).unwrap();

        let mut bitmap_block = [0u8; BLOCK_SIZE];
        set_bit_lsb0(&mut bitmap_block, 0);
        set_bit_lsb0(&mut bitmap_block, 8);

        let disk = Ext2MemoryDisk::new(128);
        disk.segment()
            .write_bytes(group.inode_bitmap_bid().to_offset(), &bitmap_block)
            .unwrap();
        let ext2 = make_test_ext2(sb, Arc::new(disk));

        let bitmap = group.load_inode_bitmap(&ext2, &sb).unwrap();
        assert_eq!(bitmap.len(), sb.inodes_per_group() as u16);
        assert!(bitmap.is_allocated(0));
        assert!(bitmap.is_allocated(8));
        assert!(!bitmap.is_allocated(1));
    }

    #[ktest]
    fn load_block_groups_bad_table() {
        let sb = make_valid_super_block(200);
        let segment = FrameAllocOptions::new()
            .zeroed(true)
            .alloc_segment(1)
            .unwrap();
        let group_descs: USegment = segment.into();

        let err = Ext2::load_block_groups(&sb, &group_descs).unwrap_err();
        assert_eq!(err.error(), Errno::EIO);
    }
}
