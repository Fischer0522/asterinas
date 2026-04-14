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
/// Logical block index within a file (0-based).
pub(super) type Iblock = u32;

/// Offsets within indirect blocks, from outermost to innermost.
#[derive(Clone, Copy, Debug)]
enum IndirectOffsets {
    /// Direct block — no indirect layers.
    None,
    /// Single indirect: one offset into the indirect block.
    Single(u32),
    /// Double indirect: offsets into L1 and L2 indirect blocks.
    Double(u32, u32),
    /// Triple indirect: offsets into L1, L2, and L3 indirect blocks.
    Triple(u32, u32, u32),
}

impl IndirectOffsets {
    /// Returns the number of indirect levels (0 for direct).
    pub fn depth(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Single(_) => 1,
            Self::Double(..) => 2,
            Self::Triple(..) => 3,
        }
    }

    /// Returns whether this is a direct block path (no indirect levels).
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// Returns the offset at the given indirect level (0-indexed from outermost).
    ///
    /// # Panics
    ///
    /// Panics if `idx` is out of range for this variant.
    pub fn offset_at(&self, idx: usize) -> u32 {
        match (self, idx) {
            (Self::Single(a), 0) => *a,
            (Self::Double(a, _), 0) => *a,
            (Self::Double(_, b), 1) => *b,
            (Self::Triple(a, _, _), 0) => *a,
            (Self::Triple(_, b, _), 1) => *b,
            (Self::Triple(_, _, c), 2) => *c,
            _ => panic!(
                "indirect offset index {idx} out of range for depth {}",
                self.depth()
            ),
        }
    }

    /// Returns the innermost (leaf) indirect offset, or `None` for direct blocks.
    pub fn leaf_offset(&self) -> Option<u32> {
        match self {
            Self::None => Option::None,
            Self::Single(a) => Some(*a),
            Self::Double(_, b) => Some(*b),
            Self::Triple(_, _, c) => Some(*c),
        }
    }

    /// Trims trailing zero offsets from the innermost levels.
    ///
    /// Used by `truncate_blocks` to raise the shared-branch level when the
    /// truncation point sits at the start of an indirect block.
    pub fn trim_trailing_zeros(&self) -> Self {
        match *self {
            Self::None => Self::None,
            Self::Single(a) => {
                if a == 0 {
                    Self::None
                } else {
                    Self::Single(a)
                }
            }
            Self::Double(a, b) => {
                if b == 0 {
                    if a == 0 { Self::None } else { Self::Single(a) }
                } else {
                    Self::Double(a, b)
                }
            }
            Self::Triple(a, b, c) => {
                if c == 0 {
                    if b == 0 {
                        if a == 0 { Self::None } else { Self::Single(a) }
                    } else {
                        Self::Double(a, b)
                    }
                } else {
                    Self::Triple(a, b, c)
                }
            }
        }
    }
}

/// Block path for direct/indirect traversal.
///
/// Produced by `logical_block_to_path` from a logical block number.
#[derive(Clone, Copy, Debug)]
struct BlockPointerPath {
    /// Index into `inode.i_block[0..15]`. For direct blocks this is 0..12;
    /// for single/double/triple indirect it is 12/13/14.
    iblock_index: u32,
    /// Offsets within indirect blocks. `None` for direct blocks.
    indirect: IndirectOffsets,
    /// Number of consecutive block slots remaining after the current offset
    /// within the lowest-level block (direct region or indirect block).
    boundary: u32,
}

impl BlockPointerPath {
    /// Total depth of the pointer chain (1 = direct, 2..4 = indirect).
    pub fn depth(&self) -> usize {
        1 + self.indirect.depth()
    }

    /// Returns the offset at the given level of the block pointer tree.
    ///
    /// Level 0 returns `iblock_index`; levels 1.. index into `indirect`.
    pub fn offset_at(&self, level: usize) -> u32 {
        if level == 0 {
            self.iblock_index
        } else {
            self.indirect.offset_at(level - 1)
        }
    }

    /// Returns the offset at the deepest (leaf) level.
    ///
    /// For direct blocks this is `iblock_index`; for indirect blocks
    /// it is the innermost indirect offset.
    pub fn leaf_offset(&self) -> u32 {
        self.indirect.leaf_offset().unwrap_or(self.iblock_index)
    }
}

/// A single level in the block-pointer chain.
///
#[derive(Clone, Debug)]
struct IndirectBlockEntry {
    /// Physical block number read from this level's slot; 0 means hole.
    key: Ext2Bid,
    /// Block number of the parent indirect block that contains this level's slot.
    /// `None` for level 0, where the slot lives in inode `i_block[]`.
    parent_bid: Option<Ext2Bid>,
}

/// Result of traversing a block-pointer chain.
#[derive(Debug)]
struct BranchChainWalkResult {
    /// Level where traversal stopped on a zero pointer, or `path.depth()` if complete.
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
                error!(
                    "failed to free indirect block {} in rollback: {:?}",
                    bid, err
                );
            }
        }

        debug_assert!(self.data_blocks.end >= self.data_blocks.start);
        let data_count = self.data_blocks.end - self.data_blocks.start;
        if data_count > 0 {
            let free_result = self.fs.free_blocks(self.data_blocks.start, data_count);
            if let Err(err) = free_result {
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

    /// Resolves a logical block to a contiguous physical block range.
    ///
    pub(super) fn lookup_block_range(
        &self,
        iblock: Iblock,
        max_blocks: u32,
    ) -> Result<Range<Ext2Bid>> {
        if max_blocks == 0 {
            return_errno_with_message!(Errno::EINVAL, "zero block range requested");
        }

        let path = self.logical_block_to_path(iblock)?;
        let branch = self.walk_block_chain(&path)?;
        self.mapped_range_from_branch(&path, &branch, max_blocks)
    }

    /// Resolves a logical block to physical block (read-only).
    ///
    pub(super) fn lookup_block(&self, iblock: Iblock) -> Result<Option<Ext2Bid>> {
        let range = self.lookup_block_range(iblock, 1)?;
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
        iblock: Iblock,
        max_blocks: u32,
        create: bool,
    ) -> Result<Range<Ext2Bid>> {
        if max_blocks == 0 {
            return_errno_with_message!(Errno::EINVAL, "zero block allocation requested");
        }

        // Convert the logical block into a direct/indirect traversal path.
        let path = self.logical_block_to_path(iblock)?;

        // Walk the existing branch until we either reach the target data slot or
        // stop at the first hole in the pointer chain.
        let branch = self.walk_block_chain(&path)?;
        if branch.partial_level == path.depth() {
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
        self.splice_branch(&guard, &path, &branch)?;
        guard.commit();
        Ok(guard.data_blocks.clone())
    }

    /// Truncates all blocks beyond `new_size`.
    ///
    pub(super) fn truncate_blocks(&mut self, fs: &Ext2, new_size: usize) -> Result<()> {
        let sectors_per_block = (BLOCK_SIZE / SECTOR_SIZE) as u32;

        // First logical block to free = ceil(new_size / block_size).
        let iblock = Iblock::try_from(new_size.div_ceil(BLOCK_SIZE))
            .map_err(|_| Error::with_message(Errno::EINVAL, "truncate size exceeds ext2 limits"))?;

        // Convert logical block number to access path.
        let path = self.logical_block_to_path(iblock)?;

        let ptrs_per_block = BLOCK_SIZE / size_of::<u32>();
        if ptrs_per_block == 0 {
            return_errno_with_message!(Errno::EIO, "invalid indirect pointer fanout");
        }

        // === Case 1: Direct blocks only ===
        if path.indirect.is_none() {
            // Free direct blocks from `iblock_index` through `block_ptrs[11]`.
            let start = (path.iblock_index as usize).min(12);
            for idx in start..12 {
                let ptr = self.raw_block_ptrs.block_ptrs[idx];
                if ptr == 0 {
                    continue;
                }
                fs.free_blocks(ptr, 1)?;
                self.raw_block_ptrs.block_ptrs[idx] = 0;
                debug_assert!(self.raw_block_ptrs.sector_count >= sectors_per_block);
                self.raw_block_ptrs.sector_count -= sectors_per_block;
            }
        } else {
            // === Case 2: Indirect blocks ===

            // --- Step 1: Adjust depth for boundary case ---
            // Ext2_find_shared-style partial branch handling.
            // If truncation point is at the start of an indirect block (offset = 0),
            // we can handle it at a higher level without reading that indirect block.
            let shared_path = BlockPointerPath {
                iblock_index: path.iblock_index,
                indirect: path.indirect.trim_trailing_zeros(),
                boundary: path.boundary,
            };
            let k = shared_path.depth();

            // --- Step 2: Read indirect block chain ---
            // branch.chain[i] contains the i-th level indirect block.
            // branch.partial_level indicates how deep we successfully read.
            let branch = self.walk_block_chain(&shared_path)?;
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
                    .and_then(|entry| entry.parent_bid)
                    .ok_or_else(|| {
                        Error::with_message(
                            Errno::EIO,
                            "missing indirect block for all-zeroes check",
                        )
                    })?;

                let all_zero = {
                    let mut indirect_blocks = self.indirect_blocks_manager.lock();
                    let block = indirect_blocks.find(current_bid)?;
                    let keep_entries = path.offset_at(partial) as usize;
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
            // Disconnect the pointer at the partial level and get the subtree root block number.
            let detached_nr;
            if partial == 0 {
                // Detach from inode.block_ptrs directly.
                let slot = path.iblock_index as usize;
                if slot >= self.raw_block_ptrs.block_ptrs.len() {
                    return_errno_with_message!(Errno::EIO, "inode block pointer slot out of range");
                }
                detached_nr = self.raw_block_ptrs.block_ptrs[slot];
                self.raw_block_ptrs.block_ptrs[slot] = 0;
            } else {
                let parent_bid = branch
                    .chain
                    .get(partial)
                    .and_then(|entry| entry.parent_bid)
                    .ok_or_else(|| {
                        Error::with_message(Errno::EIO, "missing parent indirect block")
                    })?;
                let slot = path.offset_at(partial) as usize;
                let mut indirect_blocks = self.indirect_blocks_manager.lock();
                let parent_block = indirect_blocks.find_mut(parent_bid)?;
                detached_nr = parent_block.read_bid(slot)?;
                parent_block.write_bid(slot, 0)?;
            }

            // Recursively free the detached subtree.
            if detached_nr != 0 {
                // Free detached subtree root.
                let subtree_depth = (path.depth() - 1 - partial) as u32;
                self.free_branches(fs, detached_nr, subtree_depth);
            }

            // --- Step 5: Clear right side of partially shared indirect blocks ---
            // Clear right side of each partially shared indirect block.
            // For each level from partial down to 1, free all pointers to the right
            // of the offset at that level.
            for level in (1..=partial).rev() {
                let current_bid = branch
                    .chain
                    .get(level)
                    .and_then(|entry| entry.parent_bid)
                    .ok_or_else(|| {
                        Error::with_message(Errno::EIO, "invalid indirect block number on tail")
                    })?;

                let start_idx = (path.offset_at(level) as usize) + 1;
                let child_depth = (path.depth() - 1 - level) as u32;
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
        if path.iblock_index < 12 {
            // Truncation in direct blocks: free single, double, triple indirect.
            let nr = self.raw_block_ptrs.block_ptrs[12];
            if nr != 0 {
                self.raw_block_ptrs.block_ptrs[12] = 0;
                self.free_branches(fs, nr, 1);
            }
        }
        if path.iblock_index <= 12 {
            // Truncation in direct or single indirect: free double, triple indirect.
            let nr = self.raw_block_ptrs.block_ptrs[13];
            if nr != 0 {
                self.raw_block_ptrs.block_ptrs[13] = 0;
                self.free_branches(fs, nr, 2);
            }
        }
        if path.iblock_index <= 13 {
            // Truncation in direct, single, or double indirect: free triple indirect.
            let nr = self.raw_block_ptrs.block_ptrs[14];
            if nr != 0 {
                self.raw_block_ptrs.block_ptrs[14] = 0;
                self.free_branches(fs, nr, 3);
            }
        }

        Ok(())
    }

    /// Translates a logical block number into a path of block pointer offsets.
    ///
    fn logical_block_to_path(&self, iblock: Iblock) -> Result<BlockPointerPath> {
        let ptrs = (BLOCK_SIZE / size_of::<u32>()) as u32;
        let ptrs_bits = ptrs.trailing_zeros();
        let direct_blocks = 12u32;
        let indirect_blocks = ptrs;
        let double_blocks = 1u32
            .checked_shl(ptrs_bits * 2)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "block path shift overflow"))?;

        let mut block = iblock;

        // NOTE: Each branch subtracts the preceding region's size from `block`,
        // so the branches MUST stay in this exact order.
        if block < direct_blocks {
            Ok(BlockPointerPath {
                iblock_index: block,
                indirect: IndirectOffsets::None,
                boundary: direct_blocks - 1 - block,
            })
        } else {
            block -= direct_blocks;
            if block < indirect_blocks {
                Ok(BlockPointerPath {
                    iblock_index: 12,
                    indirect: IndirectOffsets::Single(block),
                    boundary: ptrs - 1 - (block & (ptrs - 1)),
                })
            } else {
                block -= indirect_blocks;
                if block < double_blocks {
                    Ok(BlockPointerPath {
                        iblock_index: 13,
                        indirect: IndirectOffsets::Double(block >> ptrs_bits, block & (ptrs - 1)),
                        boundary: ptrs - 1 - (block & (ptrs - 1)),
                    })
                } else {
                    block -= double_blocks;
                    if (block >> (ptrs_bits * 2)) < ptrs {
                        Ok(BlockPointerPath {
                            iblock_index: 14,
                            indirect: IndirectOffsets::Triple(
                                block >> (ptrs_bits * 2),
                                (block >> ptrs_bits) & (ptrs - 1),
                                block & (ptrs - 1),
                            ),
                            boundary: ptrs - 1 - (block & (ptrs - 1)),
                        })
                    } else {
                        return_errno_with_message!(Errno::EINVAL, "block number exceeds maximum");
                    }
                }
            }
        }
    }

    /// Traverses the existing block pointer chain for a block path.
    ///
    fn walk_block_chain(&self, path: &BlockPointerPath) -> Result<BranchChainWalkResult> {
        let top_offset = path.iblock_index as usize;
        let top_key = *self
            .raw_block_ptrs
            .block_ptrs
            .get(top_offset)
            .ok_or_else(|| Error::with_message(Errno::EIO, "invalid top-level block pointer"))?;

        let depth = path.depth();
        let mut chain = Vec::with_capacity(depth);
        chain.push(IndirectBlockEntry {
            key: top_key,
            parent_bid: None,
        });
        if top_key == 0 {
            // Zero pointer means the chain is broken at level 0.
            return Ok(BranchChainWalkResult {
                partial_level: 0,
                chain,
            });
        }

        // Callers hold `InodeInner` locks, so chain pointers are stable and no
        // retry loop is needed while walking the existing branch.
        for level in 1..depth {
            let parent_key = chain[level - 1].key;
            let next_key = self
                .indirect_blocks_manager
                .lock()
                .find(parent_key)?
                .read_bid(path.offset_at(level) as usize)?;
            chain.push(IndirectBlockEntry {
                key: next_key,
                parent_bid: Some(parent_key),
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
            partial_level: depth,
            chain,
        })
    }

    fn max_blocks_in_run(path: &BlockPointerPath, max_blocks: u32) -> u32 {
        max_blocks.min(path.boundary + 1)
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
        let depth = path.depth();
        if branch.partial_level < depth {
            return Ok(0..0);
        }
        // The last chain entry is the first mapped data block for `iblock`.
        let first_bid = branch
            .chain
            .get(depth - 1)
            .ok_or_else(|| Error::with_message(Errno::EIO, "incomplete branch result"))?
            .key;
        if first_bid == 0 {
            return Ok(0..0);
        }

        let max_count = Self::max_blocks_in_run(path, max_blocks);
        let mut count = 1u32;
        let start_slot = path.leaf_offset() as usize;

        if path.indirect.is_none() {
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
                .get(depth - 1)
                .and_then(|entry| entry.parent_bid)
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
        let indirect_blks = (path.depth() - 1 - branch.partial_level) as u32;
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
        let start_slot = path.leaf_offset() as usize;
        let mut count = 1u32;
        if path.indirect.is_none() {
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
                .get(path.depth() - 1)
                .and_then(|entry| entry.parent_bid)
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

    fn free_branches(&mut self, fs: &Ext2, block_nr: Ext2Bid, depth: u32) {
        if block_nr == 0 {
            return;
        }
        let sectors_per_block = (BLOCK_SIZE / SECTOR_SIZE) as u32;

        if depth == 0 {
            if let Err(err) = fs.free_blocks(block_nr, 1) {
                // Best-effort free path logs errors and proceeds.
                error!(
                    "ext2: free_branches: failed to free data block {}: {:?}",
                    block_nr, err
                );
                return;
            }
            debug_assert!(self.raw_block_ptrs.sector_count >= sectors_per_block);
            self.raw_block_ptrs.sector_count -= sectors_per_block;
            return;
        }

        let ptrs_per_block = BLOCK_SIZE / size_of::<u32>();

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
        debug_assert!(self.raw_block_ptrs.sector_count >= sectors_per_block);
        self.raw_block_ptrs.sector_count -= sectors_per_block;
    }

    fn allocate_blocks(
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
        if branch.partial_level >= path.depth() {
            return_errno_with_message!(Errno::EIO, "branch is already complete");
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
            debug_assert!(allocated.end >= allocated.start);
            let alloc_len = allocated.end - allocated.start;
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
        debug_assert!(data_blocks_range.end >= data_blocks_range.start);
        let alloc_len = data_blocks_range.end - data_blocks_range.start;
        guard.track_data_blocks(data_blocks_range);

        if alloc_len == 0 || alloc_len > data_blks {
            return_errno_with_message!(Errno::EIO, "invalid data allocation result");
        }

        Ok(guard)
    }

    fn splice_branch(
        &mut self,
        guard: &BlockAllocGuard,
        path: &BlockPointerPath,
        branch: &BranchChainWalkResult,
    ) -> Result<()> {
        let indirect_blocks = &guard.indirect_blocks;
        let data_blocks = &guard.data_blocks;

        let sectors_per_block = (BLOCK_SIZE / SECTOR_SIZE) as u32;

        debug_assert!(data_blocks.end >= data_blocks.start);
        let total = (indirect_blocks.len() as u32)
            .checked_add(data_blocks.end - data_blocks.start)
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
                let slot = path.iblock_index as usize;
                self.write_data_range_to_direct_slots(slot, data_blocks)?;
            } else {
                let parent_bid = branch
                    .chain
                    .get(branch.partial_level)
                    .and_then(|entry| entry.parent_bid)
                    .ok_or_else(|| {
                        Error::with_message(
                            Errno::EIO,
                            "missing parent indirect block for data splice",
                        )
                    })?;
                let slot = path.offset_at(branch.partial_level) as usize;
                let mut mgr = self.indirect_blocks_manager.lock();
                let parent_block = mgr.find_mut(parent_bid)?;
                Self::write_data_range_to_indirect_block(parent_block, slot, data_blocks)?;
            }
        } else {
            let splice_ptr = indirect_blocks[0];
            let mut mgr = self.indirect_blocks_manager.lock();

            let result = (|| -> Result<()> {
                // Build the indirect blocks chain: each block points to the next;
                // the leaf block points to the allocated data range.
                let depth = path.depth();
                for (i, new_bid) in indirect_blocks.iter().copied().enumerate() {
                    let level = branch.partial_level + 1 + i;
                    if level >= depth {
                        return_errno_with_message!(
                            Errno::EIO,
                            "invalid branch depth during allocation"
                        );
                    }

                    let mut block = IndirectBlock::alloc_new(new_bid)?;
                    block.clear();
                    let start_slot = path.offset_at(level) as usize;
                    if i + 1 == indirect_blocks.len() {
                        Self::write_data_range_to_indirect_block(
                            &mut block,
                            start_slot,
                            data_blocks,
                        )?;
                    } else {
                        block.write_bid(start_slot, indirect_blocks[i + 1])?;
                    }
                    mgr.insert_new(new_bid, block)?;
                }

                // Splice the chain root into the parent pointer.
                if branch.partial_level == 0 {
                    let slot = path.iblock_index as usize;
                    if slot >= self.raw_block_ptrs.block_ptrs.len() {
                        return_errno_with_message!(Errno::EIO, "invalid inode block pointer slot");
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
                        .and_then(|entry| entry.parent_bid)
                        .ok_or_else(|| {
                            Error::with_message(
                                Errno::EIO,
                                "missing parent indirect block for splice",
                            )
                        })?;
                    let parent_block = mgr.find_mut(parent_bid)?;
                    let slot = path.offset_at(branch.partial_level) as usize;
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
}

#[cfg(ktest)]
mod test {
    use core::mem::size_of;

    use ostd::prelude::ktest;

    use super::*;
    use crate::{
        fs::fs_impls::ext2::testkit::{ErrorBioDisk, Ext2FixtureBuilder, write_indirect_ptr},
        prelude::*,
    };

    fn alloc_single_block(
        tree: &mut BlockPtrTree,
        fs: &Arc<Ext2>,
        iblock: Iblock,
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

    #[ktest]
    fn block_map_direct_and_indirect_ok() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let disk = &f.disk;

        let ptrs = (BLOCK_SIZE / size_of::<u32>()) as u32;
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
        let direct_path = block_ptr_tree.logical_block_to_path(0).unwrap();
        assert_eq!(direct_path.depth(), 1);
        assert_eq!(direct_path.iblock_index, 0);
        assert_eq!(direct_path.boundary, 11);

        let direct_last_path = block_ptr_tree.logical_block_to_path(11).unwrap();
        assert_eq!(direct_last_path.depth(), 1);
        assert_eq!(direct_last_path.iblock_index, 11);
        assert_eq!(direct_last_path.boundary, 0);

        let indirect_first_path = block_ptr_tree.logical_block_to_path(12).unwrap();
        assert_eq!(indirect_first_path.depth(), 2);
        assert_eq!(indirect_first_path.iblock_index, 12);
        assert_eq!(indirect_first_path.indirect.offset_at(0), 0);

        let indirect_path = block_ptr_tree
            .logical_block_to_path(12 + indirect_index)
            .unwrap();
        assert_eq!(indirect_path.depth(), 2);
        assert_eq!(indirect_path.iblock_index, 12);
        assert_eq!(indirect_path.indirect.offset_at(0), indirect_index);

        let indirect_last_iblock = 12 + ptrs - 1;
        let indirect_last_path = block_ptr_tree
            .logical_block_to_path(indirect_last_iblock)
            .unwrap();
        assert_eq!(indirect_last_path.depth(), 2);
        assert_eq!(indirect_last_path.iblock_index, 12);
        assert_eq!(indirect_last_path.indirect.offset_at(0), ptrs - 1);
        assert_eq!(indirect_last_path.boundary, 0);

        let first_double_iblock = 12 + ptrs;
        let first_double_path = block_ptr_tree
            .logical_block_to_path(first_double_iblock)
            .unwrap();
        assert_eq!(first_double_path.depth(), 3);
        assert_eq!(first_double_path.iblock_index, 13);
        assert_eq!(first_double_path.indirect.offset_at(0), 0);
        assert_eq!(first_double_path.indirect.offset_at(1), 0);

        let double_iblock = 12 + ptrs + (3 << ptrs_bits) + 4;
        let double_path = block_ptr_tree.logical_block_to_path(double_iblock).unwrap();
        assert_eq!(double_path.depth(), 3);
        assert_eq!(double_path.iblock_index, 13);
        assert_eq!(double_path.indirect.offset_at(0), 3);
        assert_eq!(double_path.indirect.offset_at(1), 4);

        let first_triple_iblock = 12 + ptrs + double_blocks;
        let first_triple_path = block_ptr_tree
            .logical_block_to_path(first_triple_iblock)
            .unwrap();
        assert_eq!(first_triple_path.depth(), 4);
        assert_eq!(first_triple_path.iblock_index, 14);
        assert_eq!(first_triple_path.indirect.offset_at(0), 0);
        assert_eq!(first_triple_path.indirect.offset_at(1), 0);
        assert_eq!(first_triple_path.indirect.offset_at(2), 0);

        let triple_iblock =
            12 + ptrs + double_blocks + (2 << (ptrs_bits * 2)) + (3 << ptrs_bits) + 4;
        let triple_path = block_ptr_tree.logical_block_to_path(triple_iblock).unwrap();
        assert_eq!(triple_path.depth(), 4);
        assert_eq!(triple_path.iblock_index, 14);
        assert_eq!(triple_path.indirect.offset_at(0), 2);
        assert_eq!(triple_path.indirect.offset_at(1), 3);
        assert_eq!(triple_path.indirect.offset_at(2), 4);

        // Verify block lookup resolves direct/indirect/double/triple chains.
        assert_eq!(block_ptr_tree.lookup_block(0).unwrap(), Some(11));
        assert_eq!(block_ptr_tree.lookup_block(1).unwrap(), None);
        assert_eq!(
            block_ptr_tree.lookup_block(12 + indirect_index).unwrap(),
            Some(mapped_bid)
        );
        assert_eq!(
            block_ptr_tree.lookup_block(double_iblock).unwrap(),
            Some(double_data_bid)
        );
        assert_eq!(
            block_ptr_tree.lookup_block(triple_iblock).unwrap(),
            Some(triple_data_bid)
        );
    }

    #[ktest]
    fn block_map_get_block_range_returns_contiguous_runs() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let disk = &f.disk;

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

        assert_eq!(block_ptr_tree.lookup_block_range(0, 4).unwrap(), 11..14);
        assert_eq!(block_ptr_tree.lookup_block(0).unwrap(), Some(11));
        assert_eq!(block_ptr_tree.lookup_block_range(12, 4).unwrap(), 70..73);
        assert_eq!(block_ptr_tree.lookup_block_range(15, 4).unwrap(), 90..91);
        assert!(block_ptr_tree.lookup_block_range(3, 4).unwrap().is_empty());
    }

    #[ktest]
    fn block_map_invalid_depth_returns_err() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let disk = &f.disk;

        let ptrs = (BLOCK_SIZE / size_of::<u32>()) as u64;
        let direct = 12u64;
        let indirect = ptrs;
        let double_blocks = 1u64 << (ptrs.trailing_zeros() * 2);
        let triple_blocks = 1u64 << (ptrs.trailing_zeros() * 3);
        let max_iblock = direct + indirect + double_blocks + triple_blocks - 1;
        let too_big = (max_iblock + 1) as u32;

        let block_ptr_tree = make_block_map([0; 15], 0, &f.ext2);

        // Accept the maximum valid logical block and reject the next one.
        block_ptr_tree
            .logical_block_to_path(max_iblock as u32)
            .unwrap();

        let too_big_err = block_ptr_tree.logical_block_to_path(too_big).unwrap_err();
        assert_eq!(too_big_err.error(), Errno::EINVAL);

        let get_too_big_err = block_ptr_tree.lookup_block(too_big).unwrap_err();
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
        let mut ptrs_for_io = [0u32; 15];
        ptrs_for_io[12] = 40;
        let io_block_map = make_block_map(ptrs_for_io, 0, &io_f.ext2);
        let io_err = io_block_map.lookup_block(12).unwrap_err();
        assert_eq!(io_err.error(), Errno::EIO);

        // Any zero pointer on the branch is treated as a hole (None).
        let mut ptrs_for_indirect_hole = [0u32; 15];
        ptrs_for_indirect_hole[12] = 40;
        let indirect_hole_block_map = make_block_map(ptrs_for_indirect_hole, 0, &f.ext2);
        assert_eq!(indirect_hole_block_map.lookup_block(12 + 7).unwrap(), None);

        let mut ptrs_for_double_hole = [0u32; 15];
        ptrs_for_double_hole[13] = 41;
        write_indirect_ptr(disk.as_ref(), 41, 3, 0);
        let double_hole_block_map = make_block_map(ptrs_for_double_hole, 0, &f.ext2);
        let double_hole_iblock = 12 + (ptrs as u32) + (3 << ptrs.trailing_zeros()) + 4;
        assert_eq!(
            double_hole_block_map
                .lookup_block(double_hole_iblock)
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
                .lookup_block(triple_hole_iblock)
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
        let sectors_per_block = (BLOCK_SIZE / SECTOR_SIZE) as u32;

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
        assert_eq!(block_ptr_tree.lookup_block(0).unwrap(), Some(allocated));
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
        let sectors_per_block = (BLOCK_SIZE / SECTOR_SIZE) as u32;

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
            block_ptr_tree.lookup_block_range(0, 3).unwrap(),
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
        let sectors_per_block = (BLOCK_SIZE / SECTOR_SIZE) as u32;

        let mut block_ptr_tree = make_block_map([0u32; 15], 0, &f.ext2);
        let free_before = ext2.super_block().free_blocks_count();
        let allocated = alloc_single_block(&mut block_ptr_tree, ext2, 12).unwrap();
        let free_after = ext2.super_block().free_blocks_count();

        assert_ne!(block_ptr_tree.raw_block_ptrs.block_ptrs[12], 0);
        assert_eq!(block_ptr_tree.lookup_block(12).unwrap(), Some(allocated));
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
        let sectors_per_block = (BLOCK_SIZE / SECTOR_SIZE) as u32;

        let mut block_ptr_tree = make_block_map([0u32; 15], 0, &f.ext2);
        let free_before = ext2.super_block().free_blocks_count();
        let allocated_range = block_ptr_tree
            .lookup_or_alloc_block_range(ext2, 12, 4, true)
            .unwrap();
        let free_after = ext2.super_block().free_blocks_count();

        assert_ne!(block_ptr_tree.raw_block_ptrs.block_ptrs[12], 0);
        assert_eq!(allocated_range.end - allocated_range.start, 4);
        assert_eq!(
            block_ptr_tree.lookup_block_range(12, 4).unwrap(),
            allocated_range.clone()
        );
        assert_eq!(
            block_ptr_tree.lookup_block(12).unwrap(),
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
    fn truncate_indirect_frees_shared_path() {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let ext2 = &f.ext2;
        let ptrs = (BLOCK_SIZE / size_of::<u32>()) as u32;
        let first_double_iblock = 12 + ptrs;

        let mut block_ptr_tree = make_block_map([0u32; 15], 0, &f.ext2);
        alloc_single_block(&mut block_ptr_tree, ext2, first_double_iblock).unwrap();
        alloc_single_block(&mut block_ptr_tree, ext2, first_double_iblock + 1).unwrap();
        alloc_single_block(&mut block_ptr_tree, ext2, first_double_iblock + 2).unwrap();

        block_ptr_tree
            .truncate_blocks(ext2, (first_double_iblock as usize + 1) * BLOCK_SIZE)
            .unwrap();
        assert!(
            block_ptr_tree
                .lookup_block(first_double_iblock)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            block_ptr_tree
                .lookup_block(first_double_iblock + 1)
                .unwrap(),
            None
        );
        assert_eq!(
            block_ptr_tree
                .lookup_block(first_double_iblock + 2)
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
        let ptrs = (BLOCK_SIZE / size_of::<u32>()) as u32;
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
        assert_eq!(block_ptr_tree.lookup_block(12).unwrap(), None);
        assert_eq!(
            block_ptr_tree.lookup_block(first_double_iblock).unwrap(),
            None
        );
        assert_eq!(
            block_ptr_tree.lookup_block(first_triple_iblock).unwrap(),
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
        let sectors_per_block = (BLOCK_SIZE / SECTOR_SIZE) as u32;
        let ptrs = (BLOCK_SIZE / size_of::<u32>()) as u32;
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
