// SPDX-License-Identifier: MPL-2.0

//! Batches logical block lookups into contiguous device-block runs for direct I/O.

use core::ops::Range;

use super::{
    block_ptr_tree::{BlockPtrTree, Ext2Bid, Iblock},
    prelude::*,
};

/// One contiguous mapped run translated from logical file blocks to device blocks.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct MappedRange {
    /// Logical block interval in the file that produced this mapped run.
    pub(super) logical_block_range: Range<Iblock>,
    /// Physical device block interval backing the logical block interval.
    pub(super) device_block_range: Range<Ext2Bid>,
}

/// Iterates logical block ranges and classifies each as mapped or hole.
///
/// Used by the direct-I/O path to batch contiguous device-block runs
/// into single BIO requests.
pub(super) struct IoRangeMapper<'a> {
    range: Range<Iblock>,
    block_ptr_tree: RwMutexReadGuard<'a, BlockPtrTree>,
}

/// Direct-I/O block-range classification for the current logical interval.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum IoRange {
    /// A contiguous mapped device-block range.
    Mapped(MappedRange),
    /// A hole in the file expressed as a logical-block interval.
    Hole(Range<Iblock>),
}

impl<'a> IoRangeMapper<'a> {
    pub(super) fn new(
        range: Range<Iblock>,
        block_ptr_tree: RwMutexReadGuard<'a, BlockPtrTree>,
    ) -> Self {
        Self {
            range: range.start..range.end,
            block_ptr_tree,
        }
    }

    /// Returns the next logical run for direct I/O planning.
    ///
    /// `IoRange::Mapped` returns one maximal run whose logical blocks are
    /// backed by contiguous physical device blocks.
    /// `IoRange::Hole` returns one maximal logical hole run.
    pub(super) fn next(&mut self) -> Result<Option<IoRange>> {
        if self.range.start >= self.range.end {
            return Ok(None);
        }

        let start_iblock = self.range.start;
        let max_blocks = self.range.end - self.range.start;
        let device_block_range = self
            .block_ptr_tree
            .lookup_block_range(start_iblock, max_blocks)?;
        if !device_block_range.is_empty() {
            debug_assert!(device_block_range.end >= device_block_range.start);
            let logical_end = start_iblock + device_block_range.end - device_block_range.start;
            self.range.start = logical_end;
            return Ok(Some(IoRange::Mapped(MappedRange {
                logical_block_range: start_iblock..logical_end,
                device_block_range,
            })));
        }

        let hole_start = start_iblock;
        self.range.start += 1;
        while self.range.start < self.range.end {
            let iblock = self.range.start;
            let remaining = self.range.end - iblock;
            if !self
                .block_ptr_tree
                .lookup_block_range(iblock, remaining)?
                .is_empty()
            {
                break;
            }
            self.range.start += 1;
        }

        Ok(Some(IoRange::Hole(hole_start..self.range.start)))
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::*;
    use crate::{
        fs::{
            ext2::Ext2,
            fs_impls::ext2::{block_ptr_tree::RawBlockPtrs, testkit::Ext2FixtureBuilder},
        },
        prelude::*,
        time::clocks,
    };

    fn make_block_map(block_ptrs: [u32; 15], fs: &Arc<Ext2>) -> BlockPtrTree {
        BlockPtrTree::new(RawBlockPtrs::from_parts(0, block_ptrs), Arc::downgrade(fs))
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
        let block_ptr_tree = make_block_map(block_ptrs, &f.ext2);

        let binding = RwMutex::new(block_ptr_tree);
        let mut mapper = IoRangeMapper::new(0..5, binding.read());

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
