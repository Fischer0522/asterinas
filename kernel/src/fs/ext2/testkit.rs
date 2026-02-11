// SPDX-License-Identifier: MPL-2.0

#![cfg(ktest)]

use alloc::{sync::Arc, vec::Vec};

use aster_block::{BLOCK_SIZE, BlockDevice, SECTOR_SIZE, id::Bid};
use ostd::mm::VmIo;

use super::{
    SuperBlock,
    block_group::RawGroupDesc,
    fs::{Ext2, ROOT_INO},
    inode::RawInode,
    test::{Ext2MemoryDisk, make_valid_group_desc, make_valid_raw_super_block},
};
use crate::{
    fs::utils::InodeType,
            prelude::*,
    prelude::{Errno, Error, Result, return_errno_with_message},
};

#[derive(Clone, Copy, Debug)]
pub(super) struct Group0Layout {
    pub first: u32,
    pub group_desc_bid: u32,
    pub block_bitmap: u32,
    pub inode_bitmap: u32,
    pub inode_table: u32,
    pub first_data: u32,
}

pub(super) fn group0_layout(sb: &SuperBlock) -> Group0Layout {
    let first = sb.group_first_block_no(0);
    let group_desc_bid = sb.group_descriptors_bid(0).to_raw() as u32;

    // Reserve 1 block for block bitmap, 1 for inode bitmap, then inode table.
    // Keep them away from group descriptor table block.
    let mut next = first;
    if next == group_desc_bid {
        next = next.saturating_add(1);
    }
    let block_bitmap = next;
    next = next.saturating_add(1);
    if next == group_desc_bid {
        next = next.saturating_add(1);
    }
    let inode_bitmap = next;
    next = next.saturating_add(1);
    if next == group_desc_bid {
        next = next.saturating_add(1);
    }
    let inode_table = next;
    let first_data = inode_table.saturating_add(sb.itb_per_group());

    Group0Layout {
        first,
        group_desc_bid,
        block_bitmap,
        inode_bitmap,
        inode_table,
        first_data,
    }
}

pub(super) fn validate_group0_layout(sb: &SuperBlock, layout: &Group0Layout) -> Result<()> {
    let first = sb.group_first_block_no(0);
    let last = sb.group_last_block_no(0);

    let in_range = |block: u32| block >= first && block <= last;
    if !in_range(layout.group_desc_bid)
        || !in_range(layout.block_bitmap)
        || !in_range(layout.inode_bitmap)
        || !in_range(layout.inode_table)
    {
        return_errno_with_message!(Errno::EINVAL, "test layout block out of group range");
    }

    let inode_table_last = layout
        .inode_table
        .saturating_add(sb.itb_per_group())
        .saturating_sub(1);
    if inode_table_last > last {
        return_errno_with_message!(Errno::EINVAL, "test layout inode table out of range");
    }

    if layout.block_bitmap == layout.inode_bitmap
        || layout.block_bitmap == layout.group_desc_bid
        || layout.inode_bitmap == layout.group_desc_bid
    {
        return_errno_with_message!(Errno::EINVAL, "test layout metadata blocks overlap");
    }

    if layout.block_bitmap >= layout.inode_table && layout.block_bitmap <= inode_table_last {
        return_errno_with_message!(Errno::EINVAL, "test layout block bitmap overlaps inode table");
    }
    if layout.inode_bitmap >= layout.inode_table && layout.inode_bitmap <= inode_table_last {
        return_errno_with_message!(Errno::EINVAL, "test layout inode bitmap overlaps inode table");
    }
    if layout.group_desc_bid >= layout.inode_table && layout.group_desc_bid <= inode_table_last {
        return_errno_with_message!(Errno::EINVAL, "test layout group desc overlaps inode table");
    }

    if layout.first_data <= first {
        return_errno_with_message!(Errno::EINVAL, "test layout first_data invalid");
    }

    Ok(())
}

pub(super) fn set_bit_lsb0(buf: &mut [u8], bit: usize) {
    let byte = bit / 8;
    let bit_in_byte = bit % 8;
    buf[byte] |= 1u8 << bit_in_byte;
}

pub(super) fn bit_is_set_lsb0(buf: &[u8], bit: usize) -> bool {
    let byte = bit / 8;
    let bit_in_byte = bit % 8;
    (buf[byte] & (1u8 << bit_in_byte)) != 0
}

pub(super) fn write_block_bitmap(
    disk: &Ext2MemoryDisk,
    sb: &SuperBlock,
    desc: &RawGroupDesc,
    allocated_blocks: &[u32],
) {
    let first = sb.group_first_block_no(0);
    let last = sb.group_last_block_no(0);

    let mut bitmap_block = [0u8; BLOCK_SIZE];
    let mut mark_block = |block: u32| {
        if block < first || block > last {
            return;
        }
        set_bit_lsb0(&mut bitmap_block, (block - first) as usize);
    };

    // Required metadata/system blocks.
    mark_block(sb.group_descriptors_bid(0).to_raw() as u32);
    mark_block(desc.block_bitmap);
    mark_block(desc.inode_bitmap);
    for block in desc.inode_table..desc.inode_table.saturating_add(sb.itb_per_group()) {
        mark_block(block);
    }

    for &block in allocated_blocks {
        mark_block(block);
    }

    disk.segment()
        .write_bytes(Bid::new(desc.block_bitmap as u64).to_offset(), &bitmap_block)
        .unwrap();
}

pub(super) fn write_inode_bitmap(
    disk: &Ext2MemoryDisk,
    sb: &SuperBlock,
    desc: &RawGroupDesc,
    allocated_inodes: &[u32],
) {
    let mut bitmap = [0u8; BLOCK_SIZE];

    // Reserved inodes [1, first_ino) are always allocated.
    for bit in 0..(sb.first_ino() as usize).saturating_sub(1) {
        set_bit_lsb0(&mut bitmap, bit);
    }

    for &ino in allocated_inodes {
        if ino == 0 || ino > sb.inodes_per_group() {
            continue;
        }
        set_bit_lsb0(&mut bitmap, (ino - 1) as usize);
    }

    disk.segment()
        .write_bytes(Bid::new(desc.inode_bitmap as u64).to_offset(), &bitmap)
        .unwrap();
}

fn make_root_raw_inode(root_bid: u32, block_size: usize) -> RawInode {
    RawInode {
        mode: InodeType::Dir as u16 | 0o755,
        uid: 0,
        size_lo: block_size as u32,
        atime: 0,
        ctime: 0,
        mtime: 0,
        dtime: 0,
        gid: 0,
        links_count: 2,
        blocks: (block_size / SECTOR_SIZE) as u32,
        flags: 0,
        osd1: 0,
        block: {
            let mut ptrs = [0u32; 15];
            ptrs[0] = root_bid;
            ptrs
        },
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

pub(super) fn write_simple_root_dir_block(disk: &Ext2MemoryDisk, root_bid: u32, block_size: usize) {
    let mut block = vec![0u8; block_size];

    // '.'
    block[0..4].copy_from_slice(&ROOT_INO.to_le_bytes());
    block[4..6].copy_from_slice(&(12u16).to_le_bytes());
    block[6] = 1;
    block[7] = 2;
    block[8] = b'.';

    // '..'
    block[12..16].copy_from_slice(&ROOT_INO.to_le_bytes());
    block[16..18].copy_from_slice(&((block_size - 12) as u16).to_le_bytes());
    block[18] = 2;
    block[19] = 2;
    block[20] = b'.';
    block[21] = b'.';

    disk.segment()
        .write_bytes(Bid::new(root_bid as u64).to_offset(), &block)
        .unwrap();
}

pub(super) struct Ext2Fixture {
    pub disk: Arc<Ext2MemoryDisk>,
    pub ext2: Arc<Ext2>,
    pub sb: SuperBlock,
    pub descs: Vec<RawGroupDesc>,
    pub root_bid: u32,
}

impl Ext2Fixture {
    pub(super) fn root(&self) -> Result<Arc<super::inode::Inode>> {
        self.ext2.read_inode(ROOT_INO)
    }

    pub(super) fn read_inode_bitmap(&self, group_idx: usize) -> Result<[u8; BLOCK_SIZE]> {
        let desc = self
            .descs
            .get(group_idx)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let mut inode_bitmap = [0u8; BLOCK_SIZE];
        self.disk
            .segment()
            .read_bytes(
                Bid::new(desc.inode_bitmap as u64).to_offset(),
                &mut inode_bitmap,
            )
            .map_err(|_| Error::new(Errno::EIO))?;
        Ok(inode_bitmap)
    }
}

pub(super) struct Ext2FixtureBuilder {
    groups: u32,
    nblocks: usize,
    sb_free_blocks: Option<u32>,
    sb_free_inodes: Option<u32>,
    group0_free_blocks: Option<u16>,
    group0_free_inodes: Option<u16>,
    group0_used_dirs: Option<u16>,
    init_root: bool,
}

impl Ext2FixtureBuilder {
    /// Create a deterministic ext2 ktest fixture builder.
    ///
    /// The builder enforces a non-overlapping group-0 metadata layout and can
    /// optionally initialize a valid root directory (`.` and `..`).
    /// Prefer this builder over ad-hoc per-test disk setup helpers.
    pub(super) fn new(groups: u32, nblocks: usize) -> Self {
        Self {
            groups,
            nblocks,
            sb_free_blocks: None,
            sb_free_inodes: None,
            group0_free_blocks: None,
            group0_free_inodes: None,
            group0_used_dirs: None,
            init_root: false,
        }
    }

    pub(super) fn with_free_blocks(mut self, sb_free_blocks: u32, group0_free_blocks: u16) -> Self {
        self.sb_free_blocks = Some(sb_free_blocks);
        self.group0_free_blocks = Some(group0_free_blocks);
        self
    }

    pub(super) fn with_free_inodes(mut self, sb_free_inodes: u32, group0_free_inodes: u16) -> Self {
        self.sb_free_inodes = Some(sb_free_inodes);
        self.group0_free_inodes = Some(group0_free_inodes);
        self
    }

    pub(super) fn with_group0_used_dirs(mut self, used_dirs: u16) -> Self {
        self.group0_used_dirs = Some(used_dirs);
        self
    }

    pub(super) fn with_root(mut self) -> Self {
        self.init_root = true;
        self
    }

    pub(super) fn build(self) -> Result<Ext2Fixture> {
        let mut raw_sb = make_valid_raw_super_block(self.groups);
        if let Some(sb_free_blocks) = self.sb_free_blocks {
            raw_sb.free_blocks_count = sb_free_blocks;
        }
        if let Some(sb_free_inodes) = self.sb_free_inodes {
            raw_sb.free_inodes_count = sb_free_inodes;
        }

        let sb = SuperBlock::try_from(raw_sb)?;
        let mut descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();

        // Stabilize group 0 layout so metadata never collides with group desc table.
        let layout = group0_layout(&sb);
        validate_group0_layout(&sb, &layout)?;
        descs[0].block_bitmap = layout.block_bitmap;
        descs[0].inode_bitmap = layout.inode_bitmap;
        descs[0].inode_table = layout.inode_table;

        if let Some(group0_free_blocks) = self.group0_free_blocks {
            descs[0].free_blocks_count = group0_free_blocks;
        }
        if let Some(group0_free_inodes) = self.group0_free_inodes {
            descs[0].free_inodes_count = group0_free_inodes;
        }
        if let Some(group0_used_dirs) = self.group0_used_dirs {
            descs[0].used_dirs_count = group0_used_dirs;
        }

        let disk = Arc::new(Ext2MemoryDisk::new(self.nblocks));
        disk.write_super_block(&raw_sb);
        disk.write_group_desc_table(&sb, &descs);

        let root_bid = layout.first_data.saturating_add(1);
        if self.init_root {
            write_block_bitmap(disk.as_ref(), &sb, &descs[0], &[root_bid]);
            write_inode_bitmap(disk.as_ref(), &sb, &descs[0], &[ROOT_INO]);
            write_simple_root_dir_block(disk.as_ref(), root_bid, sb.block_size());
        }

        let ext2 = Ext2::open(disk.clone() as Arc<dyn BlockDevice>)?;

        if self.init_root {
            let root_raw = make_root_raw_inode(root_bid, sb.block_size());
            ext2.write_inode_desc(ROOT_INO, &root_raw)?;
        }

        Ok(Ext2Fixture {
            disk,
            ext2,
            sb,
            descs,
            root_bid,
        })
    }
}
