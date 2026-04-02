// SPDX-License-Identifier: MPL-2.0

//! Inode-local cache of indirect metadata blocks.

use core::mem::size_of;

use lru::LruCache;

use super::{block_ptr_tree::Ext2Bid, fs::Ext2, prelude::*};

/// Inode-local cache for indirect metadata blocks.
#[derive(Debug)]
pub(super) struct IndirectBlockManager {
    cache: LruCache<Ext2Bid, IndirectBlock>,
    capacity: usize,
    fs: Weak<Ext2>,
}

impl IndirectBlockManager {
    pub(super) fn new(fs: Weak<Ext2>) -> Self {
        /// Keeps the resident indirect-block cache small and bounded.
        const MAX_SIZE: usize = 16;

        Self {
            cache: LruCache::unbounded(),
            capacity: MAX_SIZE,
            fs,
        }
    }

    pub(super) fn find(&mut self, bid: Ext2Bid) -> Result<&IndirectBlock> {
        if self.cache.get(&bid).is_none() {
            self.try_shrink()?;
            let block = self.load_block(bid)?;
            self.cache.put(bid, block);
        }

        self.cache.get(&bid).ok_or_else(|| {
            Error::with_message(Errno::EIO, "failed to retain resident indirect block")
        })
    }

    pub(super) fn find_mut(&mut self, bid: Ext2Bid) -> Result<&mut IndirectBlock> {
        if self.cache.get(&bid).is_none() {
            self.try_shrink()?;
            let block = self.load_block(bid)?;
            self.cache.put(bid, block);
        }

        self.cache.get_mut(&bid).ok_or_else(|| {
            Error::with_message(Errno::EIO, "failed to retain resident indirect block")
        })
    }

    pub(super) fn insert_new(&mut self, bid: Ext2Bid, block: IndirectBlock) -> Result<()> {
        if block.bid() != bid {
            return_errno_with_message!(Errno::EIO, "indirect block inserted with mismatched bid");
        }

        self.try_shrink()?;
        self.cache.put(bid, block);
        Ok(())
    }

    pub(super) fn remove(&mut self, bid: Ext2Bid) -> Option<IndirectBlock> {
        self.cache.pop(&bid)
    }

    pub(super) fn sync(&mut self) -> Result<()> {
        let fs = self.fs_arc()?;
        let dirty_bids: Vec<Ext2Bid> = self
            .cache
            .iter()
            .filter_map(|(bid, block)| block.is_dirty().then_some(*bid))
            .collect();

        for bid in dirty_bids {
            let block = self.cache.get_mut(&bid).ok_or_else(|| {
                Error::with_message(Errno::EIO, "indirect block disappeared during sync")
            })?;
            Self::write_back_with_fs(&fs, block)?;
        }

        Ok(())
    }

    pub(super) fn try_shrink(&mut self) -> Result<()> {
        while self.cache.len() >= self.capacity {
            self.evict()?;
        }
        Ok(())
    }

    fn evict(&mut self) -> Result<()> {
        let Some((bid, mut block)) = self.cache.pop_lru() else {
            return Ok(());
        };

        if let Err(err) = Self::write_back_with_fs(&self.fs_arc()?, &mut block) {
            self.cache.put(bid, block);
            return Err(err);
        }

        Ok(())
    }

    fn load_block(&self, bid: Ext2Bid) -> Result<IndirectBlock> {
        let fs = self.fs_arc()?;
        let mut block = IndirectBlock::alloc_uninit()?;
        block.set_bid(bid);
        let bio_segment = BioSegment::new_from_segment(
            Segment::<()>::from(block.frame().clone()).into(),
            BioDirection::FromDevice,
        );
        fs.read_blocks(bid, bio_segment)
            .map_err(|_| Error::with_message(Errno::EIO, "failed to submit indirect block read"))?;
        block.mark_clean();
        Ok(block)
    }

    fn write_back_with_fs(fs: &Arc<Ext2>, block: &mut IndirectBlock) -> Result<()> {
        if !block.is_dirty() {
            return Ok(());
        }

        let bio_segment = BioSegment::new_from_segment(
            Segment::<()>::from(block.frame().clone()).into(),
            BioDirection::ToDevice,
        );
        fs.write_blocks(block.bid(), bio_segment).map_err(|_| {
            Error::with_message(Errno::EIO, "failed to submit indirect block writeback")
        })?;

        block.mark_clean();
        Ok(())
    }

    fn fs_arc(&self) -> Result<Arc<Ext2>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))
    }
}

/// One resident indirect metadata block.
#[derive(Debug)]
pub(super) struct IndirectBlock {
    block: Frame<()>,
    bid: Ext2Bid,
    dirty: bool,
}

impl IndirectBlock {
    pub(super) fn alloc_uninit() -> Result<Self> {
        Ok(Self {
            block: FrameAllocOptions::new().zeroed(false).alloc_frame()?,
            bid: 0,
            dirty: false,
        })
    }

    pub(super) fn alloc_new(bid: Ext2Bid) -> Result<Self> {
        Ok(Self {
            block: FrameAllocOptions::new().alloc_frame()?,
            bid,
            dirty: true,
        })
    }

    pub(super) fn read_bid(&self, idx: usize) -> Result<Ext2Bid> {
        let offset = self.slot_offset(idx)?;
        self.block
            .read_val(offset)
            .map_err(|_| Error::with_message(Errno::EIO, "failed to read indirect pointer"))
    }

    pub(super) fn write_bid(&mut self, idx: usize, bid: Ext2Bid) -> Result<()> {
        let offset = self.slot_offset(idx)?;
        self.block
            .write_val(offset, &bid)
            .map_err(|_| Error::with_message(Errno::EIO, "failed to write indirect pointer"))?;
        self.dirty = true;
        Ok(())
    }

    pub(super) fn clear(&mut self) {
        let zeroes = [0u8; BLOCK_SIZE];
        let _ = self.block.write_bytes(0, &zeroes);
        self.dirty = true;
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub(super) fn bid(&self) -> Ext2Bid {
        self.bid
    }

    fn frame(&self) -> &Frame<()> {
        &self.block
    }

    fn mark_clean(&mut self) {
        self.dirty = false;
    }

    fn set_bid(&mut self, bid: Ext2Bid) {
        self.bid = bid;
    }

    fn slot_offset(&self, idx: usize) -> Result<usize> {
        let offset = idx
            .checked_mul(size_of::<Ext2Bid>())
            .ok_or_else(|| Error::with_message(Errno::EIO, "indirect pointer index overflow"))?;
        if offset + size_of::<Ext2Bid>() > BLOCK_SIZE {
            return_errno_with_message!(Errno::EIO, "indirect pointer index out of bounds");
        }
        Ok(offset)
    }
}
