// SPDX-License-Identifier: MPL-2.0

use core::{
    mem::size_of,
    sync::atomic::{AtomicU32, Ordering},
};

use aster_block::bio::BioCompleteFn;
use device_id::DeviceId;

use super::{
    block_group::{BlockGroup, RawGroupDesc},
    inode::{FilePerm, Inode, InodeDesc, RawInode},
    inode_block_map::Ext2Bid,
    prelude::*,
    super_block::{RawSuperBlock, SUPER_BLOCK_OFFSET, SuperBlock},
    utils::{Dirty, now},
};
use crate::{
    fs::vfs::file_system::FsEventSubscriberStats,
    process::{Gid, credentials::capabilities::CapSet, posix_thread::AsPosixThread},
    thread::Thread,
};

/// The root inode number defined by the ext2 on-disk format.
pub const ROOT_INO: u32 = 2;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum StatfsMode {
    #[default]
    BsdDf,
    MinixDf,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Ext2MountOptions {
    statfs_mode: StatfsMode,
}

impl Ext2MountOptions {
    /// Parses the subset of ext2 mount options that affects `statfs`.
    ///
    fn parse(data: Option<&CStr>) -> Self {
        let mut options = Self::default();
        let Some(data) = data else {
            return options;
        };

        let data = data.to_string_lossy();
        for token in data.split(',') {
            match token.trim() {
                "bsddf" => options.statfs_mode = StatfsMode::BsdDf,
                "minixdf" => options.statfs_mode = StatfsMode::MinixDf,
                _ => {}
            }
        }

        options
    }

    fn uses_minix_df(self) -> bool {
        matches!(self.statfs_mode, StatfsMode::MinixDf)
    }
}

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
    /// Block size in bytes.
    block_size: usize,
    /// Group descriptor table segment.
    group_descriptors_segment: USegment,
    /// Runtime mount options that affect statfs projection.
    mount_options: Ext2MountOptions,
    /// FS event stats for VFS.
    fs_event_subscriber_stats: FsEventSubscriberStats,
    /// Per-filesystem inode generation counter.
    next_generation: AtomicU32,
    /// Weak self reference for inode back-pointers.
    self_ref: Weak<Ext2>,
}

impl Ext2 {
    /// Opens and loads an Ext2 filesystem from a block device.
    pub(super) fn open(device: Arc<dyn BlockDevice>, data: Option<&CStr>) -> Result<Arc<Self>> {
        Self::open_with_mount_options(device, Ext2MountOptions::parse(data))
    }

    fn open_with_mount_options(
        device: Arc<dyn BlockDevice>,
        mount_options: Ext2MountOptions,
    ) -> Result<Arc<Self>> {
        let super_block = {
            let raw_super_block = device.read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)?;
            SuperBlock::try_from(raw_super_block)?
        };
        let block_size = super_block.block_size();
        assert_eq!(
            block_size, BLOCK_SIZE,
            "currently only 4096-byte block size"
        );

        let group_descriptors_segment = Self::load_group_desc_table(device.as_ref(), &super_block)?;
        Ext2::check_group_desc_table(&super_block, &group_descriptors_segment)?;
        let inodes_per_group = super_block.inodes_per_group();

        let block_groups =
            Self::load_block_groups(&super_block, &group_descriptors_segment, device.clone())?;

        let ext2 = Arc::new_cyclic(|weak_self| Ext2 {
            block_groups,
            block_device: device,
            super_block: RwMutex::new(Dirty::new(super_block)),
            inodes_per_group,
            block_size,
            group_descriptors_segment,
            mount_options,
            fs_event_subscriber_stats: FsEventSubscriberStats::new(),
            next_generation: AtomicU32::new(now().as_secs() as u32),
            self_ref: weak_self.clone(),
        });

        Ok(ext2)
    }

    /// Returns the block device.
    pub(super) fn block_device(&self) -> &dyn BlockDevice {
        self.block_device.as_ref()
    }

    /// Returns the block size in bytes.
    pub(super) fn block_size(&self) -> usize {
        self.block_size
    }

    /// Returns the maximum regular file size supported by this ext2 instance.
    ///
    /// Linux: `/root/linux/fs/ext2/super.c` (`ext2_max_size`)
    pub(super) fn max_file_size(&self) -> usize {
        self.super_block.read().max_file_size()
    }

    /// Returns whether `statfs` should report Minix-style total blocks.
    ///
    pub(super) fn uses_minix_df(&self) -> bool {
        self.mount_options.uses_minix_df()
    }

    pub(super) fn block_group(&self, idx: usize) -> &BlockGroup {
        &self.block_groups[idx]
    }

    /// Returns a read guard of the superblock.
    pub(super) fn super_block(&self) -> RwMutexReadGuard<'_, Dirty<SuperBlock>> {
        self.super_block.read()
    }

    pub(super) fn container_device_id(&self) -> DeviceId {
        self.block_device.id()
    }

    /// Returns the fs event subscriber stats.
    pub(super) fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        &self.fs_event_subscriber_stats
    }

    /// Returns the root inode.
    pub(super) fn root_inode(&self) -> Result<Arc<Inode>> {
        self.read_inode(ROOT_INO)
    }

    /// Reads an inode via per-block-group inode cache.
    ///
    pub(super) fn read_inode(&self, ino: u32) -> Result<Arc<Inode>> {
        if self.self_ref.upgrade().is_none() {
            return_errno_with_message!(Errno::EIO, "filesystem already dropped");
        }

        let sb = self.super_block.read();
        if ino == 0 || ((ino != ROOT_INO && ino < sb.first_ino()) || ino > sb.total_inodes()) {
            return_errno_with_message!(Errno::EINVAL, "inode number out of valid range");
        }
        let inodes_per_group = sb.inodes_per_group();
        drop(sb);

        let group_idx = ((ino - 1) / inodes_per_group) as usize;
        let inode_idx = (ino - 1) % inodes_per_group;

        let group = self
            .block_groups
            .get(group_idx)
            .ok_or_else(|| Error::with_message(Errno::EIO, "block group index out of range"))?;
        group.lookup_inode(inode_idx, ino, self.self_ref.clone())
    }

    /// Inserts a newly created inode into the corresponding block-group cache.
    pub(super) fn insert_inode_cache(&self, inode: Arc<Inode>) {
        let ino = inode.ino();
        if ino == 0 {
            return;
        }
        let group_idx = ((ino - 1) / self.inodes_per_group) as usize;
        let inode_idx = (ino - 1) % self.inodes_per_group;
        if let Some(group) = self.block_groups.get(group_idx) {
            group.insert_cache(inode_idx, inode);
        }
    }

    /// Removes one inode from the live block-group cache.
    ///
    pub(super) fn remove_inode_cache(&self, ino: u32) -> Option<Arc<Inode>> {
        if ino == 0 {
            return None;
        }
        let group_idx = ((ino - 1) / self.inodes_per_group) as usize;
        let inode_idx = (ino - 1) % self.inodes_per_group;
        self.block_groups
            .get(group_idx)
            .and_then(|group| group.remove_inode_cache(inode_idx))
    }

    /// Returns the inode table block ID for the given group.
    ///
    pub(super) fn inode_table_block(
        &self,
        group_idx: usize,
        table_block_index: u32,
    ) -> Result<Ext2Bid> {
        let group = self
            .block_groups
            .get(group_idx)
            .ok_or_else(|| Error::with_message(Errno::EIO, "block group index out of range"))?;
        Ok(group.inode_table_bid() + table_block_index)
    }

    /// Reads an inode descriptor from the group's `PageCache`.
    pub(super) fn read_inode_desc(&self, ino: u32) -> Result<InodeDesc> {
        let sb = self.super_block.read();
        Self::read_inode_desc_from_parts(&sb, &self.block_groups, ino)
    }

    /// Reads an inode descriptor from preloaded superblock and block groups.
    fn read_inode_desc_from_parts(
        sb: &SuperBlock,
        block_groups: &[BlockGroup],
        ino: u32,
    ) -> Result<InodeDesc> {
        // SPEC: apply ext2 inode-number validity rules before indexing groups.
        if (ino != ROOT_INO && ino < sb.first_ino()) || ino > sb.total_inodes() {
            return_errno_with_message!(Errno::EINVAL, "inode number out of valid range");
        }

        let inodes_per_group = sb.inodes_per_group();
        let group_idx = ((ino - 1) / inodes_per_group) as usize;
        let index_in_group = (ino - 1) % inodes_per_group;

        let group = block_groups
            .get(group_idx)
            .ok_or_else(|| Error::with_message(Errno::EIO, "block group index out of range"))?;

        group.read_inode_desc(index_in_group)
    }

    /// Writes an inode descriptor to the group's `PageCache`.
    pub(super) fn write_inode_desc(&self, ino: u32, raw: &RawInode) -> Result<()> {
        let sb = self.super_block.read();

        // SPEC: apply ext2 inode-number validity rules before indexing groups.
        if (ino != ROOT_INO && ino < sb.first_ino()) || ino > sb.total_inodes() {
            return_errno_with_message!(Errno::EINVAL, "inode number out of valid range");
        }

        let inodes_per_group = sb.inodes_per_group();
        let group_idx = ((ino - 1) / inodes_per_group) as usize;
        let index_in_group = (ino - 1) % inodes_per_group;
        drop(sb);

        let group = self
            .block_groups
            .get(group_idx)
            .ok_or_else(|| Error::with_message(Errno::EIO, "block group index out of range"))?;

        group.write_inode_desc(index_in_group, raw)
    }

    /// Loads the group descriptor table into a segment.
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
        match block_device.read_blocks(Bid::new(sb.group_descriptors_bid(0) as u64), bio_segment)? {
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
    pub(super) fn check_group_desc_table(sb: &SuperBlock, group_descs: &USegment) -> Result<()> {
        let groups_count = sb.block_groups_count() as usize;
        let inode_table_blocks_per_group = sb.inode_table_blocks_per_group();

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
                return_errno_with_message!(Errno::EINVAL, "block bitmap out of group range");
            }
            if inode_bitmap < first_block || inode_bitmap > last_block {
                error!("Ext2: Inode bitmap out of range");
                return_errno_with_message!(Errno::EINVAL, "inode bitmap out of group range");
            }
            let table_last = inode_table + inode_table_blocks_per_group - 1;
            if inode_table < first_block || table_last > last_block {
                error!("Ext2: Inode table out of range");
                return_errno_with_message!(Errno::EINVAL, "inode table out of group range");
            }
        }
        Ok(())
    }

    pub(super) fn load_block_groups(
        sb: &SuperBlock,
        group_descs: &USegment,
        block_device: Arc<dyn BlockDevice>,
    ) -> Result<Vec<BlockGroup>> {
        let groups_count = sb.block_groups_count() as usize;
        let mut groups = Vec::with_capacity(groups_count);
        for idx in 0..groups_count {
            let group = BlockGroup::load(group_descs, idx, sb, block_device.clone())?;
            groups.push(group);
        }
        Ok(groups)
    }

    /// Checks whether the current caller may allocate blocks.
    ///
    /// Non-privileged users are denied when free blocks fall below the reserved
    /// threshold, unless they have `CAP_SYS_RESOURCE` or match `s_resuid`/`s_resgid`.
    ///
    fn has_free_blocks(
        &self,
        free_blocks: u32,
        reserved_blocks: u32,
        resuid: u32,
        resgid: u32,
    ) -> bool {
        if free_blocks >= reserved_blocks + 1 {
            return true;
        }

        // In ktest or kernel-internal contexts there is no thread — treat as root.
        let Some(thread) = Thread::current() else {
            return true;
        };
        let Some(posix_thread) = thread.as_posix_thread() else {
            return true;
        };

        let credentials = posix_thread.credentials();

        // Treat `CAP_SYS_RESOURCE` as bypass permission for reserved blocks.
        if credentials
            .effective_capset()
            .contains(CapSet::SYS_RESOURCE)
        {
            return true;
        }

        // Allow the reserved-block owner to bypass the quota.
        if u32::from(credentials.fsuid()) == resuid {
            return true;
        }

        // Allow the reserved-block group to bypass the quota when configured.
        let resgid_val = Gid::from(resgid);
        if !resgid_val.is_root() {
            if credentials.fsgid() == resgid_val {
                return true;
            }
            if credentials.groups().contains(&resgid_val) {
                return true;
            }
        }

        false
    }

    /// Allocates up to `count` contiguous blocks.
    pub(super) fn alloc_blocks(&self, count: u32, goal: Ext2Bid) -> Result<Range<u32>> {
        if count == 0 {
            return_errno_with_message!(Errno::EINVAL, "zero block allocation requested");
        }

        let (
            groups_count,
            sb_free_blocks,
            first_data_block,
            blocks_per_group,
            reserved_blocks,
            resuid,
            resgid,
        ) = {
            let guard = self.super_block.read();
            (
                guard.block_groups_count() as usize,
                guard.free_blocks_count(),
                guard.first_data_block(),
                guard.blocks_per_group(),
                guard.reserved_blocks_count(),
                guard.default_reserved_uid(),
                guard.default_reserved_gid(),
            )
        };
        if groups_count == 0 || self.block_groups.len() < groups_count {
            return_errno_with_message!(Errno::EIO, "inconsistent block group count");
        }
        if sb_free_blocks == 0 {
            return_errno_with_message!(Errno::ENOSPC, "no free blocks on device");
        }

        if !self.has_free_blocks(sb_free_blocks, reserved_blocks, resuid, resgid) {
            return_errno_with_message!(
                Errno::ENOSPC,
                "no free blocks available for unprivileged user"
            );
        }

        let goal_raw = goal;
        let first_data_raw = first_data_block;
        let goal_group = if goal_raw > first_data_raw {
            ((goal_raw - first_data_raw) / blocks_per_group) as usize
        } else {
            0
        }
        .min(groups_count - 1);

        let mut saw_corruption = false;
        for offset in 0..groups_count {
            let group_idx = (goal_group + offset) % groups_count;
            let group = self
                .block_groups
                .get(group_idx)
                .ok_or_else(|| Error::with_message(Errno::EIO, "block group index out of range"))?;
            if group.free_blocks_count() == 0 {
                continue;
            }

            let (range, corrupt) = group.alloc_blocks(count, sb_free_blocks)?;
            if corrupt {
                saw_corruption = true;
            }
            if let Some(range) = range {
                let alloc_len = range.end - range.start;
                let mut sb_write = self.super_block.write();
                sb_write.dec_free_blocks(alloc_len);
                return Ok(range);
            }
        }

        if saw_corruption {
            return_errno_with_message!(Errno::EIO, "block bitmap corruption detected during alloc");
        }
        return_errno_with_message!(Errno::ENOSPC, "no free blocks available in any group");
    }

    /// Frees a range of blocks starting at `start`.
    pub(super) fn free_blocks(&self, start: u32, count: u32) -> Result<()> {
        if count == 0 {
            return Ok(());
        }

        let sb = self.super_block.read();
        if !sb.is_data_block_valid(start, count) {
            return_errno_with_message!(Errno::EIO, "freeing invalid data block range");
        }
        let blocks_per_group = sb.blocks_per_group();
        let first_data_block = sb.first_data_block();
        drop(sb);

        let mut current = start;
        let mut remaining = count;

        while remaining > 0 {
            let group_idx = ((current - first_data_block) / blocks_per_group) as usize;
            let group = self
                .block_groups
                .get(group_idx)
                .ok_or_else(|| Error::with_message(Errno::EIO, "block group index out of range"))?;

            let group_first = group.first_block();
            let group_last = group.last_block();
            if group_last < group_first {
                return_errno_with_message!(Errno::EIO, "block group has invalid block range");
            }
            let group_size = group_last - group_first + 1;
            let bit = current - group_first;
            if bit >= group_size {
                return_errno_with_message!(Errno::EIO, "block offset outside group boundary");
            }
            let group_count = remaining.min(group_size - bit);

            let freed = group.free_blocks(bit, group_count)?;

            if freed > 0 {
                let mut sb_write = self.super_block.write();
                sb_write.inc_free_blocks(freed);
            }

            current += group_count;
            remaining -= group_count;
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
            return_errno_with_message!(Errno::EIO, "inconsistent block group count");
        }
        if parent_ino < ROOT_INO || parent_ino > total_inodes {
            return_errno_with_message!(Errno::EIO, "parent inode number out of range");
        }
        if free_inodes == 0 {
            return_errno_with_message!(Errno::ENOSPC, "no free inodes on device");
        }

        let parent_group = ((parent_ino - 1) / inodes_per_group) as usize;
        for offset in 0..groups_count {
            let group_idx = (parent_group + offset) % groups_count;
            let group = self
                .block_groups
                .get(group_idx)
                .ok_or_else(|| Error::with_message(Errno::EIO, "block group index out of range"))?;
            if group.free_inodes_count() == 0 {
                continue;
            }

            let Some(inode_idx) = group.alloc_inode()? else {
                continue;
            };

            let ino = (group_idx as u32) * inodes_per_group + inode_idx as u32 + 1;
            if ino < first_ino || ino > total_inodes {
                return_errno_with_message!(Errno::EIO, "allocated inode number out of valid range");
            }

            group.dec_free_inodes(1);
            if inode_type.is_directory() {
                group.inc_used_dirs();
            }
            let mut sb_write = self.super_block.write();
            sb_write.dec_free_inodes();

            return Ok(ino);
        }

        return_errno_with_message!(Errno::ENOSPC, "no free inodes available in any group");
    }

    /// Allocates and initializes a new inode.
    ///
    pub(super) fn create_inode(
        &self,
        parent_ino: u32,
        inode_type: InodeType,
        perm: FilePerm,
    ) -> Result<Arc<Inode>> {
        if inode_type == InodeType::Unknown {
            return_errno_with_message!(Errno::EINVAL, "cannot create inode with unknown type");
        }

        let ino = self.alloc_inode(parent_ino, inode_type)?;
        // SPEC: initialize a valid on-disk inode before publishing it.
        let mode = (inode_type as u16) | (perm.bits() & 0o07777);
        let link_count = if inode_type.is_directory() { 2 } else { 1 };
        let (uid, gid) = if let Some(thread) = Thread::current() {
            if let Some(posix_thread) = thread.as_posix_thread() {
                let credentials = posix_thread.credentials();
                (
                    u32::from(credentials.fsuid()),
                    u32::from(credentials.fsgid()),
                )
            } else {
                // Tests and internal tasks may not have POSIX credentials.
                // Fall back to root ownership in that case.
                (0, 0)
            }
        } else {
            // Tests and internal tasks may not have a thread context.
            // Fall back to root ownership in that case.
            (0, 0)
        };
        let now_secs = now().as_secs() as u32;
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let raw = RawInode {
            mode,
            uid: uid as u16,
            size_lo: 0,
            atime: now_secs,
            ctime: now_secs,
            mtime: now_secs,
            dtime: 0,
            gid: gid as u16,
            link_count,
            sector_count: 0,
            flags: 0,
            osd1: 0,
            block: [0; 15],
            generation,
            file_acl: 0,
            size_high: 0,
            faddr: 0,
            frag: 0,
            fsize: 0,
            pad1: 0,
            uid_high: (uid >> 16) as u16,
            gid_high: (gid >> 16) as u16,
            reserved2: 0,
        };

        if let Err(err) = self.write_inode_desc(ino, &raw) {
            // SPEC: cleanup inode allocation if descriptor initialization failed.
            let _ = self.free_inode(ino, inode_type.is_directory());
            return Err(err);
        }

        let desc = InodeDesc::try_from(&raw)?;
        let block_group_idx = ((ino - 1) / self.inodes_per_group) as usize;
        Ok(Inode::new(
            ino,
            desc.type_(),
            Dirty::new(desc),
            block_group_idx,
            self.self_ref.clone(),
        ))
    }

    /// Frees an inode by number.
    pub(super) fn free_inode(&self, ino: u32, is_dir: bool) -> Result<()> {
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
            return_errno_with_message!(Errno::EIO, "inode number out of valid range for free");
        }
        if groups_count == 0 || self.block_groups.len() < groups_count {
            return_errno_with_message!(Errno::EIO, "inconsistent block group count");
        }

        let group_idx = ((ino - 1) / inodes_per_group) as usize;
        let bit = ((ino - 1) % inodes_per_group) as u16;
        let group = self
            .block_groups
            .get(group_idx)
            .ok_or_else(|| Error::with_message(Errno::EIO, "block group index out of range"))?;

        let was_allocated = group.free_inode(bit)?;

        if was_allocated {
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
    fn sync_metadata(&self) -> Result<()> {
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
            return_errno_with_message!(Errno::EIO, "inconsistent block group count");
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
            return_errno_with_message!(Errno::EIO, "failed to read group descriptor segment");
        }
        let mut sb_guard = self.super_block.write();

        // Recompute free counters from group descriptors — they are the source of truth.
        let mut total_free_blocks: u32 = 0;
        let mut total_free_inodes: u32 = 0;
        for group in &self.block_groups {
            total_free_blocks += group.free_blocks_count() as u32;
            total_free_inodes += group.free_inodes_count() as u32;
        }
        sb_guard.set_free_blocks_count(total_free_blocks);
        sb_guard.set_free_inodes_count(total_free_inodes);

        sb_guard.set_wtime(now());
        if self
            .block_device
            .write_bytes(
                Bid::new(sb_guard.group_descriptors_bid(0) as u64).to_offset(),
                &desc_buf,
            )
            .is_err()
        {
            return_errno_with_message!(Errno::EIO, "failed to write group descriptor table");
        }

        let mut raw_sb = RawSuperBlock::from(&**sb_guard);
        if self
            .block_device
            .write_bytes(SUPER_BLOCK_OFFSET, raw_sb.as_bytes())
            .is_err()
        {
            return_errno_with_message!(Errno::EIO, "failed to write superblock");
        }

        for idx in 1..groups_count {
            if !sb_guard.is_backup_group(idx) {
                continue;
            }
            raw_sb.block_group_idx = idx as u16;
            if self
                .block_device
                .write_bytes(
                    Bid::new(sb_guard.bid(idx) as u64).to_offset(),
                    raw_sb.as_bytes(),
                )
                .is_err()
            {
                return_errno_with_message!(Errno::EIO, "failed to write backup superblock");
            }
            if self
                .block_device
                .write_bytes(
                    Bid::new(sb_guard.group_descriptors_bid(idx) as u64).to_offset(),
                    &desc_buf,
                )
                .is_err()
            {
                return_errno_with_message!(Errno::EIO, "failed to write backup group descriptors");
            }
        }

        sb_guard.clear_dirty();
        Ok(())
    }

    pub(super) fn read_blocks_async(
        &self,
        bid: Ext2Bid,
        bio_segment: BioSegment,
        complete_fn: Option<BioCompleteFn>,
    ) -> Result<BioWaiter> {
        let waiter =
            self.block_device
                .read_blocks_async(Bid::new(bid as u64), bio_segment, complete_fn)?;
        Ok(waiter)
    }

    pub(super) fn read_blocks(&self, bid: Ext2Bid, bio_segment: BioSegment) -> Result<()> {
        let bio_status = self
            .block_device
            .read_blocks(Bid::new(bid as u64), bio_segment)?;
        match bio_status {
            BioStatus::Complete => Ok(()),
            _ => {
                return_errno_with_message!(Errno::EIO, "failed to read blocks from block device")
            }
        }
    }

    pub(super) fn write_blocks_async(
        &self,
        bid: Ext2Bid,
        bio_segment: BioSegment,
        complete_fn: Option<BioCompleteFn>,
    ) -> Result<BioWaiter> {
        let waiter =
            self.block_device
                .write_blocks_async(Bid::new(bid as u64), bio_segment, complete_fn)?;
        Ok(waiter)
    }

    pub(super) fn write_blocks(&self, bid: Ext2Bid, bio_segment: BioSegment) -> Result<()> {
        let bio_status = self
            .block_device
            .write_blocks(Bid::new(bid as u64), bio_segment)?;
        match bio_status {
            BioStatus::Complete => Ok(()),
            _ => {
                return_errno_with_message!(Errno::EIO, "failed to write blocks to block device")
            }
        }
    }

    /// Syncs cached inodes and block-group-local metadata in all groups.
    pub(super) fn sync_all(&self) -> Result<()> {
        for group in &self.block_groups {
            let _ = group.sync_all(&self.group_descriptors_segment)?;
        }

        self.sync_metadata()
    }
}

#[cfg(ktest)]
impl Ext2 {
    /// Returns a write guard of the superblock (test only).
    pub(super) fn super_block_write(&self) -> RwMutexWriteGuard<'_, Dirty<SuperBlock>> {
        self.super_block.write()
    }
}

#[cfg(ktest)]
mod test {

    use aster_block::bio::BioStatus;
    use ostd::{mm::VmIo, prelude::*};

    use super::*;
    use crate::{
        fs::{
            fs_impls::ext2::testkit::{
                self, ErrorBioDisk, Ext2FixtureBuilder, Ext2MemoryDisk, RawInodeBuilder,
                build_group_desc_segment, make_valid_group_desc, make_valid_super_block,
            },
            vfs::file_system::FileSystem as FileSystemTrait,
        },
        time::clocks,
    };

    fn expected_statfs_overhead_blocks(sb: &SuperBlock) -> u32 {
        let groups_count = sb.block_groups_count() as usize;
        let gdb_count =
            ((groups_count * size_of::<RawGroupDesc>()).div_ceil(sb.block_size())) as u32;
        let mut overhead = sb.first_data_block();

        for group_idx in 0..groups_count {
            if group_idx == 0 || sb.is_backup_group(group_idx) {
                overhead = overhead.saturating_add(1 + gdb_count);
            }
        }

        overhead.saturating_add(sb.block_groups_count() * (2 + sb.inode_table_blocks_per_group()))
    }

    fn make_raw_inode(mode: u16, link_count: u16, dtime: u32) -> RawInode {
        RawInodeBuilder::new(mode)
            .link_count(link_count)
            .dtime(dtime)
            .build()
    }

    #[ktest]
    fn statfs_mount_options_parse_minixdf_and_bsddf() {
        let minixdf = CString::new("minixdf").unwrap();
        let bsddf_minixdf = CString::new("bsddf,minixdf").unwrap();
        let minixdf_bsddf = CString::new("minixdf,bsddf").unwrap();
        let ignored_unknown = CString::new("debug,minixdf").unwrap();

        assert!(Ext2MountOptions::parse(Some(minixdf.as_c_str())).uses_minix_df());
        assert!(Ext2MountOptions::parse(Some(bsddf_minixdf.as_c_str())).uses_minix_df());
        assert!(!Ext2MountOptions::parse(Some(minixdf_bsddf.as_c_str())).uses_minix_df());
        assert!(Ext2MountOptions::parse(Some(ignored_unknown.as_c_str())).uses_minix_df());
        assert!(!Ext2MountOptions::parse(None).uses_minix_df());
    }

    #[ktest]
    fn filesystem_statfs_defaults_to_bsddf_overhead() {
        let f = Ext2FixtureBuilder::new(3, 512).build().unwrap();

        let stat = FileSystemTrait::sb(f.ext2.as_ref());
        let expected_overhead = expected_statfs_overhead_blocks(&f.sb);

        assert_eq!(
            stat.blocks,
            f.sb.total_blocks().saturating_sub(expected_overhead) as usize
        );
        assert!(stat.blocks < f.sb.total_blocks() as usize);
    }

    #[ktest]
    fn filesystem_statfs_minixdf_reports_total_blocks() {
        let f = Ext2FixtureBuilder::new(3, 512).build().unwrap();
        let minixdf = CString::new("minixdf").unwrap();

        let ext2 = Ext2::open(
            f.disk.clone() as Arc<dyn BlockDevice>,
            Some(minixdf.as_c_str()),
        )
        .unwrap();

        let stat = FileSystemTrait::sb(ext2.as_ref());
        assert_eq!(stat.blocks, f.sb.total_blocks() as usize);
    }

    #[ktest]
    fn filesystem_statfs_bavail_still_saturates_reserved_blocks() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let mut raw = f
            .disk
            .segment()
            .read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)
            .unwrap();
        raw.free_blocks_count = 3;
        raw.reserved_blocks_count = 5;
        f.disk
            .segment()
            .write_bytes(SUPER_BLOCK_OFFSET, raw.as_bytes())
            .unwrap();

        let ext2 = Ext2::open(f.disk.clone() as Arc<dyn BlockDevice>, None).unwrap();
        let stat = FileSystemTrait::sb(ext2.as_ref());

        assert_eq!(stat.bfree, 3);
        assert_eq!(stat.bavail, 0);
    }

    #[ktest]
    fn filesystem_sync_flushes_once_and_persists_root_updates() {
        clocks::init_for_ktest();
        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();

        root.create(
            "persisted",
            InodeType::File,
            FilePerm::from_bits_truncate(0o644),
        )
        .unwrap();

        FileSystemTrait::sync(f.ext2.as_ref()).unwrap();
        assert_eq!(f.disk.flush_count(), 1);

        let reopened = Ext2::open(f.disk.clone() as Arc<dyn BlockDevice>, None).unwrap();
        let reopened_root = reopened.read_inode(ROOT_INO).unwrap();
        assert_eq!(
            reopened_root.lookup("persisted").unwrap().inode_type(),
            InodeType::File
        );
    }

    #[ktest]
    fn filesystem_sync_propagates_flush_error() {
        clocks::init_for_ktest();
        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();

        root.create(
            "flush_err",
            InodeType::File,
            FilePerm::from_bits_truncate(0o644),
        )
        .unwrap();

        f.disk.set_flush_error(true);
        let err = FileSystemTrait::sync(f.ext2.as_ref()).unwrap_err();
        assert_eq!(err.error(), Errno::EIO);
        assert_eq!(f.disk.flush_count(), 1);
    }

    #[ktest]
    fn sync_metadata_writes_primary_and_backup() {
        clocks::init_for_ktest();
        let fixture = Ext2FixtureBuilder::new(3, 512)
            .with_free_blocks(0, 10)
            .build()
            .unwrap();
        let ext2 = &fixture.ext2;
        let disk = &fixture.disk;
        let sb = &fixture.sb;

        let expected_free_inodes = {
            let mut sb_guard = ext2.super_block_write();
            sb_guard.inc_free_inodes();
            sb_guard.free_inodes_count()
        };

        ext2.block_group(0).inc_free_blocks(3);
        ext2.block_group(0).inc_free_inodes(1);
        assert!(ext2.block_group(0).is_desc_dirty());

        ext2.sync_all().unwrap();

        assert!(!ext2.super_block().is_dirty());
        assert!(!ext2.block_group(0).is_desc_dirty());

        let groups_count = sb.block_groups_count() as usize;
        let desc_bytes = groups_count * size_of::<RawGroupDesc>();
        let primary_desc_offset = Bid::new(sb.group_descriptors_bid(0) as u64).to_offset();

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
                .read_val::<RawSuperBlock>(Bid::new(sb.bid(idx) as u64).to_offset())
                .unwrap();
            assert_eq!(backup_sb.block_group_idx, idx as u16);

            let mut primary_cmp = primary_sb;
            let mut backup_cmp = backup_sb;
            primary_cmp.block_group_idx = 0;
            backup_cmp.block_group_idx = 0;
            assert_eq!(backup_cmp.as_bytes(), primary_cmp.as_bytes());

            let mut backup_desc = vec![0u8; desc_bytes];
            disk.segment()
                .read_bytes(
                    Bid::new(sb.group_descriptors_bid(idx) as u64).to_offset(),
                    &mut backup_desc,
                )
                .unwrap();
            assert_eq!(backup_desc, primary_desc);
        }
    }

    #[ktest]
    fn block_alloc_and_free_single_group_ok() {
        // Happy path: allocate a contiguous run and then free it back.
        let f = Ext2FixtureBuilder::new(1, 128)
            .with_free_blocks(31, 31)
            .with_metadata_block_bitmap()
            .build()
            .unwrap();

        let before_sb_free = f.ext2.super_block().free_blocks_count();
        let before_group_free = f.ext2.block_group(0).free_blocks_count();

        let goal = f.sb.group_first_block_no(0);
        let range = f.ext2.alloc_blocks(8, goal).unwrap();
        let alloc_len = range.end - range.start;
        assert!(alloc_len >= 1 && alloc_len <= 8);

        {
            let sb = f.ext2.super_block();
            assert!(sb.is_data_block_valid(range.start, alloc_len));
            let first_data = sb.first_data_block();
            let start_group = (range.start - first_data) / sb.blocks_per_group();
            let end_group = (range.end - 1 - first_data) / sb.blocks_per_group();
            assert_eq!(start_group, end_group);
        }

        assert_eq!(
            f.ext2.block_group(0).free_blocks_count(),
            before_group_free - alloc_len as u16
        );
        assert_eq!(
            f.ext2.super_block().free_blocks_count(),
            before_sb_free - alloc_len
        );

        f.ext2.free_blocks(range.start, alloc_len).unwrap();
        assert_eq!(f.ext2.block_group(0).free_blocks_count(), before_group_free);
        assert_eq!(f.ext2.super_block().free_blocks_count(), before_sb_free);
    }

    #[ktest]
    fn block_alloc_and_free_invalid_returns_err() {
        // No-space and invalid-request checks.
        let f_nospc = Ext2FixtureBuilder::new(1, 128)
            .with_free_blocks(0, 0)
            .with_metadata_block_bitmap()
            .build()
            .unwrap();
        assert_eq!(
            f_nospc
                .ext2
                .alloc_blocks(1, f_nospc.sb.first_data_block())
                .unwrap_err()
                .error(),
            Errno::ENOSPC
        );
        assert_eq!(
            f_nospc
                .ext2
                .alloc_blocks(0, f_nospc.sb.first_data_block())
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );

        // Inconsistent counters/bitmap shape should surface as EIO on allocation.
        let f_corrupt = Ext2FixtureBuilder::new(1, 128)
            .with_free_blocks(1, 1)
            .with_filled_block_bitmap(true)
            .build()
            .unwrap();
        assert_eq!(
            f_corrupt
                .ext2
                .alloc_blocks(1, f_corrupt.sb.first_data_block())
                .unwrap_err()
                .error(),
            Errno::EIO
        );

        // Free-path boundary and system-zone guards.
        let f_free = Ext2FixtureBuilder::new(1, 128)
            .with_free_blocks(31, 31)
            .with_metadata_block_bitmap()
            .build()
            .unwrap();
        assert!(f_free.ext2.free_blocks(10, 0).is_ok());
        assert_eq!(
            f_free.ext2.free_blocks(1, 1).unwrap_err().error(),
            Errno::EIO
        );

        let inode_bitmap_bid = f_free.ext2.block_group(0).inode_bitmap_bid();
        assert_eq!(
            f_free
                .ext2
                .free_blocks(inode_bitmap_bid, 1)
                .unwrap_err()
                .error(),
            Errno::EIO
        );
    }

    #[ktest]
    fn block_alloc_starts_from_goal_group() {
        clocks::init_for_ktest();
        // With 2 groups both having free blocks, allocation should start from
        // the group containing goal.
        let f = Ext2FixtureBuilder::new(2, 256)
            .with_free_blocks(64, 32)
            .with_metadata_block_bitmap()
            .build()
            .unwrap();

        // Fixture builder only customizes group 0 free-block counter.
        // Make group 1 allocatable as well so goal-based start is observable.
        f.ext2.block_group(1).inc_free_blocks(16);
        {
            let mut sb = f.ext2.super_block.write();
            sb.inc_free_blocks(16);
        }

        let goal = f.sb.group_first_block_no(1) + 16;
        let range = f.ext2.alloc_blocks(4, goal).unwrap();

        let start_group = (range.start - f.sb.first_data_block()) / f.sb.blocks_per_group();
        assert_eq!(start_group, 1);
    }

    #[ktest]
    fn inode_alloc_and_free_single_group_ok() {
        // Allocate one directory inode and verify bitmap/counter transitions.
        let f = Ext2FixtureBuilder::new(1, 128)
            .with_free_inodes(16, 16)
            .with_reserved_inode_bitmap()
            .build()
            .unwrap();

        let before_sb_free = f.ext2.super_block().free_inodes_count();
        let before_group_free = f.ext2.block_group(0).free_inodes_count();
        let before_used_dirs = f.ext2.block_group(0).used_dirs_count();

        let ino = f.ext2.alloc_inode(ROOT_INO, InodeType::Dir).unwrap();
        assert!(ino >= f.sb.first_ino() && ino <= f.sb.total_inodes());

        let bit = ((ino - 1) % f.sb.inodes_per_group()) as u16;
        let bitmap = f.ext2.block_group(0).inode_bitmap();
        assert!(bitmap.is_allocated(bit));
        drop(bitmap);
        assert_eq!(f.ext2.super_block().free_inodes_count(), before_sb_free - 1);
        assert_eq!(
            f.ext2.block_group(0).free_inodes_count(),
            before_group_free - 1
        );
        assert_eq!(
            f.ext2.block_group(0).used_dirs_count(),
            before_used_dirs + 1
        );

        // Free path now takes caller-provided inode type; no inode-table read is needed.
        let raw_dir = make_raw_inode(0o040755, 1, 0);
        f.ext2.write_inode_desc(ino, &raw_dir).unwrap();
        f.ext2.free_inode(ino, true).unwrap();
        assert_eq!(f.ext2.super_block().free_inodes_count(), before_sb_free);
        assert_eq!(f.ext2.block_group(0).free_inodes_count(), before_group_free);
        assert_eq!(f.ext2.block_group(0).used_dirs_count(), before_used_dirs);
    }

    #[ktest]
    fn inode_alloc_and_free_invalid_returns_err() {
        // No free inode counter means ENOSPC without bitmap scan.
        let f_nospc = Ext2FixtureBuilder::new(1, 128)
            .with_free_inodes(0, 0)
            .with_reserved_inode_bitmap()
            .build()
            .unwrap();
        assert_eq!(
            f_nospc
                .ext2
                .alloc_inode(ROOT_INO, InodeType::File)
                .unwrap_err()
                .error(),
            Errno::ENOSPC
        );
        assert_eq!(
            f_nospc
                .ext2
                .alloc_inode(f_nospc.sb.total_inodes() + 1, InodeType::File)
                .unwrap_err()
                .error(),
            Errno::EIO
        );

        // All inode bitmap bits set -> no allocatable inode.
        let f_full = Ext2FixtureBuilder::new(1, 128)
            .with_free_inodes(8, 8)
            .with_filled_inode_bitmap(true)
            .build()
            .unwrap();
        assert_eq!(
            f_full
                .ext2
                .alloc_inode(ROOT_INO, InodeType::File)
                .unwrap_err()
                .error(),
            Errno::ENOSPC
        );

        let f_free = Ext2FixtureBuilder::new(1, 128)
            .with_free_inodes(8, 8)
            .with_reserved_inode_bitmap()
            .build()
            .unwrap();
        assert_eq!(
            f_free
                .ext2
                .free_inode(f_free.sb.first_ino() - 1, false)
                .unwrap_err()
                .error(),
            Errno::EIO
        );

        // Already-free inode: should return Ok and keep counters unchanged.
        let target_ino = f_free.sb.first_ino();
        let raw_file = make_raw_inode(0o100644, 1, 0);
        f_free.ext2.write_inode_desc(target_ino, &raw_file).unwrap();

        let before_sb = f_free.ext2.super_block().free_inodes_count();
        let before_group = f_free.ext2.block_group(0).free_inodes_count();
        f_free.ext2.free_inode(target_ino, false).unwrap();
        assert_eq!(f_free.ext2.super_block().free_inodes_count(), before_sb);
        assert_eq!(f_free.ext2.block_group(0).free_inodes_count(), before_group);
    }

    #[ktest]
    fn alloc_inode_initializes_descriptor_on_disk() {
        let fixture = Ext2FixtureBuilder::new(1, 128)
            .with_free_inodes(16, 16)
            .with_reserved_inode_bitmap()
            .build()
            .unwrap();

        let inode = fixture
            .ext2
            .create_inode(
                ROOT_INO,
                InodeType::Dir,
                FilePerm::from_bits_truncate(0o755),
            )
            .unwrap();
        let ino = inode.ino();

        // Read through PageCache (write_inode_desc uses deferred writeback).
        let desc = fixture.ext2.read_inode_desc(ino).unwrap();
        let raw = RawInode::from(&desc);
        assert_eq!(raw.mode, 0o040755);
        assert_eq!(raw.link_count, 2);
        assert_eq!(raw.size_lo, 0);
        assert_eq!(raw.sector_count, 0);
        assert_eq!(raw.block, [0; 15]);

        assert_eq!(
            fixture
                .ext2
                .create_inode(
                    ROOT_INO,
                    InodeType::Unknown,
                    FilePerm::from_bits_truncate(0o644)
                )
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
    }

    #[ktest]
    fn group_bounds_first_last_block_ok() {
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
    fn check_group_desc_bad_bitmap_returns_einval() {
        let sb = make_valid_super_block(2);
        let mut descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();

        descs[0].block_bitmap = sb.group_first_block_no(0).saturating_sub(1);

        let group_descs = build_group_desc_segment(&sb, &descs);
        let err = Ext2::check_group_desc_table(&sb, &group_descs).unwrap_err();
        assert_eq!(err.error(), Errno::EINVAL);
    }

    #[ktest]
    fn check_group_desc_bad_inode_table_returns_einval() {
        let sb = make_valid_super_block(2);
        let mut descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();

        let first = sb.group_first_block_no(0);
        let last = sb.group_last_block_no(0);
        let itb = sb.inode_table_blocks_per_group();
        descs[0].inode_table = last.saturating_sub(itb.saturating_sub(2));
        assert!(descs[0].inode_table >= first);

        let group_descs = build_group_desc_segment(&sb, &descs);
        let err = Ext2::check_group_desc_table(&sb, &group_descs).unwrap_err();
        assert_eq!(err.error(), Errno::EINVAL);
    }

    #[ktest]
    fn load_group_descs_valid_image_ok() {
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
    fn load_group_descs_io_error_returns_eio() {
        let sb = make_valid_super_block(1);
        let disk = ErrorBioDisk::new(BioStatus::IoError, 64 * BLOCK_SIZE / SECTOR_SIZE);

        let err = Ext2::load_group_desc_table(&disk, &sb).unwrap_err();
        assert_eq!(err.error(), Errno::EIO);
    }

    #[ktest]
    fn load_group_descs_invalid_desc_returns_einval() {
        let sb = make_valid_super_block(2);
        let mut descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();
        descs[1].inode_bitmap = sb.group_last_block_no(1).saturating_add(1);

        let disk = Ext2MemoryDisk::new(64);
        disk.write_group_desc_table(&sb, &descs);

        let err = Ext2::load_group_desc_table(&disk, &sb).unwrap_err();
        assert_eq!(err.error(), Errno::EINVAL);
    }

    #[ktest]
    fn read_inode_desc_valid_ino_ok() {
        let f = Ext2FixtureBuilder::new(2, 128).build().unwrap();

        let raw = make_raw_inode(0o040755, 2, 0);
        f.ext2.write_inode_desc(ROOT_INO, &raw).unwrap();

        let bid = f.ext2.inode_table_block(1, 3).unwrap();
        let base = f.descs[1].inode_table;
        assert_eq!(bid, base + 3);

        let desc = f.ext2.read_inode_desc(ROOT_INO).unwrap();
        assert_eq!(desc.type_(), InodeType::Dir);
    }

    #[ktest]
    fn read_inode_desc_deleted_ino_returns_err() {
        // Out-of-range group index.
        let f = Ext2FixtureBuilder::new(2, 128).build().unwrap();
        let group_err = f.ext2.inode_table_block(2, 0).unwrap_err();
        assert_eq!(group_err.error(), Errno::EIO);

        // Invalid inode numbers (too low / too high).
        let invalid_low = f.ext2.read_inode_desc(1).unwrap_err();
        assert_eq!(invalid_low.error(), Errno::EINVAL);
        let invalid_high = f
            .ext2
            .read_inode_desc(f.sb.total_inodes().saturating_add(1))
            .unwrap_err();
        assert_eq!(invalid_high.error(), Errno::EINVAL);

        // I/O error from block device: fail group-1 inode-table reads so mount
        // (which reads ROOT_INO from group 0) still succeeds.
        let f_base = Ext2FixtureBuilder::new(2, 128).build().unwrap();
        let inode_table_offset = Bid::new(f_base.descs[1].inode_table as u64).to_offset();
        let io_disk = ErrorBioDisk::with_read_error_at(
            f_base.disk.clone(),
            BioStatus::IoError,
            inode_table_offset,
        );
        let f_io = Ext2FixtureBuilder::new(2, 128)
            .with_device(Arc::new(io_disk))
            .build()
            .unwrap();
        let group1_ino = f_io.sb.inodes_per_group().saturating_add(1);
        let io_err = f_io.ext2.read_inode_desc(group1_ino).unwrap_err();
        assert_eq!(io_err.error(), Errno::EIO);

        // Deleted inode (dtime != 0, mode == 0) → ESTALE.
        let f_parse = Ext2FixtureBuilder::new(2, 128).build().unwrap();
        let ino = f_parse.sb.first_ino();
        let raw = make_raw_inode(0, 0, 1);
        f_parse.ext2.write_inode_desc(ino, &raw).unwrap();
        let parse_err = f_parse.ext2.read_inode_desc(ino).unwrap_err();
        assert_eq!(parse_err.error(), Errno::ESTALE);
    }

    #[ktest]
    fn read_inode_cache_hit_and_unallocated_checks() {
        let f = Ext2FixtureBuilder::namei_env().build().unwrap();

        let first = f.ext2.read_inode(ROOT_INO).unwrap();
        let second = f.ext2.read_inode(ROOT_INO).unwrap();
        assert!(Arc::ptr_eq(&first, &second));

        let unallocated_ino = f.sb.first_ino();
        let err = f.ext2.read_inode(unallocated_ino).unwrap_err();
        assert_eq!(err.error(), Errno::ENOENT);
    }

    #[ktest]
    fn create_inserts_inode_cache_and_sync_eviction_keeps_linked_inode() {
        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();

        let child = root
            .create(
                "cache_file",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        let child_ino = child.ino();

        let cached = f.ext2.read_inode(child_ino).unwrap();
        assert!(Arc::ptr_eq(&child, &cached));

        drop(cached);
        drop(child);
        f.ext2.sync_all().unwrap();

        let inode_bitmap = f.ext2.block_group(0).inode_bitmap();
        assert!(inode_bitmap.is_allocated((child_ino - 1) as u16));
        drop(inode_bitmap);

        let reloaded = f.ext2.read_inode(child_ino).unwrap();
        assert_eq!(reloaded.ino(), child_ino);
    }

    #[ktest]
    fn load_block_bitmap_valid_image_ok() {
        let f = Ext2FixtureBuilder::new(2, 128)
            .with_metadata_block_bitmap()
            .build()
            .unwrap();
        let group = f.ext2.block_group(0);
        let first = f.sb.group_first_block_no(0);

        let bitmap = group.block_bitmap();
        // Block bitmap, inode bitmap, and inode table blocks must be marked.
        let bb = (f.descs[0].block_bitmap - first) as u16;
        let ib = (f.descs[0].inode_bitmap - first) as u16;
        let it = (f.descs[0].inode_table - first) as u16;
        assert!(bitmap.is_allocated(bb));
        assert!(bitmap.is_allocated(ib));
        assert!(bitmap.is_allocated(it));
    }

    #[ktest]
    fn load_block_bitmap_missing_itable_bits_returns_err() {
        let f = Ext2FixtureBuilder::new(2, 128).build().unwrap();
        let group = f.ext2.block_group(0);
        let first = f.sb.group_first_block_no(0);

        // Write a bitmap with only block_bitmap and inode_bitmap bits set,
        // deliberately missing inode table bits.
        let mut bitmap_block = [0u8; BLOCK_SIZE];
        testkit::set_bit_lsb0(
            &mut bitmap_block,
            (f.descs[0].block_bitmap - first) as usize,
        );
        testkit::set_bit_lsb0(
            &mut bitmap_block,
            (f.descs[0].inode_bitmap - first) as usize,
        );
        f.disk
            .segment()
            .write_bytes(
                Bid::new(group.block_bitmap_bid() as u64).to_offset(),
                &bitmap_block,
            )
            .unwrap();

        // Cached bitmap is loaded during mount; direct disk mutation should not affect cache.
        let bitmap = group.block_bitmap();
        let bb = (f.descs[0].block_bitmap - first) as u16;
        let ib = (f.descs[0].inode_bitmap - first) as u16;
        let it = (f.descs[0].inode_table - first) as u16;
        assert!(bitmap.is_allocated(bb));
        assert!(bitmap.is_allocated(ib));
        assert!(bitmap.is_allocated(it));
    }

    #[ktest]
    fn load_inode_bitmap_valid_image_ok() {
        let f = Ext2FixtureBuilder::new(2, 128)
            .with_reserved_inode_bitmap()
            .build()
            .unwrap();
        let group = f.ext2.block_group(0);

        // Mutate on-disk bitmap after mount. Cache must remain unchanged.
        let mut bitmap_block = [0u8; BLOCK_SIZE];
        testkit::set_bit_lsb0(&mut bitmap_block, 31);
        f.disk
            .segment()
            .write_bytes(
                Bid::new(group.inode_bitmap_bid() as u64).to_offset(),
                &bitmap_block,
            )
            .unwrap();

        let bitmap = group.inode_bitmap();
        assert_eq!(bitmap.len(), f.sb.inodes_per_group() as u16);
        assert!(bitmap.is_allocated(0));
        assert!(bitmap.is_allocated(1));
        assert!(!bitmap.is_allocated(31));
    }

    #[ktest]
    fn load_block_groups_bad_desc_table_returns_err() {
        let sb = make_valid_super_block(200);
        let segment = FrameAllocOptions::new()
            .zeroed(true)
            .alloc_segment(1)
            .unwrap();
        let group_descs: USegment = segment.into();

        let err = Ext2::load_block_groups(&sb, &group_descs, Arc::new(Ext2MemoryDisk::new(64)))
            .unwrap_err();
        assert_eq!(err.error(), Errno::EINVAL);
    }
}
