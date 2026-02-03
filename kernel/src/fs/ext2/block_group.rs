// SPDX-License-Identifier: MPL-2.0

use core::{cmp::min, mem::size_of};

use ostd::const_assert;

use super::{prelude::*, super_block::SuperBlock};

/// In-memory block group descriptor.
#[derive(Clone, Copy, Debug)]
pub struct BlockGroupDesc {
    pub block_bitmap: u32,
    pub inode_bitmap: u32,
    pub inode_table: u32,
    pub free_blocks_count: u16,
    pub free_inodes_count: u16,
    pub dirs_count: u16,
}

impl From<RawGroupDescriptor> for BlockGroupDesc {
    fn from(desc: RawGroupDescriptor) -> Self {
        Self {
            block_bitmap: desc.block_bitmap,
            inode_bitmap: desc.inode_bitmap,
            inode_table: desc.inode_table,
            free_blocks_count: desc.free_blocks_count,
            free_inodes_count: desc.free_inodes_count,
            dirs_count: desc.dirs_count,
        }
    }
}

const_assert!(size_of::<RawGroupDescriptor>() == 32);

/// On-disk block group descriptor (ext2_group_desc).
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
struct RawGroupDescriptor {
    block_bitmap: u32,
    inode_bitmap: u32,
    inode_table: u32,
    free_blocks_count: u16,
    free_inodes_count: u16,
    dirs_count: u16,
    pad: u16,
    reserved: [u32; 3],
}

/// Cached block group descriptor table.
pub struct BlockGroupDescTable {
    descs: Vec<RwMutex<BlockGroupDesc>>,
    groups_count: u32,
    desc_per_block: u32,
}

impl BlockGroupDescTable {
    pub fn groups_count(&self) -> u32 {
        self.groups_count
    }

    pub fn desc_per_block(&self) -> u32 {
        self.desc_per_block
    }

    pub fn group_desc(&self, idx: usize) -> Result<RwMutexReadGuard<'_, BlockGroupDesc>> {
        let desc = self.descs.get(idx).ok_or_else(|| {
            Error::with_message(Errno::EINVAL, "block group index out of range")
        })?;
        Ok(desc.read())
    }

    pub fn group_desc_mut(&self, idx: usize) -> Result<RwMutexWriteGuard<'_, BlockGroupDesc>> {
        let desc = self.descs.get(idx).ok_or_else(|| {
            Error::with_message(Errno::EINVAL, "block group index out of range")
        })?;
        Ok(desc.write())
    }
}

pub fn load_group_desc_table(
    device: &dyn BlockDevice,
    sb: &SuperBlock,
) -> Result<BlockGroupDescTable> {
    let groups_count = sb.block_groups_count();
    if groups_count == 0 {
        return_errno_with_message!(Errno::EINVAL, "zero block groups");
    }

    let inode_size = sb.inode_size();
    if inode_size == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid inode size");
    }
    let inodes_per_block = BLOCK_SIZE / inode_size;
    if inodes_per_block == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid inode size");
    }

    let inodes_per_group = sb.inodes_per_group();
    if inodes_per_group == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid inodes per group");
    }
    let itb_per_group = inodes_per_group / inodes_per_block as u32;
    if itb_per_group == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid itb per group");
    }

    let blocks_per_group = sb.blocks_per_group();
    if blocks_per_group == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid blocks per group");
    }

    let total_blocks = sb.total_blocks();
    if total_blocks == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid total blocks");
    }

    let first_data_block = sb.first_data_block();

    let desc_per_block = (BLOCK_SIZE / size_of::<RawGroupDescriptor>()) as u32;
    if desc_per_block == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid descriptors per block");
    }

    let table_offset = sb.group_descriptors_bid(0).to_offset();
    let mut descs = Vec::with_capacity(groups_count as usize);

    for idx in 0..groups_count as usize {
        let offset = size_of::<RawGroupDescriptor>()
            .checked_mul(idx)
            .and_then(|delta| table_offset.checked_add(delta))
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "descriptor offset overflow"))?;
        let raw = device.read_val::<RawGroupDescriptor>(offset)?;
        let desc = BlockGroupDesc::from(raw);

        let first_block = (first_data_block as u64)
            + (idx as u64) * (blocks_per_group as u64);
        let last_block = min(
            first_block + (blocks_per_group as u64) - 1,
            (total_blocks as u64) - 1,
        );

        let block_bitmap = desc.block_bitmap as u64;
        if block_bitmap < first_block || block_bitmap > last_block {
            return_errno_with_message!(Errno::EINVAL, "block bitmap not in group");
        }

        let inode_bitmap = desc.inode_bitmap as u64;
        if inode_bitmap < first_block || inode_bitmap > last_block {
            return_errno_with_message!(Errno::EINVAL, "inode bitmap not in group");
        }

        let inode_table = desc.inode_table as u64;
        if inode_table < first_block || inode_table > last_block {
            return_errno_with_message!(Errno::EINVAL, "inode table not in group");
        }

        let itb_end = inode_table + (itb_per_group as u64) - 1;
        if itb_end > last_block {
            return_errno_with_message!(Errno::EINVAL, "inode table not in group");
        }

        descs.push(RwMutex::new(desc));
    }

    Ok(BlockGroupDescTable {
        descs,
        groups_count,
        desc_per_block,
    })
}
