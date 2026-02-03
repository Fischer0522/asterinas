// SPDX-License-Identifier: MPL-2.0

use core::mem::size_of;

use ostd::const_assert;

use super::prelude::*;

#[derive(Debug)]
pub struct BlockGroup;

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
