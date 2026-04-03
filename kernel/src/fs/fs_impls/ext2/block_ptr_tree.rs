// SPDX-License-Identifier: MPL-2.0

//! Logical-to-physical block translation via the ext2 block-pointer tree.

use core::{mem::size_of, ops::Range};

use device_id::{decode_device_numbers, encode_device_numbers};
use ostd::sync::Mutex;

use super::{
    fs::Ext2,
    indirect_block_manager::{IndirectBlock, IndirectBlockManager},
    inode::RawInode,
    prelude::*,
};

pub(super) type Ext2Bid = u32;

// TODO: Refactor this into a more idiomatic Rust representation, such as an `enum`.
/// Block path offsets for direct/indirect traversal.
///
/// Produced by `logical_block_to_path` from a logical block number.
#[derive(Clone, Copy, Debug)]
pub(super) struct BlockPointerPath {
    /// Number of levels in the pointer chain (1 = direct, 2 = single indirect,
    /// 3 = double indirect, 4 = triple indirect). A depth of 0 is invalid.
    pub depth: usize,
    /// Index at each level of the block pointer tree. Only `offsets[0..depth]`
    /// are meaningful. `offsets[0]` indexes into `inode.i_block[]` (0..11 for
    /// direct, 12/13/14 for indirect entries); subsequent elements index into
    /// the corresponding indirect block.
    pub offsets: [u32; 4],
    /// Number of consecutive block slots remaining after the current offset
    /// within the lowest-level indirect block (or the direct region for depth 1).
    /// Used for multi-block contiguous allocation optimization.
    pub boundary: u32,
}

/// A single level in the block-pointer chain.
///
#[derive(Clone, Debug)]
struct IndirectBlockEntry {
    /// Physical block number read from this level's slot; 0 means hole.
    key: Ext2Bid,
    /// Indirect metadata block that contains this level's slot.
    /// `None` for level 0, where the slot lives in inode `i_block[]`.
    bh: Option<Ext2Bid>,
}

/// Result of traversing a block-pointer chain.
#[derive(Debug)]
struct BranchChainWalkResult {
    /// Level where traversal stopped on a zero pointer, or `path.depth` if complete.
    partial_level: usize,
    /// Entries traversed so far.
    chain: Vec<IndirectBlockEntry>,
}

/// Tracks allocated metadata/data blocks during block allocation and frees
/// them on drop unless committed.
#[derive(Debug)]
struct BlockAllocGuard {
    fs: Arc<Ext2>,
    indirect_blocks: Vec<Ext2Bid>,
    data_blocks: Range<Ext2Bid>,
    committed: bool,
}

impl BlockAllocGuard {
    fn new(fs: Arc<Ext2>) -> Self {
        Self {
            fs,
            indirect_blocks: Vec::new(),
            data_blocks: Range { start: 0, end: 0 },
            committed: false,
        }
    }

    fn track_indirect_blocks(&mut self, indirect_blocks: Vec<Ext2Bid>) {
        self.indirect_blocks = indirect_blocks;
    }

    fn track_data_blocks(&mut self, data_blocks: Range<Ext2Bid>) {
        self.data_blocks = data_blocks;
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for BlockAllocGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }

        for &bid in self.indirect_blocks.iter() {
            if let Err(err) = self.fs.free_blocks(bid, 1) {
                error!("failed to free indirect block {} in rollback: {:?}", bid, err);
            }
        }

        let data_count = self.data_blocks.end.saturating_sub(self.data_blocks.start);
        if data_count > 0 {
            if let Err(err) = self.fs.free_blocks(self.data_blocks.start, data_count) {
                error!("failed to free data blocks in rollback: {:?}", err);
            }
        }
    }
}

/// On-disk block pointer state copied from [`RawInode`].
///
/// Holds `i_blocks` (sector count) and the 15-entry `i_block[]` array.
#[derive(Clone, Copy, Debug)]
#[must_use = "RawBlockPtrs snapshot must be synced back to InodeDesc"]
pub(super) struct RawBlockPtrs {
    pub(super) sector_count: u32,
    pub(super) block_ptrs: [u32; 15],
}

impl RawBlockPtrs {
    pub(super) fn from_raw(raw: &RawInode) -> Self {
        Self {
            sector_count: raw.sector_count,
            block_ptrs: raw.block,
        }
    }

    pub(super) fn from_parts(sector_count: u32, block_ptrs: [u32; 15]) -> Self {
        Self {
            sector_count,
            block_ptrs,
        }
    }

    /// Decodes the ext2 special-file device encoding stored in `i_block`.
    pub(super) fn decode_device_id(&self) -> u64 {
        let (major, minor) = if self.block_ptrs[0] != 0 {
            let val = self.block_ptrs[0];
            // Old_decode_dev: (major << 8) | minor with 8-bit major/minor.
            (((val >> 8) & 0xFF), (val & 0xFF))
        } else {
            let dev = self.block_ptrs[1];
            // Decode the extended major/minor bit layout.
            (
                ((dev & 0xFFF00) >> 8),
                ((dev & 0xFF) | ((dev >> 12) & 0xFFF00)),
            )
        };

        encode_device_numbers(major, minor)
    }

    /// Encodes a device ID into the ext2 special-file `i_block` layout.
    pub(super) fn encode_device_id(&mut self, device_id: u64) {
        let (major, minor) = decode_device_numbers(device_id);

        // Old_valid_dev: MAJOR/MINOR must both fit in 8 bits.
        if major < 256 && minor < 256 {
            self.block_ptrs[0] = (major << 8) | minor;
            self.block_ptrs[1] = 0;
        } else {
            self.block_ptrs[0] = 0;
            self.block_ptrs[1] = (minor & 0xFF) | (major << 8) | ((minor & !0xFF) << 12);
            self.block_ptrs[2] = 0;
        }
    }
}

/// Manages the ext2 block-pointer tree for one inode.
///
/// Translates logical block numbers to physical device blocks
/// by traversing the direct, single-indirect, double-indirect,
/// and triple-indirect pointer chain stored in `i_block[15]`.
/// Also handles block allocation, deallocation, and truncation.
#[derive(Debug)]
pub(super) struct BlockPtrTree {
    pub(super) raw_block_ptrs: Dirty<RawBlockPtrs>,
    pub(super) indirect_blocks_manager: Mutex<IndirectBlockManager>,
}

impl BlockPtrTree {
    pub(super) fn new(desc: RawBlockPtrs, fs: Weak<Ext2>) -> Self {
        Self {
            raw_block_ptrs: Dirty::new(desc),
            indirect_blocks_manager: Mutex::new(IndirectBlockManager::new(fs)),
        }
    }

    pub(super) fn raw_block_ptrs(&self) -> &RawBlockPtrs {
        &self.raw_block_ptrs
    }

    pub(super) fn sync_indirect_blocks(&self) -> Result<()> {
        self.indirect_blocks_manager.lock().sync()
    }

    /// Translates a logical block number into a path of block pointer offsets.
    ///
    pub(super) fn logical_block_to_path(&self, fs: &Ext2, iblock: u32) -> Result<BlockPointerPath> {
        let sb = fs.super_block();
        let ptrs = (sb.block_size() / size_of::<u32>()) as u32;
        if ptrs == 0 {
            return_errno_with_message!(Errno::EINVAL, "block number out of range");
        }

        let ptrs_bits = ptrs.trailing_zeros();
        let direct_blocks = 12u32;
        let indirect_blocks = ptrs;
        let double_blocks = 1u32
            .checked_shl(ptrs_bits * 2)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "block path shift overflow"))?;

        let mut offsets = [0u32; 4];
        let depth;
        let boundary;
        let mut block = iblock;

        if block < direct_blocks {
            offsets[0] = block;
            depth = 1usize;
            boundary = direct_blocks - 1 - block;
        } else if {
            block -= direct_blocks;
            block < indirect_blocks
        } {
            offsets[0] = 12;
            offsets[1] = block;
            depth = 2usize;
            boundary = ptrs - 1 - (block & (ptrs - 1));
        } else if {
            block -= indirect_blocks;
            block < double_blocks
        } {
            offsets[0] = 13;
            offsets[1] = block >> ptrs_bits;
            offsets[2] = block & (ptrs - 1);
            depth = 3usize;
            boundary = ptrs - 1 - (block & (ptrs - 1));
        } else if {
            block -= double_blocks;
            (block >> (ptrs_bits * 2)) < ptrs
        } {
            offsets[0] = 14;
            offsets[1] = block >> (ptrs_bits * 2);
            offsets[2] = (block >> ptrs_bits) & (ptrs - 1);
            offsets[3] = block & (ptrs - 1);
            depth = 4usize;
            boundary = ptrs - 1 - (block & (ptrs - 1));
        } else {
            return_errno_with_message!(Errno::EINVAL, "block number exceeds maximum");
        }

        Ok(BlockPointerPath {
            depth,
            offsets,
            boundary,
        })
    }

    /// Traverses the existing block pointer chain for a block path.
    ///
    fn walk_block_chain(
        &self,
        path: &BlockPointerPath,
        fs: &Ext2,
    ) -> Result<BranchChainWalkResult> {
        if path.depth == 0 || path.depth > path.offsets.len() {
            return_errno_with_message!(Errno::EIO, "invalid block path depth");
        }

        let top_offset = path.offsets[0] as usize;
        let top_key = *self
            .raw_block_ptrs
            .block_ptrs
            .get(top_offset)
            .ok_or_else(|| Error::with_message(Errno::EIO, "invalid top-level block pointer"))?;

        let mut chain = Vec::with_capacity(path.depth);
        chain.push(IndirectBlockEntry {
            key: top_key,
            bh: None,
        });
        if top_key == 0 {
            // Zero pointer means the chain is broken at level 0.
            return Ok(BranchChainWalkResult {
                partial_level: 0,
                chain,
            });
        }

        if fs.block_size() < size_of::<u32>() {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        // Callers hold `InodeInner` locks, so chain pointers are stable and no
        // retry loop is needed while walking the existing branch.
        for level in 1..path.depth {
            let parent_key = chain[level - 1].key;
            let next_key = self
                .indirect_blocks_manager
                .lock()
                .find(parent_key)?
                .read_bid(path.offsets[level] as usize)?;
            chain.push(IndirectBlockEntry {
                key: next_key,
                bh: Some(parent_key),
            });
            if next_key == 0 {
                // Include the zero-key entry and report break level.
                return Ok(BranchChainWalkResult {
                    partial_level: level,
                    chain,
                });
            }
        }

        Ok(BranchChainWalkResult {
            partial_level: path.depth,
            chain,
        })
    }

    fn max_blocks_in_run(path: &BlockPointerPath, max_blocks: u32) -> u32 {
        max_blocks.min(path.boundary.saturating_add(1))
    }

    fn mapped_range_from_branch(
        &self,
        path: &BlockPointerPath,
        branch: &BranchChainWalkResult,
        max_blocks: u32,
    ) -> Result<Range<Ext2Bid>> {
        if max_blocks == 0 {
            return_errno_with_message!(Errno::EINVAL, "zero block range requested");
        }
        // A partial walk means the logical block lands in a hole, so there is
        // no mapped physical range to report.
        if branch.partial_level < path.depth {
            return Ok(0..0);
        }

        // The last chain entry is the first mapped data block for `iblock`.
        let first_bid = branch
            .chain
            .get(path.depth - 1)
            .ok_or_else(|| Error::with_message(Errno::EIO, "incomplete branch result"))?
            .key;
        if first_bid == 0 {
            return Ok(0..0);
        }

        let max_count = Self::max_blocks_in_run(path, max_blocks);
        let mut count = 1u32;
        let start_slot = path.offsets[path.depth - 1] as usize;

        if path.depth == 1 {
            // Direct blocks are stored inline in the inode, so scan forward in
            // `i_block[]` while the next physical block stays contiguous.
            while count < max_count {
                let slot = start_slot
                    .checked_add(count as usize)
                    .ok_or_else(|| Error::with_message(Errno::EIO, "direct slot overflow"))?;
                let Some(&next_bid) = self.raw_block_ptrs.block_ptrs.get(slot) else {
                    break;
                };
                if next_bid == 0 || next_bid != first_bid.saturating_add(count) {
                    break;
                }
                count += 1;
            }
        } else {
            // Indirect cases share the same contiguity rule, but the leaf slots
            // live in the final indirect block reached by the walk.
            let leaf_bid = branch
                .chain
                .get(path.depth - 1)
                .and_then(|entry| entry.bh)
                .ok_or_else(|| Error::with_message(Errno::EIO, "missing indirect leaf block"))?;
            let mut indirect_blocks = self.indirect_blocks_manager.lock();
            let leaf_block = indirect_blocks.find(leaf_bid)?;
            while count < max_count {
                let slot = start_slot
                    .checked_add(count as usize)
                    .ok_or_else(|| Error::with_message(Errno::EIO, "indirect slot overflow"))?;
                let next_bid = leaf_block.read_bid(slot)?;
                if next_bid == 0 || next_bid != first_bid.saturating_add(count) {
                    break;
                }
                count += 1;
            }
        }

        Ok(first_bid..first_bid.saturating_add(count))
    }

    fn blks_to_allocate(
        &self,
        branch: &BranchChainWalkResult,
        path: &BlockPointerPath,
        max_blocks: u32,
    ) -> Result<(u32, u32)> {
        if max_blocks == 0 {
            return_errno_with_message!(Errno::EINVAL, "zero block allocation requested");
        }

        // Missing branch levels correspond to indirect metadata blocks that
        // must be allocated before any data block can be linked into the tree.
        let indirect_blks = (path.depth - 1 - branch.partial_level) as u32;
        // Never allocate past the current leaf boundary even if the caller asks
        // for a larger contiguous run.
        let max_data_blks = Self::max_blocks_in_run(path, max_blocks);
        if indirect_blks > 0 {
            // If the branch is incomplete, the fresh leaf starts empty, so we
            // can request the whole bounded data run immediately.
            return Ok((indirect_blks, max_data_blks));
        }

        // The metadata path already exists, so only count how many consecutive
        // empty slots remain in the current leaf.
        let start_slot = path.offsets[path.depth - 1] as usize;
        let mut count = 1u32;
        if path.depth == 1 {
            // Direct blocks live inline in the inode's `i_block[]`.
            while count < max_data_blks {
                let slot = start_slot
                    .checked_add(count as usize)
                    .ok_or_else(|| Error::with_message(Errno::EIO, "direct slot overflow"))?;
                let Some(&next_bid) = self.raw_block_ptrs.block_ptrs.get(slot) else {
                    break;
                };
                if next_bid != 0 {
                    break;
                }
                count += 1;
            }
        } else {
            // Indirect cases use the final indirect block as the allocation leaf.
            let leaf_bid = branch
                .chain
                .get(path.depth - 1)
                .and_then(|entry| entry.bh)
                .ok_or_else(|| Error::with_message(Errno::EIO, "missing indirect leaf block"))?;
            let mut indirect_blocks = self.indirect_blocks_manager.lock();
            let leaf_block = indirect_blocks.find(leaf_bid)?;
            while count < max_data_blks {
                let slot = start_slot
                    .checked_add(count as usize)
                    .ok_or_else(|| Error::with_message(Errno::EIO, "indirect slot overflow"))?;
                if leaf_block.read_bid(slot)? != 0 {
                    break;
                }
                count += 1;
            }
        }

        Ok((0, count))
    }

    fn write_data_range_to_direct_slots(
        &mut self,
        start_slot: usize,
        data_range: &Range<Ext2Bid>,
    ) -> Result<()> {
        for (offset, _) in data_range.clone().enumerate() {
            let slot = start_slot
                .checked_add(offset)
                .ok_or_else(|| Error::with_message(Errno::EIO, "direct slot overflow"))?;
            let entry = self
                .raw_block_ptrs
                .block_ptrs
                .get(slot)
                .ok_or_else(|| Error::with_message(Errno::EIO, "direct slot out of bounds"))?;
            if *entry != 0 {
                return_errno_with_message!(Errno::EIO, "block pointer changed during allocation");
            }
        }

        for (offset, bid) in data_range.clone().enumerate() {
            let slot = start_slot
                .checked_add(offset)
                .ok_or_else(|| Error::with_message(Errno::EIO, "direct slot overflow"))?;
            let entry = self
                .raw_block_ptrs
                .block_ptrs
                .get_mut(slot)
                .ok_or_else(|| Error::with_message(Errno::EIO, "direct slot out of bounds"))?;
            *entry = bid;
        }

        Ok(())
    }

    fn write_data_range_to_indirect_block(
        block: &mut IndirectBlock,
        start_slot: usize,
        data_range: &Range<Ext2Bid>,
    ) -> Result<()> {
        for (offset, _) in data_range.clone().enumerate() {
            let slot = start_slot
                .checked_add(offset)
                .ok_or_else(|| Error::with_message(Errno::EIO, "indirect slot overflow"))?;
            if block.read_bid(slot)? != 0 {
                return_errno_with_message!(Errno::EIO, "block pointer changed during allocation");
            }
        }

        for (offset, bid) in data_range.clone().enumerate() {
            let slot = start_slot
                .checked_add(offset)
                .ok_or_else(|| Error::with_message(Errno::EIO, "indirect slot overflow"))?;
            block.write_bid(slot, bid)?;
        }

        Ok(())
    }

    pub(super) fn free_branches(&mut self, fs: &Ext2, block_nr: Ext2Bid, depth: u32) {
        if block_nr == 0 {
            return;
        }

        let block_size = fs.block_size();
        let sectors_per_block = (block_size / SECTOR_SIZE) as u32;
        if sectors_per_block == 0 {
            error!(
                "ext2: free_branches: invalid sector accounting for block size {}",
                block_size
            );
            return;
        }

        if depth == 0 {
            if let Err(err) = fs.free_blocks(block_nr, 1) {
                // Best-effort free path logs errors and proceeds.
                error!(
                    "ext2: free_branches: failed to free data block {}: {:?}",
                    block_nr, err
                );
                return;
            }
            self.raw_block_ptrs.sector_count = self
                .raw_block_ptrs
                .sector_count
                .saturating_sub(sectors_per_block);
            return;
        }

        let ptrs_per_block = block_size / size_of::<u32>();

        let child_blocks = {
            let mut indirect_blocks = self.indirect_blocks_manager.lock();
            let child_blocks = {
                let block = match indirect_blocks.find(block_nr) {
                    Ok(block) => block,
                    Err(_) => {
                        // Skip the damaged branch after logging the read failure so
                        // cleanup can continue for the remaining subtree.
                        error!(
                            "ext2: free_branches: failed to read indirect block {} (depth {})",
                            block_nr, depth
                        );
                        return;
                    }
                };

                let mut child_blocks = Vec::new();
                for idx in 0..ptrs_per_block {
                    match block.read_bid(idx) {
                        Ok(0) => continue,
                        Ok(nr) => child_blocks.push(nr),
                        Err(_) => break,
                    }
                }
                child_blocks
            };
            indirect_blocks.remove(block_nr);
            child_blocks
        };

        for nr in child_blocks {
            self.free_branches(fs, nr, depth - 1);
        }

        if let Err(err) = fs.free_blocks(block_nr, 1) {
            error!(
                "ext2: free_branches: failed to free indirect block {}: {:?}",
                block_nr, err
            );
            return;
        }
        self.raw_block_ptrs.sector_count = self
            .raw_block_ptrs
            .sector_count
            .saturating_sub(sectors_per_block);
    }

    fn allocate_blocks (
        &self,
        fs: &Arc<Ext2>,
        indirect_blks: u32,
        data_blks: u32,
        path: &BlockPointerPath,
        branch: &BranchChainWalkResult,
    ) -> Result<BlockAllocGuard> {
        if data_blks == 0 {
            return_errno_with_message!(Errno::EIO, "invalid zero data allocation");
        }
        if branch.partial_level >= path.depth {
            return_errno_with_message!(Errno::EIO, "branch is already complete");
        }

        if fs.block_size() < size_of::<u32>() {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        // Prefer allocating near the existing branch tip so newly added
        // metadata/data blocks are likely to stay close on disk.
        let alloc_goal = branch
            .chain
            .last()
            .map_or(self.raw_block_ptrs.block_ptrs[0], |entry| entry.key)
            .max(fs.super_block().first_data_block());

        let mut guard = BlockAllocGuard::new(fs.clone());
        // Allocate the missing indirect metadata first, then place data blocks
        // immediately after the metadata run when possible.
                let mut indirect_blocks = Vec::with_capacity(indirect_blks as usize);
        let mut alloc_goal = alloc_goal.max(fs.super_block().first_data_block());

        while (indirect_blocks.len() as u32) < indirect_blks {
            let remain = indirect_blks - indirect_blocks.len() as u32;
            let allocated = fs.alloc_blocks(remain, alloc_goal)?;
            let alloc_len = allocated.end.saturating_sub(allocated.start);
            if alloc_len == 0 || alloc_len > remain {
                return_errno_with_message!(Errno::EIO, "invalid metadata allocation result");
            }

            indirect_blocks.extend(allocated.clone());
            alloc_goal = allocated.end;
        }

        let data_goal = indirect_blocks
            .last()
            .copied()
            .map_or(alloc_goal, |last_metadata| last_metadata.saturating_add(1));
        guard.track_indirect_blocks(indirect_blocks);

        let data_blocks_range = fs.alloc_blocks(data_blks, data_goal)?;
        let alloc_len = data_blocks_range.end.saturating_sub(data_blocks_range.start);
        guard.track_data_blocks(data_blocks_range);

        if alloc_len == 0 || alloc_len > data_blks {
            return_errno_with_message!(Errno::EIO, "invalid data allocation result");
        }

        Ok(guard)
    }

    fn splice_branch(
        &mut self,
        fs: &Ext2,
        guard: &BlockAllocGuard,
        path: &BlockPointerPath,
        branch: &BranchChainWalkResult,
    ) -> Result<()> {

        let indirect_blocks = &guard.indirect_blocks;
        let data_blocks = &guard.data_blocks;

        let block_size = fs.block_size();
        let sectors_per_block = (block_size / SECTOR_SIZE) as u32;

        let total = (indirect_blocks.len() as u32)
            .checked_add(data_blocks.end.saturating_sub(data_blocks.start))
            .ok_or_else(|| Error::with_message(Errno::EIO, "block allocation count overflow"))?;
        let added_sectors = total
            .checked_mul(sectors_per_block)
            .ok_or_else(|| Error::with_message(Errno::EIO, "inode block accounting overflow"))?;
        let new_block_count = self
            .raw_block_ptrs
            .sector_count
            .checked_add(added_sectors)
            .ok_or_else(|| Error::with_message(Errno::EIO, "inode block count overflow"))?;

        if indirect_blocks.is_empty() {
            // The branch already exists; only fill data pointers into existing slots.
            if branch.partial_level == 0 {
                let slot = path.offsets[0] as usize;
                self.write_data_range_to_direct_slots(slot, &data_blocks)?;
            } else {
                let parent_bid = branch
                    .chain
                    .get(branch.partial_level)
                    .and_then(|entry| entry.bh)
                    .ok_or_else(|| {
                        Error::with_message(
                            Errno::EIO,
                            "missing parent indirect block for data splice",
                        )
                    })?;
                let slot = path.offsets[branch.partial_level] as usize;
                let mut mgr = self.indirect_blocks_manager.lock();
                let parent_block = mgr.find_mut(parent_bid)?;
                Self::write_data_range_to_indirect_block(parent_block, slot, &data_blocks)?;
            }
        } else {
            let splice_ptr = indirect_blocks[0];
            let mut mgr = self.indirect_blocks_manager.lock();

            let result = (|| -> Result<()> {
                // Build the indirect blocks chain: each block points to the next;
                // the leaf block points to the allocated data range.
                for (i, new_bid) in indirect_blocks.iter().copied().enumerate() {
                    let level = branch.partial_level + 1 + i;
                    if level >= path.depth {
                        return_errno_with_message!(
                            Errno::EIO,
                            "invalid branch depth during allocation"
                        );
                    }

                    let mut block = IndirectBlock::alloc_new(new_bid)?;
                    block.clear();
                    let start_slot = path.offsets[level] as usize;
                    if i + 1 == indirect_blocks.len() {
                        Self::write_data_range_to_indirect_block(
                            &mut block,
                            start_slot,
                            &data_blocks,
                        )?;
                    } else {
                        block.write_bid(start_slot, indirect_blocks[i + 1])?;
                    }
                    mgr.insert_new(new_bid, block)?;
                }

                // Splice the chain root into the parent pointer.
                if branch.partial_level == 0 {
                    let slot = path.offsets[0] as usize;
                    if slot >= self.raw_block_ptrs.block_ptrs.len() {
                        return_errno_with_message!(
                            Errno::EIO,
                            "invalid inode block pointer slot"
                        );
                    }
                    if self.raw_block_ptrs.block_ptrs[slot] != 0 {
                        return_errno_with_message!(
                            Errno::EIO,
                            "block pointer changed during allocation"
                        );
                    }
                    self.raw_block_ptrs.block_ptrs[slot] = splice_ptr;
                } else {
                    let parent_bid = branch
                        .chain
                        .get(branch.partial_level)
                        .and_then(|entry| entry.bh)
                        .ok_or_else(|| {
                            Error::with_message(
                                Errno::EIO,
                                "missing parent indirect block for splice",
                            )
                        })?;
                    let parent_block = mgr.find_mut(parent_bid)?;
                    let slot = path.offsets[branch.partial_level] as usize;
                    if parent_block.read_bid(slot)? != 0 {
                        return_errno_with_message!(
                            Errno::EIO,
                            "block pointer changed during allocation"
                        );
                    }
                    parent_block.write_bid(slot, splice_ptr)?;
                }

                Ok(())
            })();

            if let Err(err) = result {
                // Evict any indirect blocks already inserted into the cache.
                // remove() is a no-op for non-existent keys, so sweep all.
                for &bid in indirect_blocks.iter() {
                    mgr.remove(bid);
                }
                // AllocGuard::drop will free the disk blocks.
                return Err(err);
            }
        }

        self.raw_block_ptrs.sector_count = new_block_count;
        Ok(())
    }

    /// Resolves a logical block to a contiguous physical block range.
    ///
    pub(super) fn lookup_block_range(
        &self,
        fs: &Ext2,
        iblock: u32,
        max_blocks: u32,
    ) -> Result<Range<Ext2Bid>> {
        if max_blocks == 0 {
            return_errno_with_message!(Errno::EINVAL, "zero block range requested");
        }

        let path = self.logical_block_to_path(fs, iblock)?;
        if path.depth == 0 {
            return Ok(0..0);
        }

        let branch = self.walk_block_chain(&path, fs)?;
        self.mapped_range_from_branch(&path, &branch, max_blocks)
    }

    /// Resolves a logical block to physical block (read-only).
    ///
    pub(super) fn lookup_block(&self, fs: &Ext2, iblock: u32) -> Result<Option<Ext2Bid>> {
        let range = self.lookup_block_range(fs, iblock, 1)?;
        Ok(if range.is_empty() {
            None
        } else {
            Some(range.start)
        })
    }

    /// Resolves a logical block to a contiguous physical block range, allocating if needed.
    ///
    pub(super) fn lookup_or_alloc_block_range(
        &mut self,
        fs: &Arc<Ext2>,
        iblock: u32,
        max_blocks: u32,
        create: bool,
    ) -> Result<Range<Ext2Bid>> {
        if max_blocks == 0 {
            return_errno_with_message!(Errno::EINVAL, "zero block allocation requested");
        }

        // Convert the logical block into a direct/indirect traversal path.
        let path = self.logical_block_to_path(fs, iblock)?;
        if path.depth == 0 {
            return_errno_with_message!(Errno::EIO, "invalid block path depth");
        }

        // Walk the existing branch until we either reach the target data slot or
        // stop at the first hole in the pointer chain.
        let branch = self.walk_block_chain(&path, fs)?;
        if branch.partial_level == path.depth {
            // The full mapping already exists, so only report the contiguous run.
            return self.mapped_range_from_branch(&path, &branch, max_blocks);
        }
        if !create {
            return Ok(0..0);
        }

        // Allocate the missing indirect metadata blocks, plus as many contiguous
        // data blocks as we can place into the current leaf run.
        let (indirect_blks, data_blks) = self.blks_to_allocate(&branch, &path, max_blocks)?;

        let mut guard = self.allocate_blocks(fs, indirect_blks, data_blks, &path, &branch)?;
        self.splice_branch(fs, &guard, &path, &branch)?;
        guard.commit();
        Ok(guard.data_blocks.clone())
    }

    /// Truncates all blocks beyond `new_size`.
    ///
    pub(super) fn truncate_blocks(&mut self, fs: &Ext2, new_size: usize) -> Result<()> {
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }
        let sectors_per_block = (block_size / SECTOR_SIZE) as u32;
        if sectors_per_block == 0 {
            return_errno_with_message!(Errno::EIO, "invalid sector accounting for block size");
        }

        // First logical block to free = ceil(new_size / block_size).
        let iblock = u32::try_from(new_size.div_ceil(block_size))
            .map_err(|_| Error::with_message(Errno::EINVAL, "truncate size exceeds ext2 limits"))?;

        // Convert logical block number to access path (depth + offsets).
        let path = self.logical_block_to_path(fs, iblock)?;
        if path.depth == 0 {
            return Ok(());
        }

        let ptrs_per_block = block_size / size_of::<u32>();
        if ptrs_per_block == 0 {
            return_errno_with_message!(Errno::EIO, "invalid indirect pointer fanout");
        }

        // === Case 1: Direct blocks only ===
        if path.depth == 1 {
            // Free direct blocks from `offsets[0]` through `block_ptrs[11]`.
            let start = (path.offsets[0] as usize).min(12);
            for idx in start..12 {
                let ptr = self.raw_block_ptrs.block_ptrs[idx];
                if ptr == 0 {
                    continue;
                }
                fs.free_blocks(ptr, 1)?;
                self.raw_block_ptrs.block_ptrs[idx] = 0;
                self.raw_block_ptrs.sector_count = self
                    .raw_block_ptrs
                    .sector_count
                    .saturating_sub(sectors_per_block);
            }
        } else {
            // === Case 2: Indirect blocks ===

            // --- Step 1: Adjust depth for boundary case ---
            // Ext2_find_shared-style partial branch handling.
            // If truncation point is at the start of an indirect block (offset = 0),
            // we can handle it at a higher level without reading that indirect block.
            let mut k = path.depth;
            while k > 1 && path.offsets[k - 1] == 0 {
                k -= 1;
            }
            let shared_path = BlockPointerPath {
                depth: k,
                offsets: path.offsets,
                boundary: path.boundary,
            };

            // --- Step 2: Read indirect block chain ---
            // branch.chain[i] contains the i-th level indirect block.
            // branch.partial_level indicates how deep we successfully read.
            let branch = self.walk_block_chain(&shared_path, fs)?;
            // `partial` is an index into `branch.chain[]`, pointing to the
            // deepest level from which we start detaching and freeing blocks.
            // If the full chain was read (partial_level == k), the last valid
            // index is k-1; otherwise partial_level already is the index where
            // traversal stopped on a zero pointer.
            // The all_zeroes loop below may shrink `partial` upward.
            let mut partial = if branch.partial_level == k {
                k - 1
            } else {
                branch.partial_level
            };

            // --- Step 3: all_zeroes optimization ---
            // Walk upward to the highest indirect block that can be fully
            // detached when the preserved left side is all zeros.
            // If the left side (to be kept) of an indirect block is all zeros,
            // we can free the entire indirect block and handle it at a higher level.
            while partial > 0 {
                let current_bid = branch
                    .chain
                    .get(partial)
                    .and_then(|entry| entry.bh)
                    .ok_or_else(|| {
                        Error::with_message(
                            Errno::EIO,
                            "missing indirect block for all-zeroes check",
                        )
                    })?;

                let all_zero = {
                    let mut indirect_blocks = self.indirect_blocks_manager.lock();
                    let block = indirect_blocks.find(current_bid)?;
                    let keep_entries = path.offsets[partial] as usize;
                    let mut all_zero = true;
                    for idx in 0..keep_entries {
                        if block.read_bid(idx)? != 0 {
                            all_zero = false;
                            break;
                        }
                    }
                    all_zero
                };
                if !all_zero {
                    break;
                }

                // Left side is all zeros, move up one level.
                partial -= 1;
            }

            // --- Step 4: Detach subtree root ---
            // Disconnect the pointer at offsets[partial] and get the subtree root block number.
            let detached_nr;
            if partial == 0 {
                // Detach from inode.block_ptrs directly.
                let slot = path.offsets[0] as usize;
                if slot >= self.raw_block_ptrs.block_ptrs.len() {
                    return_errno_with_message!(Errno::EIO, "inode block pointer slot out of range");
                }
                detached_nr = self.raw_block_ptrs.block_ptrs[slot];
                self.raw_block_ptrs.block_ptrs[slot] = 0;
            } else {
                let parent_bid = branch
                    .chain
                    .get(partial)
                    .and_then(|entry| entry.bh)
                    .ok_or_else(|| {
                        Error::with_message(Errno::EIO, "missing parent indirect block")
                    })?;
                let slot = path.offsets[partial] as usize;
                let mut indirect_blocks = self.indirect_blocks_manager.lock();
                let parent_block = indirect_blocks.find_mut(parent_bid)?;
                detached_nr = parent_block.read_bid(slot)?;
                parent_block.write_bid(slot, 0)?;
            }

            // Recursively free the detached subtree.
            if detached_nr != 0 {
                // Free detached subtree root.
                let subtree_depth = (path.depth - 1 - partial) as u32;
                self.free_branches(fs, detached_nr, subtree_depth);
            }

            // --- Step 5: Clear right side of partially shared indirect blocks ---
            // Clear right side of each partially shared indirect block.
            // For each level from partial down to 1, free all pointers to the right
            // of offsets[level].
            for level in (1..=partial).rev() {
                let current_bid = branch
                    .chain
                    .get(level)
                    .and_then(|entry| entry.bh)
                    .ok_or_else(|| {
                        Error::with_message(Errno::EIO, "invalid indirect block number on tail")
                    })?;

                let start_idx = (path.offsets[level] as usize) + 1;
                let child_depth = (path.depth - 1 - level) as u32;
                let child_blocks = {
                    let mut indirect_blocks = self.indirect_blocks_manager.lock();
                    let block = indirect_blocks.find_mut(current_bid)?;
                    let mut child_blocks = Vec::new();
                    for idx in start_idx..ptrs_per_block {
                        let nr = block.read_bid(idx)?;
                        if nr == 0 {
                            continue;
                        }
                        block.write_bid(idx, 0)?;
                        child_blocks.push(nr);
                    }
                    child_blocks
                };
                for nr in child_blocks {
                    self.free_branches(fs, nr, child_depth);
                }
            }
        }

        // === Step 6: Free complete indirect block trees ===
        // If truncation point is in direct blocks, free all indirect trees.
        // If in single indirect, free double and triple indirect trees, etc.
        if path.offsets[0] < 12 {
            // Truncation in direct blocks: free single, double, triple indirect.
            let nr = self.raw_block_ptrs.block_ptrs[12];
            if nr != 0 {
                self.raw_block_ptrs.block_ptrs[12] = 0;
                self.free_branches(fs, nr, 1);
            }
        }
        if path.offsets[0] <= 12 {
            // Truncation in direct or single indirect: free double, triple indirect.
            let nr = self.raw_block_ptrs.block_ptrs[13];
            if nr != 0 {
                self.raw_block_ptrs.block_ptrs[13] = 0;
                self.free_branches(fs, nr, 2);
            }
        }
        if path.offsets[0] <= 13 {
            // Truncation in direct, single, or double indirect: free triple indirect.
            let nr = self.raw_block_ptrs.block_ptrs[14];
            if nr != 0 {
                self.raw_block_ptrs.block_ptrs[14] = 0;
                self.free_branches(fs, nr, 3);
            }
        }

        Ok(())
    }
}

#[cfg(ktest)]
mod test {
    use core::mem::size_of;

    use ostd::{mm::VmIo, prelude::ktest};

    use super::*;
    use crate::{
        fs::{
            fs_impls::ext2::testkit::{self, ErrorBioDisk, Ext2FixtureBuilder, write_indirect_ptr},
            utils::IdBitmap,
        },
        prelude::*,
    };

    fn alloc_single_block(
        tree: &mut BlockPtrTree,
        fs: &Arc<Ext2>,
        iblock: u32,
    ) -> Result<Ext2Bid> {
        let range = tree.lookup_or_alloc_block_range(fs, iblock, 1, true)?;
        assert!(!range.is_empty());
        Ok(range.start)
    }

    fn make_block_map(block_ptrs: [u32; 15], sector_count: u32, fs: &Arc<Ext2>) -> BlockPtrTree {
        BlockPtrTree::new(
            RawBlockPtrs::from_parts(sector_count, block_ptrs),
            Arc::downgrade(fs),
        )
    }

    fn reload_group0_cached_bitmaps_from_disk(f: &testkit::Ext2Fixture) {
        let group = f.ext2.block_group(0);

        let block_bitmap_bid = group.block_bitmap_bid();
        let inode_bitmap_bid = group.inode_bitmap_bid();

        let mut block_bitmap_buf = vec![0u8; BLOCK_SIZE];
        f.disk
            .segment()
            .read_bytes(
                Bid::new(block_bitmap_bid as u64).to_offset(),
                &mut block_bitmap_buf,
            )
            .unwrap();

        let mut inode_bitmap_buf = vec![0u8; BLOCK_SIZE];
        f.disk
            .segment()
            .read_bytes(
                Bid::new(inode_bitmap_bid as u64).to_offset(),
                &mut inode_bitmap_buf,
            )
            .unwrap();

        let mut metadata = group.metadata_mut();
        let block_len = metadata.block_bitmap.len();
        *metadata.block_bitmap = IdBitmap::from_buf(block_bitmap_buf.into_boxed_slice(), block_len);
        let inode_len = metadata.inode_bitmap.len();
        *metadata.inode_bitmap = IdBitmap::from_buf(inode_bitmap_buf.into_boxed_slice(), inode_len);
    }

    #[ktest]
    fn block_map_direct_and_indirect_ok() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let (disk, ext2) = (&f.disk, &f.ext2);

        let ptrs = (ext2.block_size() / size_of::<u32>()) as u32;
        let ptrs_bits = ptrs.trailing_zeros();
        let double_blocks = 1u32 << (ptrs_bits * 2);

        let indirect_bid = 40u32;
        let indirect_index = 5u32;
        let mapped_bid = 77u32;
        write_indirect_ptr(disk.as_ref(), indirect_bid, indirect_index, mapped_bid);

        let double_l1_bid = 41u32;
        let double_l2_bid = 42u32;
        let double_data_bid = 78u32;
        write_indirect_ptr(disk.as_ref(), double_l1_bid, 3, double_l2_bid);
        write_indirect_ptr(disk.as_ref(), double_l2_bid, 4, double_data_bid);

        let triple_l1_bid = 43u32;
        let triple_l2_bid = 44u32;
        let triple_l3_bid = 45u32;
        let triple_data_bid = 79u32;
        write_indirect_ptr(disk.as_ref(), triple_l1_bid, 2, triple_l2_bid);
        write_indirect_ptr(disk.as_ref(), triple_l2_bid, 3, triple_l3_bid);
        write_indirect_ptr(disk.as_ref(), triple_l3_bid, 4, triple_data_bid);

        let mut block_ptrs = [0u32; 15];
        block_ptrs[0] = 11;
        block_ptrs[12] = indirect_bid;
        block_ptrs[13] = double_l1_bid;
        block_ptrs[14] = triple_l1_bid;
        let block_ptr_tree = make_block_map(block_ptrs, 0, &f.ext2);

        // Cover exact transition boundaries across all block-map levels.
        let direct_path = block_ptr_tree.logical_block_to_path(ext2, 0).unwrap();
        assert_eq!(direct_path.depth, 1);
        assert_eq!(direct_path.offsets[0], 0);
        assert_eq!(direct_path.boundary, 11);

        let direct_last_path = block_ptr_tree.logical_block_to_path(ext2, 11).unwrap();
        assert_eq!(direct_last_path.depth, 1);
        assert_eq!(direct_last_path.offsets[0], 11);
        assert_eq!(direct_last_path.boundary, 0);

        let indirect_first_path = block_ptr_tree.logical_block_to_path(ext2, 12).unwrap();
        assert_eq!(indirect_first_path.depth, 2);
        assert_eq!(indirect_first_path.offsets[0], 12);
        assert_eq!(indirect_first_path.offsets[1], 0);

        let indirect_path = block_ptr_tree
            .logical_block_to_path(ext2, 12 + indirect_index)
            .unwrap();
        assert_eq!(indirect_path.depth, 2);
        assert_eq!(indirect_path.offsets[0], 12);
        assert_eq!(indirect_path.offsets[1], indirect_index);

        let indirect_last_iblock = 12 + ptrs - 1;
        let indirect_last_path = block_ptr_tree
            .logical_block_to_path(ext2, indirect_last_iblock)
            .unwrap();
        assert_eq!(indirect_last_path.depth, 2);
        assert_eq!(indirect_last_path.offsets[0], 12);
        assert_eq!(indirect_last_path.offsets[1], ptrs - 1);
        assert_eq!(indirect_last_path.boundary, 0);

        let first_double_iblock = 12 + ptrs;
        let first_double_path = block_ptr_tree
            .logical_block_to_path(ext2, first_double_iblock)
            .unwrap();
        assert_eq!(first_double_path.depth, 3);
        assert_eq!(first_double_path.offsets[0], 13);
        assert_eq!(first_double_path.offsets[1], 0);
        assert_eq!(first_double_path.offsets[2], 0);

        let double_iblock = 12 + ptrs + (3 << ptrs_bits) + 4;
        let double_path = block_ptr_tree
            .logical_block_to_path(ext2, double_iblock)
            .unwrap();
        assert_eq!(double_path.depth, 3);
        assert_eq!(double_path.offsets[0], 13);
        assert_eq!(double_path.offsets[1], 3);
        assert_eq!(double_path.offsets[2], 4);

        let first_triple_iblock = 12 + ptrs + double_blocks;
        let first_triple_path = block_ptr_tree
            .logical_block_to_path(ext2, first_triple_iblock)
            .unwrap();
        assert_eq!(first_triple_path.depth, 4);
        assert_eq!(first_triple_path.offsets[0], 14);
        assert_eq!(first_triple_path.offsets[1], 0);
        assert_eq!(first_triple_path.offsets[2], 0);
        assert_eq!(first_triple_path.offsets[3], 0);

        let triple_iblock =
            12 + ptrs + double_blocks + (2 << (ptrs_bits * 2)) + (3 << ptrs_bits) + 4;
        let triple_path = block_ptr_tree
            .logical_block_to_path(ext2, triple_iblock)
            .unwrap();
        assert_eq!(triple_path.depth, 4);
        assert_eq!(triple_path.offsets[0], 14);
        assert_eq!(triple_path.offsets[1], 2);
        assert_eq!(triple_path.offsets[2], 3);
        assert_eq!(triple_path.offsets[3], 4);

        // Verify block lookup resolves direct/indirect/double/triple chains.
        assert_eq!(block_ptr_tree.lookup_block(ext2, 0).unwrap(), Some(11));
        assert_eq!(block_ptr_tree.lookup_block(ext2, 1).unwrap(), None);
        assert_eq!(
            block_ptr_tree
                .lookup_block(ext2, 12 + indirect_index)
                .unwrap(),
            Some(mapped_bid)
        );
        assert_eq!(
            block_ptr_tree.lookup_block(ext2, double_iblock).unwrap(),
            Some(double_data_bid)
        );
        assert_eq!(
            block_ptr_tree.lookup_block(ext2, triple_iblock).unwrap(),
            Some(triple_data_bid)
        );
    }

    #[ktest]
    fn block_map_get_block_range_returns_contiguous_runs() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let (disk, ext2) = (&f.disk, &f.ext2);

        let indirect_bid = 40u32;
        write_indirect_ptr(disk.as_ref(), indirect_bid, 0, 70);
        write_indirect_ptr(disk.as_ref(), indirect_bid, 1, 71);
        write_indirect_ptr(disk.as_ref(), indirect_bid, 2, 72);
        write_indirect_ptr(disk.as_ref(), indirect_bid, 3, 90);

        let mut block_ptrs = [0u32; 15];
        block_ptrs[0] = 11;
        block_ptrs[1] = 12;
        block_ptrs[2] = 13;
        block_ptrs[12] = indirect_bid;
        let block_ptr_tree = make_block_map(block_ptrs, 0, &f.ext2);

        assert_eq!(
            block_ptr_tree.lookup_block_range(ext2, 0, 4).unwrap(),
            11..14
        );
        assert_eq!(block_ptr_tree.lookup_block(ext2, 0).unwrap(), Some(11));
        assert_eq!(
            block_ptr_tree.lookup_block_range(ext2, 12, 4).unwrap(),
            70..73
        );
        assert_eq!(
            block_ptr_tree.lookup_block_range(ext2, 15, 4).unwrap(),
            90..91
        );
        assert!(block_ptr_tree.lookup_block_range(ext2, 3, 4).unwrap().is_empty());
    }

    #[ktest]
    fn block_map_invalid_depth_returns_err() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let (disk, ext2) = (&f.disk, &f.ext2);

        let ptrs = (ext2.block_size() / size_of::<u32>()) as u64;
        let direct = 12u64;
        let indirect = ptrs;
        let double_blocks = 1u64 << (ptrs.trailing_zeros() * 2);
        let triple_blocks = 1u64 << (ptrs.trailing_zeros() * 3);
        let max_iblock = direct + indirect + double_blocks + triple_blocks - 1;
        let too_big = (max_iblock + 1) as u32;

        let block_ptr_tree = make_block_map([0; 15], 0, &f.ext2);

        // Accept the maximum valid logical block and reject the next one.
        block_ptr_tree
            .logical_block_to_path(ext2, max_iblock as u32)
            .unwrap();

        let too_big_err = block_ptr_tree
            .logical_block_to_path(ext2, too_big)
            .unwrap_err();
        assert_eq!(too_big_err.error(), Errno::EINVAL);

        let get_too_big_err = block_ptr_tree.lookup_block(ext2, too_big).unwrap_err();
        assert_eq!(get_too_big_err.error(), Errno::EINVAL);

        // Inject a deterministic read failure on the indirect block read path.
        let io_base = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let io_fail_offset = Bid::new(40).to_offset();
        let io_disk = Arc::new(ErrorBioDisk::with_read_error_at(
            io_base.disk.clone(),
            BioStatus::IoError,
            io_fail_offset,
        ));
        let io_f = Ext2FixtureBuilder::new(2, 256)
            .with_device(io_disk)
            .build()
            .unwrap();
        let io_ext2 = &io_f.ext2;

        let mut ptrs_for_io = [0u32; 15];
        ptrs_for_io[12] = 40;
        let io_block_map = make_block_map(ptrs_for_io, 0, &io_f.ext2);
        let io_err = io_block_map.lookup_block(io_ext2, 12).unwrap_err();
        assert_eq!(io_err.error(), Errno::EIO);

        // Any zero pointer on the branch is treated as a hole (None).
        let mut ptrs_for_indirect_hole = [0u32; 15];
        ptrs_for_indirect_hole[12] = 40;
        let indirect_hole_block_map = make_block_map(ptrs_for_indirect_hole, 0, &f.ext2);
        assert_eq!(
            indirect_hole_block_map.lookup_block(ext2, 12 + 7).unwrap(),
            None
        );

        let mut ptrs_for_double_hole = [0u32; 15];
        ptrs_for_double_hole[13] = 41;
        write_indirect_ptr(disk.as_ref(), 41, 3, 0);
        let double_hole_block_map = make_block_map(ptrs_for_double_hole, 0, &f.ext2);
        let double_hole_iblock = 12 + (ptrs as u32) + (3 << ptrs.trailing_zeros()) + 4;
        assert_eq!(
            double_hole_block_map
                .lookup_block(ext2, double_hole_iblock)
                .unwrap(),
            None
        );

        let mut ptrs_for_triple_hole = [0u32; 15];
        ptrs_for_triple_hole[14] = 43;
        write_indirect_ptr(disk.as_ref(), 43, 2, 44);
        write_indirect_ptr(disk.as_ref(), 44, 3, 0);
        let triple_hole_block_map = make_block_map(ptrs_for_triple_hole, 0, &f.ext2);
        let triple_hole_iblock = 12
            + (ptrs as u32)
            + (double_blocks as u32)
            + (2 << (ptrs.trailing_zeros() * 2))
            + (3 << ptrs.trailing_zeros())
            + 4;
        assert_eq!(
            triple_hole_block_map
                .lookup_block(ext2, triple_hole_iblock)
                .unwrap(),
            None
        );
    }

    #[ktest]
    fn block_alloc_direct_path_ok() {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let ext2 = &f.ext2;
        let sectors_per_block = (ext2.block_size() / SECTOR_SIZE) as u32;

        let mut block_ptr_tree = make_block_map([0u32; 15], 0, &f.ext2);
        assert!(
            block_ptr_tree
                .lookup_or_alloc_block_range(ext2, 0, 1, false)
                .unwrap()
                .is_empty()
        );
        assert_eq!(block_ptr_tree.raw_block_ptrs.block_ptrs[0], 0);

        let free_before = ext2.super_block().free_blocks_count();
        let allocated = alloc_single_block(&mut block_ptr_tree, ext2, 0).unwrap();
        let free_after = ext2.super_block().free_blocks_count();

        assert_eq!(block_ptr_tree.raw_block_ptrs.block_ptrs[0], allocated);
        assert_eq!(
            block_ptr_tree.lookup_block(ext2, 0).unwrap(),
            Some(allocated)
        );
        assert_eq!(
            block_ptr_tree.raw_block_ptrs.sector_count,
            sectors_per_block
        );
        assert_eq!(free_before.saturating_sub(free_after), 1);
    }

    #[ktest]
    fn block_alloc_direct_range_ok() {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let ext2 = &f.ext2;
        let sectors_per_block = (ext2.block_size() / SECTOR_SIZE) as u32;

        let mut block_ptr_tree = make_block_map([0u32; 15], 0, &f.ext2);
        let free_before = ext2.super_block().free_blocks_count();
        let allocated_range = block_ptr_tree
            .lookup_or_alloc_block_range(ext2, 0, 3, true)
            .unwrap();
        let free_after = ext2.super_block().free_blocks_count();

        assert_eq!(allocated_range.end - allocated_range.start, 3);
        assert_eq!(
            block_ptr_tree.raw_block_ptrs.block_ptrs[0],
            allocated_range.start
        );
        assert_eq!(
            block_ptr_tree.raw_block_ptrs.block_ptrs[1],
            allocated_range.start + 1
        );
        assert_eq!(
            block_ptr_tree.raw_block_ptrs.block_ptrs[2],
            allocated_range.start + 2
        );
        assert_eq!(
            block_ptr_tree.lookup_block_range(ext2, 0, 3).unwrap(),
            allocated_range.clone()
        );
        assert_eq!(
            block_ptr_tree
                .lookup_or_alloc_block_range(ext2, 0, 1, true)
                .unwrap()
                .start,
            allocated_range.start
        );
        assert_eq!(
            block_ptr_tree.raw_block_ptrs.sector_count,
            sectors_per_block.saturating_mul(3)
        );
        assert_eq!(free_before.saturating_sub(free_after), 3);
    }

    #[ktest]
    fn block_alloc_indirect_path_ok() {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let ext2 = &f.ext2;
        let sectors_per_block = (ext2.block_size() / SECTOR_SIZE) as u32;

        let mut block_ptr_tree = make_block_map([0u32; 15], 0, &f.ext2);
        let free_before = ext2.super_block().free_blocks_count();
        let allocated = alloc_single_block(&mut block_ptr_tree, ext2, 12).unwrap();
        let free_after = ext2.super_block().free_blocks_count();

        assert_ne!(block_ptr_tree.raw_block_ptrs.block_ptrs[12], 0);
        assert_eq!(
            block_ptr_tree.lookup_block(ext2, 12).unwrap(),
            Some(allocated)
        );
        assert_eq!(
            block_ptr_tree.raw_block_ptrs.sector_count,
            sectors_per_block.saturating_mul(2)
        );
        assert_eq!(free_before.saturating_sub(free_after), 2);
    }

    #[ktest]
    fn block_alloc_indirect_range_ok() {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let ext2 = &f.ext2;
        let sectors_per_block = (ext2.block_size() / SECTOR_SIZE) as u32;

        let mut block_ptr_tree = make_block_map([0u32; 15], 0, &f.ext2);
        let free_before = ext2.super_block().free_blocks_count();
        let allocated_range = block_ptr_tree
            .lookup_or_alloc_block_range(ext2, 12, 4, true)
            .unwrap();
        let free_after = ext2.super_block().free_blocks_count();

        assert_ne!(block_ptr_tree.raw_block_ptrs.block_ptrs[12], 0);
        assert_eq!(allocated_range.end - allocated_range.start, 4);
        assert_eq!(
            block_ptr_tree.lookup_block_range(ext2, 12, 4).unwrap(),
            allocated_range.clone()
        );
        assert_eq!(
            block_ptr_tree.lookup_block(ext2, 12).unwrap(),
            Some(allocated_range.start)
        );
        assert_eq!(
            block_ptr_tree.raw_block_ptrs.sector_count,
            sectors_per_block.saturating_mul(5)
        );
        assert_eq!(free_before.saturating_sub(free_after), 5);
    }

    #[ktest]
    fn block_alloc_enospc_preserves_inode_state() {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(0, 0)
            .with_filled_block_bitmap(true)
            .build()
            .unwrap();
        let ext2 = &f.ext2;

        let mut block_ptr_tree = make_block_map([0u32; 15], 0, &f.ext2);
        let err = block_ptr_tree
            .lookup_or_alloc_block_range(ext2, 0, 1, true)
            .unwrap_err();
        assert_eq!(err.error(), Errno::ENOSPC);
        assert_eq!(block_ptr_tree.raw_block_ptrs.block_ptrs, [0u32; 15]);
        assert_eq!(block_ptr_tree.raw_block_ptrs.sector_count, 0);
    }

    #[ktest]
    fn block_alloc_fragmented_chain_ok() {
        // Corner case: the required metadata chain exists only as fragmented free
        // blocks, so metadata allocation must be accumulated across multiple
        // fs.alloc_blocks() calls before the single data block is attached.
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(3, 3)
            .build()
            .unwrap();
        let ext2 = &f.ext2;
        let sb = &f.sb;
        let desc = &f.descs[0];

        let first = sb.group_first_block_no(0);
        let last = sb.group_last_block_no(0);
        let data_base = first
            .saturating_add(2)
            .saturating_add(sb.inode_table_blocks_per_group())
            .saturating_add(2);
        let free0 = data_base;
        let free1 = data_base.saturating_add(2);
        let free2 = data_base.saturating_add(4);
        assert!(free2 <= last);

        // Mark every block allocated except three isolated free blocks.
        let mut allocated_blocks = Vec::new();
        for block in first..=last {
            if block == free0 || block == free1 || block == free2 {
                continue;
            }
            allocated_blocks.push(block);
        }
        testkit::write_block_bitmap(f.disk.as_ref(), sb, desc, &allocated_blocks);
        reload_group0_cached_bitmaps_from_disk(&f);

        let ptrs = (ext2.block_size() / size_of::<u32>()) as u32;
        let first_double_iblock = 12 + ptrs;
        let mut block_ptr_tree = make_block_map([0u32; 15], 0, &f.ext2);

        let allocated_data =
            alloc_single_block(&mut block_ptr_tree, ext2, first_double_iblock).unwrap();
        assert_ne!(block_ptr_tree.raw_block_ptrs.block_ptrs[13], 0);
        assert_eq!(
            block_ptr_tree
                .lookup_block(ext2, first_double_iblock)
                .unwrap(),
            Some(allocated_data)
        );

        // All three isolated free blocks should be consumed.
        let block_size = ext2.block_size();
        assert_eq!(ext2.super_block().free_blocks_count(), 0);
        assert_eq!(f.ext2.block_group(0).free_blocks_count(), 0);
        assert_eq!(
            block_ptr_tree.raw_block_ptrs.sector_count,
            ((block_size / SECTOR_SIZE) as u32) * 3
        );
    }

    #[ktest]
    fn truncate_indirect_frees_shared_path() {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let ext2 = &f.ext2;
        let block_size = ext2.block_size();
        let ptrs = (block_size / size_of::<u32>()) as u32;
        let first_double_iblock = 12 + ptrs;

        let mut block_ptr_tree = make_block_map([0u32; 15], 0, &f.ext2);
        alloc_single_block(&mut block_ptr_tree, ext2, first_double_iblock).unwrap();
        alloc_single_block(&mut block_ptr_tree, ext2, first_double_iblock + 1).unwrap();
        alloc_single_block(&mut block_ptr_tree, ext2, first_double_iblock + 2).unwrap();

        block_ptr_tree
            .truncate_blocks(ext2, (first_double_iblock as usize + 1) * block_size)
            .unwrap();
        assert!(
            block_ptr_tree
                .lookup_block(ext2, first_double_iblock)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            block_ptr_tree
                .lookup_block(ext2, first_double_iblock + 1)
                .unwrap(),
            None
        );
        assert_eq!(
            block_ptr_tree
                .lookup_block(ext2, first_double_iblock + 2)
                .unwrap(),
            None
        );
    }

    #[ktest]
    fn truncate_releases_all_indirect_blocks() {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let ext2 = &f.ext2;
        let block_size = ext2.block_size();
        let ptrs = (block_size / size_of::<u32>()) as u32;
        let first_double_iblock = 12 + ptrs;
        let first_triple_iblock = 12 + ptrs + (1u32 << (ptrs.trailing_zeros() * 2));

        let mut block_ptr_tree = make_block_map([0u32; 15], 0, &f.ext2);
        alloc_single_block(&mut block_ptr_tree, ext2, 12).unwrap();
        alloc_single_block(&mut block_ptr_tree, ext2, first_double_iblock).unwrap();
        alloc_single_block(&mut block_ptr_tree, ext2, first_triple_iblock).unwrap();
        assert_ne!(block_ptr_tree.raw_block_ptrs.block_ptrs[12], 0);
        assert_ne!(block_ptr_tree.raw_block_ptrs.block_ptrs[13], 0);
        assert_ne!(block_ptr_tree.raw_block_ptrs.block_ptrs[14], 0);

        block_ptr_tree.truncate_blocks(ext2, 0).unwrap();
        assert_eq!(block_ptr_tree.raw_block_ptrs.block_ptrs[12], 0);
        assert_eq!(block_ptr_tree.raw_block_ptrs.block_ptrs[13], 0);
        assert_eq!(block_ptr_tree.raw_block_ptrs.block_ptrs[14], 0);
        assert_eq!(block_ptr_tree.lookup_block(ext2, 12).unwrap(), None);
        assert_eq!(
            block_ptr_tree
                .lookup_block(ext2, first_double_iblock)
                .unwrap(),
            None
        );
        assert_eq!(
            block_ptr_tree
                .lookup_block(ext2, first_triple_iblock)
                .unwrap(),
            None
        );
        assert_eq!(block_ptr_tree.raw_block_ptrs.sector_count, 0);
    }

    #[ktest]
    fn free_branches_recursively_releases_blocks() {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let ext2 = &f.ext2;
        let block_size = ext2.block_size();
        let sectors_per_block = (block_size / SECTOR_SIZE) as u32;
        let ptrs = (block_size / size_of::<u32>()) as u32;
        let first_triple_iblock = 12 + ptrs + (1u32 << (ptrs.trailing_zeros() * 2));

        let mut block_ptr_tree = make_block_map([0u32; 15], 0, &f.ext2);
        alloc_single_block(&mut block_ptr_tree, ext2, first_triple_iblock).unwrap();
        let root = block_ptr_tree.raw_block_ptrs.block_ptrs[14];
        assert_ne!(root, 0);
        assert_eq!(
            block_ptr_tree.raw_block_ptrs.sector_count,
            sectors_per_block.saturating_mul(4)
        );

        let free_before = ext2.super_block().free_blocks_count();
        block_ptr_tree.free_branches(ext2, root, 3);
        block_ptr_tree.raw_block_ptrs.block_ptrs[14] = 0;
        let free_after = ext2.super_block().free_blocks_count();

        assert_eq!(free_after.saturating_sub(free_before), 4);
        assert_eq!(block_ptr_tree.raw_block_ptrs.sector_count, 0);
    }
}
