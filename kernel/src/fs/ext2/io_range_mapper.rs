// SPDX-License-Identifier: MPL-2.0

use core::ops::Range;

use ostd::sync::RwMutexReadGuard;

use super::prelude::*;
use crate::fs::ext2::{
    Ext2,
    block_ptr::{Ext2Bid, InodeBlockMap},
};

/// One contiguous mapped run translated from logical file blocks to device blocks.
pub(super) struct MappedRange {
    /// Logical block interval in the file that produced this mapped run.
    pub(super) logical_block_range: Range<u32>,
    /// Physical device block interval backing the logical block interval.
    pub(super) device_block_range: Range<Ext2Bid>,
}

pub(super) struct IoRangeMapper<'a> {
    range: Range<u32>,
    mapping: RwMutexReadGuard<'a, InodeBlockMap>,
    fs: &'a Ext2,
}


pub(super) enum IoRange {
    // A contigunous range of device blocks, mapping logiccal range to physical range.
    Mapped(MappedRange),
    // A hole in the file, with the length of the hole.
    Hole(Range<u32>),
}

impl<'a> IoRangeMapper<'a> {
    /// Linux: /root/linux/fs/ext2/inode.c:783 (ext2_get_block)
    /// Linux: /root/asterinas/kernel/src/fs/ext2_old/inode.rs:1997 (DeviceRangeReader::new)
    pub(super) fn new(
        range: Range<Ext2Bid>,
        mapping: RwMutexReadGuard<'a, InodeBlockMap>,
        fs: &'a Ext2,
    ) -> Self {
        Self {
            range: range.start..range.end,
            mapping,
            fs,
        }
    }

    // Return the next mapped for the given range to perform io,
    // if the result is Mapped: then the upper layer can directly read/write the device block range.
    // if the result is Hole: then the upper layer should zero out the hole when reading, and fallback to buffer i/o when writing.
    // if the result is PartialMapped: then the upper layer should read-modify-write the block.
    pub(super) fn next(&mut self) -> Result<Option<IoRange>> {
        todo!()
    }
}
