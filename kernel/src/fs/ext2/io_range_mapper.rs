// SPDX-License-Identifier: MPL-2.0

use core::ops::Range;

use ostd::sync::RwMutexReadGuard;

use super::prelude::*;
use crate::fs::ext2::{
    Ext2,
    block_ptr::{Ext2Bid, InodeBlockMap},
};

/// One contiguous mapped run translated from logical file blocks to device blocks.
#[derive(Debug, PartialEq, Eq)]
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

/// Direct-I/O block-range classification for the current logical interval.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum IoRange {
    /// A contiguous mapped device-block range.
    Mapped(MappedRange),
    /// A hole in the file expressed as a logical-block interval.
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

    /// Returns the next logical run for direct I/O planning.
    ///
    /// `IoRange::Mapped` returns one maximal run whose logical blocks are
    /// backed by contiguous physical device blocks.
    /// `IoRange::Hole` returns one maximal logical hole run.
    pub(super) fn next(&mut self) -> Result<Option<IoRange>> {
        let mut current_logical_start = self.range.start;
        let mut current_physical_range: Option<Range<Ext2Bid>> = None;
        let mut is_hole = false;

        while self.range.start < self.range.end {
            let iblock = self.range.start;
            match self.mapping.get_block(self.fs, iblock)? {
                Some(bid) => {
                    if is_hole {
                        // Keep `self.range.start` on the first mapped block so the
                        // next call can start a mapped run from that block.
                        return Ok(Some(IoRange::Hole(current_logical_start..iblock)));
                    }

                    if let Some(device_range) = current_physical_range.as_mut() {
                        if bid != device_range.end {
                            // Stop before the first physically non-contiguous block.
                            // The next call will resume from the same logical block
                            // and start a new mapped run there.
                            return Ok(Some(IoRange::Mapped(MappedRange {
                                logical_block_range: current_logical_start..iblock,
                                device_block_range: device_range.clone(),
                            })));
                        }
                        device_range.end += 1;
                    } else {
                        current_logical_start = iblock;
                        current_physical_range = Some(bid..bid + 1);
                    }
                }
                None => {
                    if let Some(device_physical_range) = current_physical_range.take() {
                        // A hole terminates the current mapped run. Leave the hole's
                        // logical block unconsumed so the next call can report the
                        // hole run explicitly.
                        return Ok(Some(IoRange::Mapped(MappedRange {
                            logical_block_range: current_logical_start..iblock,
                            device_block_range: device_physical_range,
                        })));
                    }
                    if !is_hole {
                        is_hole = true;
                        current_logical_start = iblock;
                    }
                }
            }
            self.range.start += 1;
        }

        if is_hole {
            Ok(Some(IoRange::Hole(current_logical_start..self.range.end)))
        } else if let Some(device_range) = current_physical_range {
            Ok(Some(IoRange::Mapped(MappedRange {
                logical_block_range: current_logical_start..self.range.end,
                device_block_range: device_range,
            })))
        } else {
            Ok(None)
        }
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::*;
    use crate::{
        fs::ext2::{block_ptr::BlockMapDesc, testkit::Ext2FixtureBuilder},
        prelude::*,
        time::clocks,
    };

    fn make_mapping(block_ptrs: [u32; 15], fs: &Arc<Ext2>) -> InodeBlockMap {
        InodeBlockMap::new(BlockMapDesc::from_parts(0, block_ptrs), Arc::downgrade(fs))
    }

    #[ktest]
    fn io_range_mapper() {
        clocks::init_for_ktest();
        let f = Ext2FixtureBuilder::new(1, 256).build().unwrap();

        let mut block_ptrs = [0u32; 15];
        block_ptrs[0] = 11;
        block_ptrs[1] = 12;
        block_ptrs[2] = 20;
        block_ptrs[4] = 30;
        let mapping = make_mapping(block_ptrs, &f.ext2);

        let binding = RwMutex::new(mapping);
        let mut mapper = IoRangeMapper::new(0..5, binding.read(), &f.ext2);

        assert_eq!(
            mapper.next().unwrap(),
            Some(IoRange::Mapped(MappedRange {
                logical_block_range: 0..2,
                device_block_range: 11..13,
            }))
        );
        assert_eq!(
            mapper.next().unwrap(),
            Some(IoRange::Mapped(MappedRange {
                logical_block_range: 2..3,
                device_block_range: 20..21,
            }))
        );
        assert_eq!(mapper.next().unwrap(), Some(IoRange::Hole(3..4)));
        assert_eq!(
            mapper.next().unwrap(),
            Some(IoRange::Mapped(MappedRange {
                logical_block_range: 4..5,
                device_block_range: 30..31,
            }))
        );
        assert_eq!(mapper.next().unwrap(), None);
    }
}
