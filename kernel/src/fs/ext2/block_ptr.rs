// SPDX-License-Identifier: MPL-2.0

use core::mem::size_of;

use device_id::{decode_device_numbers, encode_device_numbers};

use super::{fs::Ext2, inode::RawInode, prelude::*};

///TODO: Refactor this with a more rusty approach (e.g. enum).
/// Block path offsets for direct/indirect traversal.
///
/// Produced by `block_to_path` from a logical block number.
/// Linux analogue: output of `ext2_block_to_path` in `fs/ext2/inode.c`.
#[derive(Clone, Copy, Debug)]
pub(super) struct BlockPath {
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
/// Linux analogue: `Indirect` entry in `/root/linux/fs/ext2/inode.c`.
#[derive(Clone, Debug)]
struct IndirectEntry {
    /// Physical block number read from this level's slot; 0 means hole.
    key: u32,
    /// Cached indirect block that contains this level's slot.
    /// `None` for level 0, where the slot lives in inode `i_block[]`.
    bh: Option<Vec<u8>>,
}

/// Result of traversing a block-pointer chain.
#[derive(Debug)]
struct BranchResult {
    /// Level where traversal stopped on a zero pointer, or `path.depth` if complete.
    partial_level: usize,
    /// Entries traversed so far.
    chain: Vec<IndirectEntry>,
}

// In-memory inode mapping (raw on-disk view only i_blocks/i_block[]).
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeMappingDesc {
    pub(super) blocks: u32,
    pub(super) block_ptrs: [u32; 15],
}

impl InodeMappingDesc {
    pub(super) fn from_raw(raw: &RawInode) -> Self {
        Self {
            blocks: raw.blocks,
            block_ptrs: raw.block,
        }
    }

    pub(super) fn from_parts(blocks: u32, block_ptrs: [u32; 15]) -> Self {
        Self { blocks, block_ptrs }
    }

    /// Decodes Linux ext2 old/new special-file device encoding from `i_block`.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1493-1500
    /// Linux: /root/linux/include/linux/kdev_t.h:34-37,46-51
    pub(super) fn decode_device_id(&self) -> u64 {
        let (major, minor) = if self.block_ptrs[0] != 0 {
            let val = self.block_ptrs[0];
            // SPEC: old_decode_dev((major << 8) | minor) with 8-bit major/minor.
            (((val >> 8) & 0xFF), (val & 0xFF))
        } else {
            let dev = self.block_ptrs[1];
            // SPEC: new_decode_dev bit layout in Linux kdev_t.h.
            (
                ((dev & 0xFFF00) >> 8),
                ((dev & 0xFF) | ((dev >> 12) & 0xFFF00)),
            )
        };

        encode_device_numbers(major, minor)
    }

    /// Encodes an Asterinas u64 device ID into Linux ext2 `i_block` layout.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1589-1599
    /// Linux: /root/linux/include/linux/kdev_t.h:24-32,39-44
    pub(super) fn encode_device_id(&mut self, device_id: u64) {
        let (major, minor) = decode_device_numbers(device_id);

        // SPEC: old_valid_dev => MAJOR/MINOR must both fit in 8 bits.
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

#[derive(Debug)]
pub(super) struct InodeMapping {
    pub(super) desc: Dirty<InodeMappingDesc>,
}

impl InodeMapping {
    pub(super) fn new(desc: InodeMappingDesc) -> Self {
        Self {
            desc: Dirty::new(desc),
        }
    }

    pub(super) fn blocks_512(&self) -> u32 {
        self.desc.blocks
    }

    pub(super) fn set_blocks_512(&mut self, blocks: u32) {
        self.desc.blocks = blocks;
    }

    pub(super) fn get_desc(&self) -> &InodeMappingDesc {
        &self.desc
    }

    pub(super) fn encode_device_id(&mut self, device_id: u64) {
        self.desc.encode_device_id(device_id);
    }

    /// Translates a logical block number into a path of block pointer offsets.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:163 (ext2_block_to_path)
    pub(super) fn block_to_path(&self, fs: &Ext2, iblock: u32) -> Result<BlockPath> {
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

        Ok(BlockPath {
            depth,
            offsets,
            boundary,
        })
    }

    /// Traverses the existing block pointer chain for a block path.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:234 (ext2_get_branch)
    fn get_branch(&self, path: &BlockPath, fs: &Ext2) -> Result<BranchResult> {
        if path.depth == 0 || path.depth > path.offsets.len() {
            return_errno_with_message!(Errno::EIO, "invalid block path depth");
        }

        let top_offset = path.offsets[0] as usize;
        let top_key =
            *self.desc.block_ptrs.get(top_offset).ok_or_else(|| {
                Error::with_message(Errno::EIO, "invalid top-level block pointer")
            })?;

        let mut chain = Vec::with_capacity(path.depth);
        chain.push(IndirectEntry {
            key: top_key,
            bh: None,
        });
        if top_key == 0 {
            // SPEC: zero pointer means the chain is broken at level 0.
            return Ok(BranchResult {
                partial_level: 0,
                chain,
            });
        }

        let block_size = fs.block_size();
        if block_size < size_of::<u32>() {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        // DIFF from Linux ext2_get_branch: no verify_chain/-EAGAIN retry loop.
        // Asterinas callers hold InodeInner locks, so chain pointers are stable.
        for level in 1..path.depth {
            let parent_key = chain[level - 1].key;
            let mut buf = vec![0u8; block_size];
            if fs
                .block_device()
                .read_bytes(Bid::new(parent_key as u64).to_offset(), &mut buf)
                .is_err()
            {
                // SPEC: indirect block read failure must return EIO.
                return_errno_with_message!(Errno::EIO, "failed to read indirect block");
            }

            let ptr_offset = (path.offsets[level] as usize) * size_of::<u32>();
            let ptr_end = ptr_offset + size_of::<u32>();
            if ptr_end > buf.len() {
                return_errno_with_message!(Errno::EIO, "indirect pointer offset out of bounds");
            }

            let next_key = u32::from_le_bytes([
                buf[ptr_offset],
                buf[ptr_offset + 1],
                buf[ptr_offset + 2],
                buf[ptr_offset + 3],
            ]);
            chain.push(IndirectEntry {
                key: next_key,
                bh: Some(buf),
            });
            if next_key == 0 {
                // SPEC: include the zero-key entry and report break level.
                return Ok(BranchResult {
                    partial_level: level,
                    chain,
                });
            }
        }

        Ok(BranchResult {
            partial_level: path.depth,
            chain,
        })
    }

    /// Linux: /root/linux/fs/ext2/inode.c:361 (ext2_blks_to_allocate)
    fn blks_to_allocate(&self, branch: &BranchResult, path: &BlockPath) -> (u32, u32) {
        let indirect_blks = (path.depth - 1 - branch.partial_level) as u32;
        (indirect_blks, 1)
    }

    /// Linux: /root/linux/fs/ext2/inode.c:1096 (ext2_free_data)
    /// Linux: /root/linux/fs/ext2/inode.c:1136 (ext2_free_branches)
    pub(super) fn free_branches(&mut self, fs: &Ext2, block_nr: u32, depth: u32) {
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
                // SPEC: best-effort free path logs errors and proceeds.
                error!(
                    "ext2: free_branches: failed to free data block {}: {:?}",
                    block_nr, err
                );
                return;
            }
            self.desc.blocks = self.desc.blocks.saturating_sub(sectors_per_block);
            return;
        }

        let ptrs_per_block = block_size / size_of::<u32>();
        if ptrs_per_block == 0 {
            error!(
                "ext2: free_branches: invalid indirect fanout for block size {}",
                block_size
            );
            return;
        }

        let mut buf = vec![0u8; block_size];
        if fs
            .block_device()
            .read_bytes(Bid::new(block_nr as u64).to_offset(), &mut buf)
            .is_err()
        {
            // Linux ext2_free_branches logs read failure and skips that branch.
            error!(
                "ext2: free_branches: failed to read indirect block {} (depth {})",
                block_nr, depth
            );
            return;
        }

        for idx in 0..ptrs_per_block {
            let ptr_offset = idx * size_of::<u32>();
            let ptr_end = ptr_offset + size_of::<u32>();
            if ptr_end > buf.len() {
                break;
            }

            let nr = u32::from_le_bytes([
                buf[ptr_offset],
                buf[ptr_offset + 1],
                buf[ptr_offset + 2],
                buf[ptr_offset + 3],
            ]);
            if nr == 0 {
                continue;
            }
            self.free_branches(fs, nr, depth - 1);
        }

        if let Err(err) = fs.free_blocks(block_nr, 1) {
            error!(
                "ext2: free_branches: failed to free indirect block {}: {:?}",
                block_nr, err
            );
            return;
        }
        self.desc.blocks = self.desc.blocks.saturating_sub(sectors_per_block);
    }

    /// Linux: /root/linux/fs/ext2/inode.c:479 (ext2_alloc_branch)
    /// Linux: /root/linux/fs/ext2/inode.c:561 (ext2_splice_branch)
    fn alloc_and_splice_branch(
        &mut self,
        fs: &Ext2,
        indirect_blks: u32,
        data_blks: u32,
        path: &BlockPath,
        branch: &BranchResult,
    ) -> Result<Bid> {
        if data_blks == 0 {
            return_errno_with_message!(Errno::EIO, "invalid zero data allocation");
        }
        if branch.partial_level >= path.depth {
            return_errno_with_message!(Errno::EIO, "branch is already complete");
        }

        let total = indirect_blks
            .checked_add(data_blks)
            .ok_or_else(|| Error::with_message(Errno::EIO, "block allocation count overflow"))?;
        if total == 0 {
            return_errno_with_message!(Errno::EIO, "invalid zero total allocation");
        }

        let block_size = fs.block_size();
        if block_size < size_of::<u32>() {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        let sectors_per_block = (block_size / SECTOR_SIZE) as u32;
        if sectors_per_block == 0 {
            return_errno_with_message!(Errno::EIO, "invalid sector accounting for block size");
        }
        let added_sectors = total
            .checked_mul(sectors_per_block)
            .ok_or_else(|| Error::with_message(Errno::EIO, "inode block accounting overflow"))?;
        let new_block_count = self
            .desc
            .blocks
            .checked_add(added_sectors)
            .ok_or_else(|| Error::with_message(Errno::EIO, "inode block count overflow"))?;

        let mut new_blocks = Vec::with_capacity(total as usize);
        let mut alloc_goal = Bid::new(
            branch
                .chain
                .last()
                .map_or(self.desc.block_ptrs[0], |entry| entry.key)
                .max(fs.super_block().first_data_block()) as u64,
        );
        // Rollback helper: frees all blocks allocated so far.
        let free_all_fn = |blocks: &Vec<u32>| {
            for bid in blocks {
                let _ = fs.free_blocks(*bid, 1);
            }
        };

        while (new_blocks.len() as u32) < total {
            let remain = total - new_blocks.len() as u32;
            let allocated = match fs.alloc_blocks(remain, alloc_goal) {
                Ok(allocated) => allocated,
                Err(err) => {
                    free_all_fn(&new_blocks);
                    return Err(err);
                }
            };

            let alloc_len = allocated.end - allocated.start;
            if alloc_len == 0 || alloc_len > remain {
                free_all_fn(&new_blocks);
                return_errno_with_message!(Errno::EIO, "invalid block allocation result");
            }

            new_blocks.extend(allocated);
            if let Some(last) = new_blocks.last() {
                alloc_goal = Bid::new((last + 1) as u64);
            }
        }

        // SPEC: data block is at index `indirect_blks` in allocation order.
        let data_block = match new_blocks.get(indirect_blks as usize) {
            Some(bid) => *bid,
            None => {
                free_all_fn(&new_blocks);
                return_errno_with_message!(Errno::EIO, "allocated chain missing data block");
            }
        };

        // Phase 1: Build the new chain — zero-fill each indirect block, write
        // the next-level pointer, and flush to disk. The chain is fully formed
        // but still unreachable from the existing tree.
        for i in 0..(indirect_blks as usize) {
            let level = branch.partial_level + 1 + i;
            if level >= path.depth {
                free_all_fn(&new_blocks);
                return_errno_with_message!(Errno::EIO, "invalid branch depth during allocation");
            }

            let ptr_offset = (path.offsets[level] as usize) * size_of::<u32>();
            let ptr_end = ptr_offset + size_of::<u32>();
            if ptr_end > block_size {
                free_all_fn(&new_blocks);
                return_errno_with_message!(Errno::EIO, "indirect pointer offset out of bounds");
            }

            let next_block = match new_blocks.get(i + 1) {
                Some(bid) => *bid,
                None => {
                    free_all_fn(&new_blocks);
                    return_errno_with_message!(Errno::EIO, "allocated chain metadata mismatch");
                }
            };

            let mut buf = vec![0u8; block_size];
            buf[ptr_offset..ptr_end].copy_from_slice(&next_block.to_le_bytes());
            if fs
                .block_device()
                .write_bytes(Bid::new(new_blocks[i] as u64).to_offset(), &buf)
                .is_err()
            {
                free_all_fn(&new_blocks);
                return_errno_with_message!(Errno::EIO, "failed to write new indirect block");
            }
        }

        // Phase 2: Splice — write the chain head into the break point, making
        // the entire new chain reachable in one pointer write.
        let splice_ptr = new_blocks[0];
        if branch.partial_level == 0 {
            let slot = path.offsets[0] as usize;
            if slot >= self.desc.block_ptrs.len() {
                free_all_fn(&new_blocks);
                return_errno_with_message!(Errno::EIO, "invalid inode block pointer slot");
            }
            if self.desc.block_ptrs[slot] != 0 {
                free_all_fn(&new_blocks);
                return_errno_with_message!(Errno::EIO, "block pointer changed during allocation");
            }

            // SPEC: splice directly into inode i_block[].
            self.desc.block_ptrs[slot] = splice_ptr;
        } else {
            let parent_entry_level = branch.partial_level;
            let mut parent_buf = match branch
                .chain
                .get(parent_entry_level)
                .and_then(|entry| entry.bh.clone())
            {
                Some(buf) => buf,
                None => {
                    free_all_fn(&new_blocks);
                    return_errno_with_message!(
                        Errno::EIO,
                        "missing parent indirect block for splice"
                    );
                }
            };

            let parent_bid = branch
                .chain
                .get(parent_entry_level - 1)
                .map(|entry| entry.key)
                .unwrap_or(0);
            if parent_bid == 0 {
                free_all_fn(&new_blocks);
                return_errno_with_message!(Errno::EIO, "invalid parent indirect block number");
            }

            let ptr_offset = (path.offsets[parent_entry_level] as usize) * size_of::<u32>();
            let ptr_end = ptr_offset + size_of::<u32>();
            if ptr_end > parent_buf.len() {
                free_all_fn(&new_blocks);
                return_errno_with_message!(Errno::EIO, "splice offset out of bounds");
            }
            parent_buf[ptr_offset..ptr_end].copy_from_slice(&splice_ptr.to_le_bytes());

            if fs
                .block_device()
                .write_bytes(Bid::new(parent_bid as u64).to_offset(), &parent_buf)
                .is_err()
            {
                free_all_fn(&new_blocks);
                return_errno_with_message!(Errno::EIO, "failed to splice branch into parent");
            }
        }

        // SPEC: ext2_splice_branch-style inode accounting and ctime update.
        self.desc.blocks = new_block_count;

        Ok(Bid::new(data_block as u64))
    }

    /// Resolves a logical block to physical block (read-only).
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:783 (ext2_get_block)
    pub(super) fn get_block(&self, fs: &Ext2, iblock: u32) -> Result<Option<Bid>> {
        let path = self.block_to_path(fs, iblock)?;
        if path.depth == 0 {
            return Ok(None);
        }

        let branch = self.get_branch(&path, fs)?;
        if branch.partial_level < path.depth {
            return Ok(None);
        }

        let bid = branch
            .chain
            .get(path.depth - 1)
            .ok_or_else(|| Error::with_message(Errno::EIO, "incomplete branch result"))?
            .key;
        if bid == 0 {
            return Ok(None);
        }
        Ok(Some(Bid::new(bid as u64)))
    }

    /// Resolves a logical block to physical, optionally allocating missing branch.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:624 (ext2_get_blocks)
    pub(super) fn get_or_alloc_block(
        &mut self,
        fs: &Ext2,
        iblock: u32,
        create: bool,
    ) -> Result<Option<Bid>> {
        let path = self.block_to_path(fs, iblock)?;
        if path.depth == 0 {
            return_errno_with_message!(Errno::EIO, "invalid block path depth");
        }

        let branch = self.get_branch(&path, fs)?;
        if branch.partial_level == path.depth {
            let mapped = branch
                .chain
                .get(path.depth - 1)
                .ok_or_else(|| Error::with_message(Errno::EIO, "incomplete branch result"))?
                .key;
            return Ok(Some(Bid::new(mapped as u64)));
        }
        if !create {
            return Ok(None);
        }

        let (indirect_blks, data_blks) = self.blks_to_allocate(&branch, &path);
        let bid = self.alloc_and_splice_branch(fs, indirect_blks, data_blks, &path, &branch)?;
        Ok(Some(bid))
    }

    /// Truncates all blocks beyond `new_size`.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1172 (__ext2_truncate_blocks)
    pub(super) fn truncate_blocks(&mut self, fs: &Ext2, new_size: usize) -> Result<()> {
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }
        let sectors_per_block = (block_size / SECTOR_SIZE) as u32;
        if sectors_per_block == 0 {
            return_errno_with_message!(Errno::EIO, "invalid sector accounting for block size");
        }

        // SPEC: first logical block to free = ceil(new_size / block_size).
        let iblock = u32::try_from(new_size.div_ceil(block_size))
            .map_err(|_| Error::with_message(Errno::EINVAL, "truncate size exceeds ext2 limits"))?;

        // Convert logical block number to access path (depth + offsets).
        let path = self.block_to_path(fs, iblock)?;
        if path.depth == 0 {
            return Ok(());
        }

        let ptrs_per_block = block_size / size_of::<u32>();
        if ptrs_per_block == 0 {
            return_errno_with_message!(Errno::EIO, "invalid indirect pointer fanout");
        }

        // === Case 1: Direct blocks only ===
        if path.depth == 1 {
            // Linux: ext2_free_data(i_data + offsets[0], i_data + EXT2_NDIR_BLOCKS).
            // Free direct blocks from offsets[0] to block_ptrs[11].
            let start = (path.offsets[0] as usize).min(12);
            for idx in start..12 {
                let ptr = self.desc.block_ptrs[idx];
                if ptr == 0 {
                    continue;
                }
                fs.free_blocks(ptr, 1)?;
                self.desc.block_ptrs[idx] = 0;
                self.desc.blocks = self.desc.blocks.saturating_sub(sectors_per_block);
            }
        } else {
            // === Case 2: Indirect blocks ===

            // --- Step 1: Adjust depth for boundary case ---
            // SPEC: ext2_find_shared-style partial branch handling.
            // If truncation point is at the start of an indirect block (offset = 0),
            // we can handle it at a higher level without reading that indirect block.
            let mut k = path.depth;
            while k > 1 && path.offsets[k - 1] == 0 {
                k -= 1;
            }
            let shared_path = BlockPath {
                depth: k,
                offsets: path.offsets,
                boundary: path.boundary,
            };

            // --- Step 2: Read indirect block chain ---
            // branch.chain[i] contains the i-th level indirect block.
            // branch.partial_level indicates how deep we successfully read.
            let branch = self.get_branch(&shared_path, fs)?;
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
            // SPEC: preserve Linux all_zeroes optimization by walking up to the
            // highest indirect block that can be fully detached.
            // If the left side (to be kept) of an indirect block is all zeros,
            // we can free the entire indirect block and handle it at a higher level.
            while partial > 0 {
                let buf = branch
                    .chain
                    .get(partial)
                    .and_then(|entry| entry.bh.as_ref())
                    .ok_or_else(|| {
                        Error::with_message(
                            Errno::EIO,
                            "missing indirect block buffer for all-zeroes check",
                        )
                    })?;

                // Check if entries [0..offsets[partial]) are all zero.
                let keep_entries = path.offsets[partial] as usize;
                let keep_bytes = keep_entries * size_of::<u32>();
                if keep_bytes > buf.len() {
                    return_errno_with_message!(Errno::EIO, "all-zeroes check offset out of bounds");
                }

                let mut all_zero = true;
                let mut byte = 0usize;
                while byte < keep_bytes {
                    let val = u32::from_le_bytes([
                        buf[byte],
                        buf[byte + 1],
                        buf[byte + 2],
                        buf[byte + 3],
                    ]);
                    if val != 0 {
                        all_zero = false;
                        break;
                    }
                    byte += size_of::<u32>();
                }
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
                if slot >= self.desc.block_ptrs.len() {
                    return_errno_with_message!(Errno::EIO, "inode block pointer slot out of range");
                }
                detached_nr = self.desc.block_ptrs[slot];
                self.desc.block_ptrs[slot] = 0;
            } else {
                // Detach from parent indirect block.
                // chain[partial] contains the current level's buffer.
                // chain[partial-1].key is the block number to write back.
                let mut parent_buf = branch
                    .chain
                    .get(partial)
                    .and_then(|entry| entry.bh.clone())
                    .ok_or_else(|| {
                        Error::with_message(Errno::EIO, "missing parent indirect block buffer")
                    })?;
                let parent_bid = branch
                    .chain
                    .get(partial - 1)
                    .map(|entry| entry.key)
                    .unwrap_or(0);
                if parent_bid == 0 {
                    return_errno_with_message!(Errno::EIO, "invalid parent indirect block number");
                }

                // Read the pointer at offsets[partial].
                let ptr_offset = (path.offsets[partial] as usize) * size_of::<u32>();
                let ptr_end = ptr_offset + size_of::<u32>();
                if ptr_end > parent_buf.len() {
                    return_errno_with_message!(Errno::EIO, "shared branch pointer out of bounds");
                }

                detached_nr = u32::from_le_bytes([
                    parent_buf[ptr_offset],
                    parent_buf[ptr_offset + 1],
                    parent_buf[ptr_offset + 2],
                    parent_buf[ptr_offset + 3],
                ]);

                // Zero out the pointer and write back.
                parent_buf[ptr_offset..ptr_end].copy_from_slice(&0u32.to_le_bytes());
                if fs
                    .block_device()
                    .write_bytes(Bid::new(parent_bid as u64).to_offset(), &parent_buf)
                    .is_err()
                {
                    return_errno_with_message!(Errno::EIO, "failed to detach shared branch");
                }
            }

            // Recursively free the detached subtree.
            if detached_nr != 0 {
                // SPEC: free detached subtree root.
                let subtree_depth = (path.depth - 1 - partial) as u32;
                self.free_branches(fs, detached_nr, subtree_depth);
            }

            // --- Step 5: Clear right side of partially shared indirect blocks ---
            // SPEC: clear right side of each partially shared indirect block.
            // For each level from partial down to 1, free all pointers to the right
            // of offsets[level].
            for level in (1..=partial).rev() {
                let parent_bid = branch
                    .chain
                    .get(level - 1)
                    .map(|entry| entry.key)
                    .unwrap_or(0);
                if parent_bid == 0 {
                    return_errno_with_message!(Errno::EIO, "invalid indirect block number on tail");
                }

                // Re-read the current indirect block state to avoid stale-buffer
                // overwrite after the detach step above.
                let mut buf = vec![0u8; block_size];
                if fs
                    .block_device()
                    .read_bytes(Bid::new(parent_bid as u64).to_offset(), &mut buf)
                    .is_err()
                {
                    return_errno_with_message!(
                        Errno::EIO,
                        "failed to read indirect block for truncation tail"
                    );
                }

                // Free all pointers from offsets[level]+1 to the end.
                let start_idx = (path.offsets[level] as usize) + 1;
                let child_depth = (path.depth - 1 - level) as u32;
                for idx in start_idx..ptrs_per_block {
                    let ptr_offset = idx * size_of::<u32>();
                    let ptr_end = ptr_offset + size_of::<u32>();
                    if ptr_end > buf.len() {
                        break;
                    }
                    let nr = u32::from_le_bytes([
                        buf[ptr_offset],
                        buf[ptr_offset + 1],
                        buf[ptr_offset + 2],
                        buf[ptr_offset + 3],
                    ]);
                    if nr == 0 {
                        continue;
                    }
                    buf[ptr_offset..ptr_end].copy_from_slice(&0u32.to_le_bytes());
                    self.free_branches(fs, nr, child_depth);
                }

                // Write back the modified indirect block.
                if fs
                    .block_device()
                    .write_bytes(Bid::new(parent_bid as u64).to_offset(), &buf)
                    .is_err()
                {
                    return_errno_with_message!(
                        Errno::EIO,
                        "failed to persist partial indirect truncation"
                    );
                }
            }
        }

        // === Step 6: Free complete indirect block trees ===
        // Linux: do_indirects switch/fallthrough by offsets[0].
        // If truncation point is in direct blocks, free all indirect trees.
        // If in single indirect, free double and triple indirect trees, etc.
        if path.offsets[0] < 12 {
            // Truncation in direct blocks: free single, double, triple indirect.
            let nr = self.desc.block_ptrs[12];
            if nr != 0 {
                self.desc.block_ptrs[12] = 0;
                self.free_branches(fs, nr, 1);
            }
        }
        if path.offsets[0] <= 12 {
            // Truncation in direct or single indirect: free double, triple indirect.
            let nr = self.desc.block_ptrs[13];
            if nr != 0 {
                self.desc.block_ptrs[13] = 0;
                self.free_branches(fs, nr, 2);
            }
        }
        if path.offsets[0] <= 13 {
            // Truncation in direct, single, or double indirect: free triple indirect.
            let nr = self.desc.block_ptrs[14];
            if nr != 0 {
                self.desc.block_ptrs[14] = 0;
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
            ext2::{
                inode::{FileFlags, InodeDesc},
                testkit::{
                    self, ErrorBioDisk, Ext2FixtureBuilder, RawInodeBuilder, write_indirect_ptr,
                },
            },
            utils::IdBitmap,
        },
        prelude::*,
    };

    fn make_raw_inode(mode: u16) -> RawInode {
        RawInodeBuilder::new(mode).build()
    }

    fn make_mapping(block_ptrs: [u32; 15], blocks: u32) -> InodeMapping {
        InodeMapping::new(InodeMappingDesc::from_parts(blocks, block_ptrs))
    }

    fn reload_group0_cached_bitmaps_from_disk(f: &testkit::Ext2Fixture) {
        let group = f.block_group(0);

        let mut block_bitmap_buf = vec![0u8; BLOCK_SIZE];
        f.disk
            .segment()
            .read_bytes(group.block_bitmap_bid().to_offset(), &mut block_bitmap_buf)
            .unwrap();
        let block_len = {
            let bitmap = group.block_bitmap();
            bitmap.len()
        };
        let mut block_bitmap = group.block_bitmap_mut();
        **block_bitmap = IdBitmap::from_buf(block_bitmap_buf.into_boxed_slice(), block_len);

        let mut inode_bitmap_buf = vec![0u8; BLOCK_SIZE];
        f.disk
            .segment()
            .read_bytes(group.inode_bitmap_bid().to_offset(), &mut inode_bitmap_buf)
            .unwrap();
        let inode_len = {
            let bitmap = group.inode_bitmap();
            bitmap.len()
        };
        let mut inode_bitmap = group.inode_bitmap_mut();
        **inode_bitmap = IdBitmap::from_buf(inode_bitmap_buf.into_boxed_slice(), inode_len);
    }

    #[ktest]
    fn block_mapping_direct_and_indirect_ok() {
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
        let mapping = make_mapping(block_ptrs, 0);

        // Cover exact transition boundaries across all mapping levels.
        let direct_path = mapping.block_to_path(ext2, 0).unwrap();
        assert_eq!(direct_path.depth, 1);
        assert_eq!(direct_path.offsets[0], 0);
        assert_eq!(direct_path.boundary, 11);

        let direct_last_path = mapping.block_to_path(ext2, 11).unwrap();
        assert_eq!(direct_last_path.depth, 1);
        assert_eq!(direct_last_path.offsets[0], 11);
        assert_eq!(direct_last_path.boundary, 0);

        let indirect_first_path = mapping.block_to_path(ext2, 12).unwrap();
        assert_eq!(indirect_first_path.depth, 2);
        assert_eq!(indirect_first_path.offsets[0], 12);
        assert_eq!(indirect_first_path.offsets[1], 0);

        let indirect_path = mapping.block_to_path(ext2, 12 + indirect_index).unwrap();
        assert_eq!(indirect_path.depth, 2);
        assert_eq!(indirect_path.offsets[0], 12);
        assert_eq!(indirect_path.offsets[1], indirect_index);

        let indirect_last_iblock = 12 + ptrs - 1;
        let indirect_last_path = mapping.block_to_path(ext2, indirect_last_iblock).unwrap();
        assert_eq!(indirect_last_path.depth, 2);
        assert_eq!(indirect_last_path.offsets[0], 12);
        assert_eq!(indirect_last_path.offsets[1], ptrs - 1);
        assert_eq!(indirect_last_path.boundary, 0);

        let first_double_iblock = 12 + ptrs;
        let first_double_path = mapping.block_to_path(ext2, first_double_iblock).unwrap();
        assert_eq!(first_double_path.depth, 3);
        assert_eq!(first_double_path.offsets[0], 13);
        assert_eq!(first_double_path.offsets[1], 0);
        assert_eq!(first_double_path.offsets[2], 0);

        let double_iblock = 12 + ptrs + (3 << ptrs_bits) + 4;
        let double_path = mapping.block_to_path(ext2, double_iblock).unwrap();
        assert_eq!(double_path.depth, 3);
        assert_eq!(double_path.offsets[0], 13);
        assert_eq!(double_path.offsets[1], 3);
        assert_eq!(double_path.offsets[2], 4);

        let first_triple_iblock = 12 + ptrs + double_blocks;
        let first_triple_path = mapping.block_to_path(ext2, first_triple_iblock).unwrap();
        assert_eq!(first_triple_path.depth, 4);
        assert_eq!(first_triple_path.offsets[0], 14);
        assert_eq!(first_triple_path.offsets[1], 0);
        assert_eq!(first_triple_path.offsets[2], 0);
        assert_eq!(first_triple_path.offsets[3], 0);

        let triple_iblock =
            12 + ptrs + double_blocks + (2 << (ptrs_bits * 2)) + (3 << ptrs_bits) + 4;
        let triple_path = mapping.block_to_path(ext2, triple_iblock).unwrap();
        assert_eq!(triple_path.depth, 4);
        assert_eq!(triple_path.offsets[0], 14);
        assert_eq!(triple_path.offsets[1], 2);
        assert_eq!(triple_path.offsets[2], 3);
        assert_eq!(triple_path.offsets[3], 4);

        // Verify block lookup resolves direct/indirect/double/triple chains.
        assert_eq!(mapping.get_block(ext2, 0).unwrap(), Some(Bid::new(11)));
        assert_eq!(mapping.get_block(ext2, 1).unwrap(), None);
        assert_eq!(
            mapping.get_block(ext2, 12 + indirect_index).unwrap(),
            Some(Bid::new(mapped_bid as u64))
        );
        assert_eq!(
            mapping.get_block(ext2, double_iblock).unwrap(),
            Some(Bid::new(double_data_bid as u64))
        );
        assert_eq!(
            mapping.get_block(ext2, triple_iblock).unwrap(),
            Some(Bid::new(triple_data_bid as u64))
        );
    }

    #[ktest]
    fn block_mapping_invalid_depth_returns_err() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let (disk, ext2) = (&f.disk, &f.ext2);

        let ptrs = (ext2.block_size() / size_of::<u32>()) as u64;
        let direct = 12u64;
        let indirect = ptrs;
        let double_blocks = 1u64 << (ptrs.trailing_zeros() * 2);
        let triple_blocks = 1u64 << (ptrs.trailing_zeros() * 3);
        let max_iblock = direct + indirect + double_blocks + triple_blocks - 1;
        let too_big = (max_iblock + 1) as u32;

        let mapping = make_mapping([0; 15], 0);

        // Linux semantics: max valid iblock is accepted, max + 1 is rejected.
        mapping.block_to_path(ext2, max_iblock as u32).unwrap();

        let too_big_err = mapping.block_to_path(ext2, too_big).unwrap_err();
        assert_eq!(too_big_err.error(), Errno::EINVAL);

        let get_too_big_err = mapping.get_block(ext2, too_big).unwrap_err();
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
        let io_mapping = make_mapping(ptrs_for_io, 0);
        let io_err = io_mapping.get_block(io_ext2, 12).unwrap_err();
        assert_eq!(io_err.error(), Errno::EIO);

        // Any zero pointer on the branch is treated as a hole (None).
        let mut ptrs_for_indirect_hole = [0u32; 15];
        ptrs_for_indirect_hole[12] = 40;
        let indirect_hole_mapping = make_mapping(ptrs_for_indirect_hole, 0);
        assert_eq!(indirect_hole_mapping.get_block(ext2, 12 + 7).unwrap(), None);

        let mut ptrs_for_double_hole = [0u32; 15];
        ptrs_for_double_hole[13] = 41;
        write_indirect_ptr(disk.as_ref(), 41, 3, 0);
        let double_hole_mapping = make_mapping(ptrs_for_double_hole, 0);
        let double_hole_iblock = 12 + (ptrs as u32) + (3 << ptrs.trailing_zeros()) + 4;
        assert_eq!(
            double_hole_mapping
                .get_block(ext2, double_hole_iblock)
                .unwrap(),
            None
        );

        let mut ptrs_for_triple_hole = [0u32; 15];
        ptrs_for_triple_hole[14] = 43;
        write_indirect_ptr(disk.as_ref(), 43, 2, 44);
        write_indirect_ptr(disk.as_ref(), 44, 3, 0);
        let triple_hole_mapping = make_mapping(ptrs_for_triple_hole, 0);
        let triple_hole_iblock = 12
            + (ptrs as u32)
            + (double_blocks as u32)
            + (2 << (ptrs.trailing_zeros() * 2))
            + (3 << ptrs.trailing_zeros())
            + 4;
        assert_eq!(
            triple_hole_mapping
                .get_block(ext2, triple_hole_iblock)
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

        let mut mapping = make_mapping([0u32; 15], 0);
        assert_eq!(mapping.get_or_alloc_block(ext2, 0, false).unwrap(), None);
        assert_eq!(mapping.desc.block_ptrs[0], 0);

        let free_before = ext2.super_block().free_blocks_count();
        let allocated = mapping.get_or_alloc_block(ext2, 0, true).unwrap().unwrap();
        let free_after = ext2.super_block().free_blocks_count();

        assert_eq!(mapping.desc.block_ptrs[0], allocated.to_raw() as u32);
        assert_eq!(mapping.get_block(ext2, 0).unwrap(), Some(allocated));
        assert_eq!(mapping.desc.blocks, sectors_per_block);
        assert_eq!(free_before.saturating_sub(free_after), 1);
    }

    #[ktest]
    fn block_alloc_indirect_path_ok() {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let ext2 = &f.ext2;
        let sectors_per_block = (ext2.block_size() / SECTOR_SIZE) as u32;

        let mut mapping = make_mapping([0u32; 15], 0);
        let free_before = ext2.super_block().free_blocks_count();
        let allocated = mapping.get_or_alloc_block(ext2, 12, true).unwrap().unwrap();
        let free_after = ext2.super_block().free_blocks_count();

        assert_ne!(mapping.desc.block_ptrs[12], 0);
        assert_eq!(mapping.get_block(ext2, 12).unwrap(), Some(allocated));
        assert_eq!(mapping.desc.blocks, sectors_per_block.saturating_mul(2));
        assert_eq!(free_before.saturating_sub(free_after), 2);
    }

    #[ktest]
    fn block_alloc_enospc_preserves_inode_state() {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(0, 0)
            .with_filled_block_bitmap(true)
            .build()
            .unwrap();
        let ext2 = &f.ext2;

        let mut mapping = make_mapping([0u32; 15], 0);
        let err = mapping.get_or_alloc_block(ext2, 0, true).unwrap_err();
        assert_eq!(err.error(), Errno::ENOSPC);
        assert_eq!(mapping.desc.block_ptrs, [0u32; 15]);
        assert_eq!(mapping.desc.blocks, 0);
    }

    #[ktest]
    fn block_alloc_fragmented_chain_ok() {
        // Corner case: total free blocks are enough, but no contiguous run can satisfy
        // the full request in one call. This forces get_or_alloc_block() to loop and
        // accumulate allocations across multiple fs.alloc_blocks() calls.
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
            .saturating_add(sb.itb_per_group())
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
        let mut mapping = make_mapping([0u32; 15], 0);

        let allocated_data = mapping
            .get_or_alloc_block(ext2, first_double_iblock, true)
            .unwrap()
            .unwrap();
        assert_ne!(mapping.desc.block_ptrs[13], 0);
        assert_eq!(
            mapping.get_block(ext2, first_double_iblock).unwrap(),
            Some(allocated_data)
        );

        // All three isolated free blocks should be consumed.
        let block_size = ext2.block_size();
        assert_eq!(ext2.super_block().free_blocks_count(), 0);
        assert_eq!(f.block_group(0).free_blocks_count(), 0);
        assert_eq!(mapping.desc.blocks, ((block_size / SECTOR_SIZE) as u32) * 3);
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

        let mut mapping = make_mapping([0u32; 15], 0);
        mapping
            .get_or_alloc_block(ext2, first_double_iblock, true)
            .unwrap();
        mapping
            .get_or_alloc_block(ext2, first_double_iblock + 1, true)
            .unwrap();
        mapping
            .get_or_alloc_block(ext2, first_double_iblock + 2, true)
            .unwrap();

        mapping
            .truncate_blocks(ext2, (first_double_iblock as usize + 1) * block_size)
            .unwrap();
        assert!(
            mapping
                .get_block(ext2, first_double_iblock)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            mapping.get_block(ext2, first_double_iblock + 1).unwrap(),
            None
        );
        assert_eq!(
            mapping.get_block(ext2, first_double_iblock + 2).unwrap(),
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

        let mut mapping = make_mapping([0u32; 15], 0);
        mapping.get_or_alloc_block(ext2, 12, true).unwrap();
        mapping
            .get_or_alloc_block(ext2, first_double_iblock, true)
            .unwrap();
        mapping
            .get_or_alloc_block(ext2, first_triple_iblock, true)
            .unwrap();
        assert_ne!(mapping.desc.block_ptrs[12], 0);
        assert_ne!(mapping.desc.block_ptrs[13], 0);
        assert_ne!(mapping.desc.block_ptrs[14], 0);

        mapping.truncate_blocks(ext2, 0).unwrap();
        assert_eq!(mapping.desc.block_ptrs[12], 0);
        assert_eq!(mapping.desc.block_ptrs[13], 0);
        assert_eq!(mapping.desc.block_ptrs[14], 0);
        assert_eq!(mapping.get_block(ext2, 12).unwrap(), None);
        assert_eq!(mapping.get_block(ext2, first_double_iblock).unwrap(), None);
        assert_eq!(mapping.get_block(ext2, first_triple_iblock).unwrap(), None);
        assert_eq!(mapping.desc.blocks, 0);
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

        let mut mapping = make_mapping([0u32; 15], 0);
        mapping
            .get_or_alloc_block(ext2, first_triple_iblock, true)
            .unwrap();
        let root = mapping.desc.block_ptrs[14];
        assert_ne!(root, 0);
        assert_eq!(mapping.desc.blocks, sectors_per_block.saturating_mul(4));

        let free_before = ext2.super_block().free_blocks_count();
        mapping.free_branches(ext2, root, 3);
        mapping.desc.block_ptrs[14] = 0;
        let free_after = ext2.super_block().free_blocks_count();

        assert_eq!(free_after.saturating_sub(free_before), 4);
        assert_eq!(mapping.desc.blocks, 0);
    }
}
