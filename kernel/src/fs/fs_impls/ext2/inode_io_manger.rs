// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::{AtomicUsize, Ordering};

use aster_block::bio::BioCompleteFn;
use alloc::sync::Weak;
use ostd::sync::RwMutex;

use crate::fs::ext2::inode_block_map::Ext2Bid;

use super::{
    fs::Ext2,
    inode_block_map::InodeBlockMap,
    prelude::*,
};




/// Represents a prepared write operation that can be either applied or rolled back.
#[derive(Debug)]
pub(super) struct WritePlan {
    old_size: usize,
    offset: usize,
    end: usize,
    block_size: usize,
    allocated_bids: Vec<Ext2Bid>,
}


impl WritePlan {
    pub(super) fn apply(self) {
        todo!()
    }

    pub(super) fn rollback(self) {
        todo!()
    }
}

/// Inode I/O manager that coordinates block map mutations and page cache I/O.
/// It manages block mapping, partial block zeroing, and direct I/O.
/// It is implemented as a `PageCacheBackend` and wrapped as an `Arc`, shared
/// between `Inode` and `PageCache`, protecting the concurrent access between `write_at`/`read_at` and `mmap`.
#[derive(Debug)]
pub(super) struct InodeIoManager {
    /// Serializes backend traversal vs foreground block-map mutations.
    block_map: RwMutex<InodeBlockMap>,
    /// Cached `npages` bound for PageCache.
    npages: AtomicUsize,
    /// Filesystem handle for indirect I/O and BIO submission.
    fs: Weak<Ext2>,
}


impl InodeIoManager {
    pub(super) fn new(block_map: InodeBlockMap, fs: Weak<Ext2>, npages: usize) -> Arc<Self> {
        Arc::new(Self {
            block_map: RwMutex::new(block_map),
            npages: AtomicUsize::new(npages),
            fs,
        })
    }

    pub(super) fn npages(&self) -> usize {
        self.npages.load(Ordering::Acquire)
    }

    pub(super) fn set_npages(&self, npages: usize) {
        self.npages.store(npages, Ordering::Release);
    }

    pub(super) fn read_block_map(&self) -> RwMutexReadGuard<'_, InodeBlockMap> {
        self.block_map.read()
    }

    pub(super) fn write_block_map(&self) -> RwMutexWriteGuard<'_, InodeBlockMap> {
        self.block_map.write()
    }

    pub(super) fn prepare_write(&self, offset: usize, len: usize) -> Result<WritePlan> {
        // 1. Zero the old partial tail if the write extends EOF (no need for write_link, since it is a new inode).
        // and zero the new tail if the last block is partial.
        // 2. handle the partial head and tail for current writes.
        // 3. Allocate new blocks for the write range.
        // 4. fill zero for the partial head and tail.
        // 5. Create a handle that contains enssentail metadata for the write and rollback

        todo!()
    }

    pub(super) fn rollback_write(&self, write: WritePlan) -> Result<()> {
        // only truncate_blocks,
        // let the caller udpate file size and truncate page cache
        todo!()
    }

    // fill zeros the page cache
    fn zero_blocks(&self,old_start: usize, old_end:usize, new_start: usize, new_end: usize) -> Result<()> {
        todo!()
    }

    fn fs(&self) -> Result<Arc<Ext2>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))
    }

}

impl PageCacheBackend for InodeIoManager {
    fn read_page_raw(
        &self,
        idx: usize,
        bio_segment: BioSegment,
        complete_fn: Option<BioCompleteFn>,
    ) -> Result<BioWaiter> {
        let block_map = self.block_map.read();
        let fs = self.fs()?;
        let iblock = u32::try_from(idx)
            .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;

        let bid = block_map
            .get_block(&fs, iblock)?
            .ok_or_else(|| Error::with_message(Errno::EIO, "sparse hole should not issue raw read"))?;
        fs.read_blocks_async(bid, bio_segment, complete_fn)
    }

    fn write_page_raw(
        &self,
        idx: usize,
        bio_segment: BioSegment,
        complete_fn: Option<BioCompleteFn>,
    ) -> Result<BioWaiter> {
        let block_map = self.block_map.upread();
        let fs = self.fs()?;
        let iblock = u32::try_from(idx)
            .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;

        // Fast path: bid is already allocated, only acquire read lock.
        let mut bid = block_map.get_block(&fs, iblock)?;

        // Slow path: bid is not allocated, acquire write lock and allocate.
        // In the normal write path, the blocks should be already allocated by foreground write,
        // but if we perform mmap then truncate and resize to the origin size,
        // the blocks are already reclaimed and only holes left.
        // In this case, we need to allocate new blocks for the mmaped pages when triggering writeback.
        if bid.is_none() {
            let mut block_map = block_map.upgrade();
            bid = block_map.get_or_alloc_block(&fs, iblock, true)?;
        }
        // The bid is guaranteed to be allocated.
        let bid = bid.unwrap();
        fs.write_blocks_async(bid, bio_segment, complete_fn)
    }

    fn npages(&self) -> usize {
        self.npages()
    }
}
