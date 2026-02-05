// SPDX-License-Identifier: MPL-2.0

use core::mem::size_of;

use ostd::const_assert;

use super::prelude::*;
use super::fs::Ext2;
use super::super_block::SuperBlock;
use crate::fs::utils::IdBitmap;

#[derive(Debug)]
pub struct BlockGroup {
    idx: usize,
    desc: RwMutex<Dirty<GroupDesc>>,
}

/// On-disk block group descriptor (32 bytes).
///
/// Linux: /root/linux/fs/ext2/ext2.h:191 (struct ext2_group_desc)
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct RawGroupDesc {
    pub block_bitmap: u32,      // bg_block_bitmap
    pub inode_bitmap: u32,      // bg_inode_bitmap
    pub inode_table: u32,       // bg_inode_table
    pub free_blocks_count: u16, // bg_free_blocks_count
    pub free_inodes_count: u16, // bg_free_inodes_count
    pub used_dirs_count: u16,   // bg_used_dirs_count
    pub pad: u16,               // bg_pad
    pub reserved: [u32; 3],     // bg_reserved
}

const_assert!(size_of::<RawGroupDesc>() == 32);

/// In-memory block group descriptor.
#[derive(Clone, Copy, Debug)]
pub(super) struct GroupDesc {
    pub block_bitmap: Bid,
    pub inode_bitmap: Bid,
    pub inode_table: Bid,
    pub free_blocks_count: u16,
    pub free_inodes_count: u16,
    pub used_dirs_count: u16,
}

impl From<RawGroupDesc> for GroupDesc {
    fn from(raw: RawGroupDesc) -> Self {
        Self {
            block_bitmap: Bid::new(raw.block_bitmap as u64),
            inode_bitmap: Bid::new(raw.inode_bitmap as u64),
            inode_table: Bid::new(raw.inode_table as u64),
            free_blocks_count: raw.free_blocks_count,
            free_inodes_count: raw.free_inodes_count,
            used_dirs_count: raw.used_dirs_count,
        }
    }
}

impl BlockGroup {
    pub fn load(group_descs: &USegment, idx: usize) -> Result<Self> {
        let offset = idx * size_of::<RawGroupDesc>();
        let raw = group_descs.read_val::<RawGroupDesc>(offset)?;
        let desc = GroupDesc::from(raw);
        Ok(Self {
            idx,
            desc: RwMutex::new(Dirty::new(desc)),
        })
    }

    pub fn idx(&self) -> usize {
        self.idx
    }

    pub fn block_bitmap_bid(&self) -> Bid {
        self.desc.read().block_bitmap
    }

    pub fn inode_bitmap_bid(&self) -> Bid {
        self.desc.read().inode_bitmap
    }

    pub fn inode_table_bid(&self) -> Bid {
        self.desc.read().inode_table
    }

    pub fn free_blocks_count(&self) -> u16 {
        self.desc.read().free_blocks_count
    }

    pub fn free_inodes_count(&self) -> u16 {
        self.desc.read().free_inodes_count
    }

    pub fn used_dirs_count(&self) -> u16 {
        self.desc.read().used_dirs_count
    }

    /// Decreases the free-block counter for this group.
    pub(super) fn dec_free_blocks(&self, count: u16) {
        let mut desc = self.desc.write();
        desc.free_blocks_count = desc.free_blocks_count.saturating_sub(count);
    }

    /// Increases the free-block counter for this group.
    pub(super) fn inc_free_blocks(&self, count: u16) {
        let mut desc = self.desc.write();
        desc.free_blocks_count = desc.free_blocks_count.saturating_add(count);
    }

    /// Loads and validates the block bitmap for this group.
    ///
    /// Linux: /root/linux/fs/ext2/balloc.c:129 (read_block_bitmap)
    pub fn load_block_bitmap(&self, fs: &Ext2, sb: &SuperBlock) -> Result<IdBitmap> {
        let desc = self.desc.read();
        let bitmap_bid = desc.block_bitmap;

        let mut buf = vec![0u8; BLOCK_SIZE];
        if fs
            .block_device()
            .read_bytes(bitmap_bid.to_offset(), &mut buf)
            .is_err()
        {
            return_errno!(Errno::EIO);
        }

        let first_block = sb.group_first_block_no(self.idx());
        let last_block = sb.group_last_block_no(self.idx());
        if last_block < first_block {
            return_errno!(Errno::EINVAL);
        }
        let max_bit = last_block - first_block;
        let capacity = max_bit.saturating_add(1) as usize;
        if capacity > IdBitmap::capacity() as usize {
            return_errno!(Errno::EINVAL);
        }
        let itb_per_group = sb.itb_per_group();
        let bitmap = IdBitmap::from_buf(buf.into_boxed_slice(), capacity as u16);

        let valid_block_bitmap =
            |first_block: u32, max_bit: u32, desc: &GroupDesc, bitmap: &IdBitmap| -> Result<()> {
                let block_bitmap = desc.block_bitmap.to_raw() as u32;
                let inode_bitmap = desc.inode_bitmap.to_raw() as u32;
                let inode_table = desc.inode_table.to_raw() as u32;

                let mut offset = block_bitmap.wrapping_sub(first_block);
                if block_bitmap < first_block || offset > max_bit {
                    return_errno!(Errno::EINVAL);
                }
                if !bitmap.is_allocated(offset as u16) {
                    return_errno!(Errno::EINVAL);
                }

                offset = inode_bitmap.wrapping_sub(first_block);
                if inode_bitmap < first_block || offset > max_bit {
                    return_errno!(Errno::EINVAL);
                }
                if !bitmap.is_allocated(offset as u16) {
                    return_errno!(Errno::EINVAL);
                }

                offset = inode_table.wrapping_sub(first_block);
                if inode_table < first_block || offset > max_bit {
                    return_errno!(Errno::EINVAL);
                }
                let table_last = offset.saturating_add(itb_per_group.saturating_sub(1));
                if table_last > max_bit {
                    return_errno!(Errno::EINVAL);
                }

                let end = offset.saturating_add(itb_per_group);
                let mut bit = offset;
                while bit < end {
                    if !bitmap.is_allocated(bit as u16) {
                        return_errno!(Errno::EINVAL);
                    }
                    bit += 1;
                }
                Ok(())
            };

        valid_block_bitmap(first_block, max_bit, &desc, &bitmap)?;

        Ok(bitmap)
    }

    /// Loads the inode bitmap for this group.
    ///
    /// Linux: /root/linux/fs/ext2/ialloc.c:31 (read_inode_bitmap)
    pub fn load_inode_bitmap(&self, fs: &Ext2, sb: &SuperBlock) -> Result<IdBitmap> {
        let desc = self.desc.read();
        let bitmap_bid = desc.inode_bitmap;

        let mut buf = vec![0u8; BLOCK_SIZE];
        if fs
            .block_device()
            .read_bytes(bitmap_bid.to_offset(), &mut buf)
            .is_err()
        {
            return_errno!(Errno::EIO);
        }

        let capacity = sb.inodes_per_group() as usize;
        if capacity > IdBitmap::capacity() as usize {
            return_errno!(Errno::EINVAL);
        }

        Ok(IdBitmap::from_buf(buf.into_boxed_slice(), capacity as u16))
    }
}
