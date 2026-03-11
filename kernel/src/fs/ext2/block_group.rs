// SPDX-License-Identifier: MPL-2.0

use core::{fmt, mem::size_of};

use ostd::const_assert;

use super::{
    block_ptr::Ext2Bid,
    fs::Ext2,
    inode::{Inode, InodeDesc, RawInode},
    prelude::*,
    super_block::SuperBlock,
};
use crate::fs::utils::IdBitmap;

/// Backend of the inode table page cache in one block group.
///
/// Linux equivalent: `sb_bread()` buffer_head cache path in
/// `/root/linux/fs/ext2/inode.c:1314` (`ext2_get_inode`).
/// Asterinas equivalent: `PageCacheBackend` implementation.
struct InodeTableBackend {
    /// Physical block ID of `bg_inode_table`.
    inode_table_bid: Ext2Bid,
    /// Total inode table size in bytes (`inodes_per_group * inode_size`).
    raw_inodes_size: usize,
    /// Block device handle for I/O (replaces `Weak<Ext2>`).
    block_device: Arc<dyn BlockDevice>,
}

impl PageCacheBackend for InodeTableBackend {
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter> {
        let bid = Bid::new(self.inode_table_bid as u64) + idx as u64;
        let bio_segment = BioSegment::new_from_segment(
            Segment::from(frame.clone()).into(),
            BioDirection::FromDevice,
        );
        Ok(self.block_device.read_blocks_async(bid, bio_segment)?)
    }

    fn write_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter> {
        let bid = Bid::new(self.inode_table_bid as u64) + idx as u64;
        let bio_segment = BioSegment::new_from_segment(
            Segment::from(frame.clone()).into(),
            BioDirection::ToDevice,
        );
        Ok(self.block_device.write_blocks_async(bid, bio_segment)?)
    }

    fn npages(&self) -> usize {
        self.raw_inodes_size.div_ceil(BLOCK_SIZE)
    }
}

/// A single Ext2 block group.
///
/// Owns all per-group state: descriptor, bitmaps, inode table cache,
/// and the block device handle needed for I/O. Provides self-contained
/// operations for block/inode allocation, deallocation, and inode
/// descriptor read/write within this group.
pub struct BlockGroup {
    /// Block group index (0-based).
    idx: usize,
    /// Group descriptor with dirty tracking.
    desc: RwMutex<Dirty<GroupDesc>>,
    /// Block bitmap cached in memory.
    block_bitmap: RwMutex<Dirty<IdBitmap>>,
    /// Inode bitmap cached in memory.
    inode_bitmap: RwMutex<Dirty<IdBitmap>>,
    /// Backing block device (shared with Ext2 and other groups).
    block_device: Arc<dyn BlockDevice>,
    /// Cached geometry: first filesystem-wide block number of this group.
    first_block: u32,
    /// Cached geometry: last filesystem-wide block number of this group.
    last_block: u32,
    /// Cached geometry: inode table blocks per group.
    itb_per_group: u32,
    /// Cached geometry: inodes per group.
    inodes_per_group: u32,
    /// Cached geometry: inode size in bytes.
    inode_size: usize,
    /// Inode table page cache backend.
    _inode_table_backend: Arc<InodeTableBackend>,
    /// Inode table page cache.
    inode_table_cache: PageCache,
    /// Per-group inode cache keyed by group-local inode index.
    ///
    /// Linux equivalent is VFS global inode hash (fs/inode.c:63).
    /// Asterinas keeps per-group cache because VFS does not provide inode caching.
    inode_cache: RwMutex<BTreeMap<u32, Arc<Inode>>>,
}

impl fmt::Debug for BlockGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockGroup")
            .field("idx", &self.idx)
            .finish()
    }
}

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

/// In-memory block group descriptor.
#[derive(Clone, Copy, Debug)]
pub(super) struct GroupDesc {
    pub block_bitmap: Ext2Bid,
    pub inode_bitmap: Ext2Bid,
    pub inode_table: Ext2Bid,
    pub free_blocks_count: u16,
    pub free_inodes_count: u16,
    pub used_dirs_count: u16,
}

impl From<RawGroupDesc> for GroupDesc {
    fn from(raw: RawGroupDesc) -> Self {
        Self {
            block_bitmap: raw.block_bitmap,
            inode_bitmap: raw.inode_bitmap,
            inode_table: raw.inode_table,
            free_blocks_count: raw.free_blocks_count,
            free_inodes_count: raw.free_inodes_count,
            used_dirs_count: raw.used_dirs_count,
        }
    }
}

impl From<GroupDesc> for RawGroupDesc {
    fn from(desc: GroupDesc) -> Self {
        Self {
            block_bitmap: desc.block_bitmap,
            inode_bitmap: desc.inode_bitmap,
            inode_table: desc.inode_table,
            free_blocks_count: desc.free_blocks_count,
            free_inodes_count: desc.free_inodes_count,
            used_dirs_count: desc.used_dirs_count,
            pad: 0,
            reserved: [0; 3],
        }
    }
}

impl BlockGroup {
    /// Loads a block group from the descriptor table.
    ///
    /// Now takes `Arc<dyn BlockDevice>` directly instead of `Weak<Ext2>`.
    /// Caches per-group geometry from `SuperBlock` at load time.
    pub fn load(
        group_descs: &USegment,
        idx: usize,
        sb: &SuperBlock,
        block_device: Arc<dyn BlockDevice>,
    ) -> Result<Self> {
        let offset = idx * size_of::<RawGroupDesc>();
        let raw = group_descs
            .read_val::<RawGroupDesc>(offset)
            .map_err(|_| Error::with_message(Errno::EIO, "failed to read group descriptor"))?;
        let desc = GroupDesc::from(raw);

        // Cache geometry from SuperBlock at load time.
        let first_block = sb.group_first_block_no(idx);
        let last_block = sb.group_last_block_no(idx);
        let itb_per_group = sb.itb_per_group();
        let inodes_per_group = sb.inodes_per_group();
        let inode_size = sb.inode_size();

        // SPEC: load and validate bitmaps once during mount, keep them cached in memory.
        let block_bitmap = Self::load_block_bitmap(
            block_device.as_ref(),
            first_block,
            last_block,
            itb_per_group,
            &desc,
        )?;
        let inode_bitmap = Self::load_inode_bitmap(block_device.as_ref(), inodes_per_group, &desc)?;

        // Create PageCache for inode table backed by InodeTableBackend.
        let raw_inodes_size = (inodes_per_group as usize) * inode_size;
        let backend = Arc::new(InodeTableBackend {
            inode_table_bid: desc.inode_table,
            raw_inodes_size,
            block_device: block_device.clone(),
        });
        let inode_table_cache =
            PageCache::with_capacity(raw_inodes_size, Arc::downgrade(&backend) as _)?;

        Ok(Self {
            idx,
            desc: RwMutex::new(Dirty::new(desc)),
            block_bitmap: RwMutex::new(Dirty::new(block_bitmap)),
            inode_bitmap: RwMutex::new(Dirty::new(inode_bitmap)),
            block_device,
            first_block,
            last_block,
            itb_per_group,
            inodes_per_group,
            inode_size,
            _inode_table_backend: backend,
            inode_table_cache,
            inode_cache: RwMutex::new(BTreeMap::new()),
        })
    }

    /// Looks up an allocated inode by group-local index and returns cached/in-memory object.
    ///
    /// Linux: /root/linux/fs/inode.c:1371 (iget_locked hash lookup + allocate on miss)
    /// Linux: /root/linux/fs/ext2/inode.c:1387 (ext2_iget)
    pub(super) fn lookup_inode(
        &self,
        inode_idx: u32,
        ino: u32,
        fs: Weak<Ext2>,
    ) -> Result<Arc<Inode>> {
        let inode_bit = u16::try_from(inode_idx)
            .map_err(|_| Error::with_message(Errno::EINVAL, "inode index out of range"))?;

        {
            let inode_bitmap = self.inode_bitmap.read();
            if !inode_bitmap.is_allocated(inode_bit) {
                return_errno!(Errno::ENOENT);
            }
        }

        // Fast path: cache hit under read lock.
        if let Some(inode) = self.inode_cache.read().get(&inode_idx) {
            return Ok(inode.clone());
        }

        // Slow path: double-check under write lock and load on miss.
        let mut inode_cache = self.inode_cache.write();
        if let Some(inode) = inode_cache.get(&inode_idx) {
            return Ok(inode.clone());
        }

        {
            let inode_bitmap = self.inode_bitmap.read();
            if !inode_bitmap.is_allocated(inode_bit) {
                return_errno!(Errno::ENOENT);
            }
        }

        let desc = self.read_inode_desc(inode_idx)?;
        let desc = Dirty::new(desc);
        let inode = Inode::new(ino, desc.type_(), desc, self.idx, fs);
        inode_cache.insert(inode_idx, inode.clone());
        Ok(inode)
    }

    /// Inserts a fully initialized inode into this group's cache.
    pub(super) fn insert_cache(&self, inode_idx: u32, inode: Arc<Inode>) {
        self.inode_cache.write().insert(inode_idx, inode);
    }

    /// Removes one inode from the per-group live cache.
    ///
    /// Linux analogue: /root/linux/fs/inode.c:1910 (iput_final)
    pub(super) fn remove_inode_cache(&self, inode_idx: u32) -> Option<Arc<Inode>> {
        self.inode_cache.write().remove(&inode_idx)
    }

    /// Syncs per-group inode state and bitmap metadata.
    ///
    /// Linux trigger analogue: /root/linux/fs/inode.c:1910 (iput_final)
    pub(super) fn sync_all(&self, group_descs: &USegment) -> Result<()> {
        self.sync_inodes()?;
        self.sync_metadata(group_descs)?;
        Ok(())
    }

    /// Syncs cached inodes and evicts unreferenced entries.
    fn sync_inodes(&self) -> Result<()> {
        // Phase 1: remove unreferenced inodes from cache.
        let unused_inodes: Vec<Arc<Inode>> = self
            .inode_cache
            .write()
            .extract_if(.., |_, inode| Arc::strong_count(inode) == 1)
            .map(|(_, inode)| inode)
            .collect();

        // Phase 2: sync removed live inodes without holding the cache lock.
        for inode in &unused_inodes {
            inode.sync_all(false)?;
        }

        // Phase 3: sync still-referenced cached inodes.
        let remaining_inodes: Vec<Arc<Inode>> = self.inode_cache.read().values().cloned().collect();
        for inode in &remaining_inodes {
            inode.sync_all(false)?;
        }

        //Phase 4: sync inode table page cache.
        self.sync_inode_table()?;
        Ok(())
    }

    pub(super) fn sync_inode_table(&self) -> Result<()> {
        let size = self.inodes_per_group as usize * self.inode_size;
        let range = 0..size;
        self.inode_table_cache.evict_range(range)
    }

    pub fn block_bitmap(&self) -> RwMutexReadGuard<'_, Dirty<IdBitmap>> {
        self.block_bitmap.read()
    }

    pub fn block_bitmap_mut(&self) -> RwMutexWriteGuard<'_, Dirty<IdBitmap>> {
        self.block_bitmap.write()
    }

    pub fn inode_bitmap(&self) -> RwMutexReadGuard<'_, Dirty<IdBitmap>> {
        self.inode_bitmap.read()
    }

    pub fn inode_bitmap_mut(&self) -> RwMutexWriteGuard<'_, Dirty<IdBitmap>> {
        self.inode_bitmap.write()
    }

    pub fn idx(&self) -> usize {
        self.idx
    }

    pub fn block_bitmap_bid(&self) -> Ext2Bid {
        self.desc.read().block_bitmap
    }

    pub fn inode_bitmap_bid(&self) -> Ext2Bid {
        self.desc.read().inode_bitmap
    }

    pub fn inode_table_bid(&self) -> Ext2Bid {
        self.desc.read().inode_table
    }

    pub fn inode_table_cache(&self) -> &PageCache {
        &self.inode_table_cache
    }

    /// Returns the first filesystem-wide block number of this group.
    pub fn first_block(&self) -> u32 {
        self.first_block
    }

    /// Returns the last filesystem-wide block number of this group.
    pub fn last_block(&self) -> u32 {
        self.last_block
    }

    pub fn free_blocks_count(&self) -> u16 {
        self.desc.read().free_blocks_count
    }

    pub fn free_inodes_count(&self) -> u16 {
        self.desc.read().free_inodes_count
    }

    pub fn used_dirs_count(&self) -> u16 {
        self.desc.read().used_dirs_count
    }

    /// Decreases the free-block counter for this group.
    pub(super) fn dec_free_blocks(&self, count: u16) {
        let mut desc = self.desc.write();
        desc.free_blocks_count = desc.free_blocks_count.saturating_sub(count);
    }

    /// Increases the free-block counter for this group.
    pub(super) fn inc_free_blocks(&self, count: u16) {
        let mut desc = self.desc.write();
        desc.free_blocks_count = desc.free_blocks_count.saturating_add(count);
    }

    /// Decreases the free-inode counter for this group.
    pub(super) fn dec_free_inodes(&self, count: u16) {
        let mut desc = self.desc.write();
        desc.free_inodes_count = desc.free_inodes_count.saturating_sub(count);
    }

    /// Increases the free-inode counter for this group.
    pub(super) fn inc_free_inodes(&self, count: u16) {
        let mut desc = self.desc.write();
        desc.free_inodes_count = desc.free_inodes_count.saturating_add(count);
    }

    /// Increases the used-dirs counter for this group.
    pub(super) fn inc_used_dirs(&self) {
        let mut desc = self.desc.write();
        desc.used_dirs_count = desc.used_dirs_count.saturating_add(1);
    }

    /// Decreases the used-dirs counter for this group.
    pub(super) fn dec_used_dirs(&self) {
        let mut desc = self.desc.write();
        desc.used_dirs_count = desc.used_dirs_count.saturating_sub(1);
    }

    pub(super) fn is_desc_dirty(&self) -> bool {
        self.desc.read().is_dirty()
    }

    pub(super) fn is_bitmap_dirty(&self) -> bool {
        self.block_bitmap.read().is_dirty() || self.inode_bitmap.read().is_dirty()
    }

    fn sync_metadata(&self, group_descs: &USegment) -> Result<()> {
        self.sync_bitmaps()?;
        self.sync_group_desc(group_descs)
    }

    fn sync_group_desc(&self, group_descs: &USegment) -> Result<()> {
        if !self.desc.read().is_dirty() {
            return Ok(());
        }

        let mut desc = self.desc.write();
        if !desc.is_dirty() {
            return Ok(());
        }

        let raw = RawGroupDesc::from(**desc);
        let offset = self.idx * size_of::<RawGroupDesc>();
        group_descs.write_val(offset, &raw)?;
        desc.clear_dirty();
        Ok(())
    }

    fn sync_bitmaps(&self) -> Result<()> {
        let (block_bitmap_bid, inode_bitmap_bid) = {
            // SPEC: read descriptor block addresses before bitmap locks to keep lock ordering.
            let desc = self.desc.read();
            (desc.block_bitmap, desc.inode_bitmap)
        };

        if self.block_bitmap.read().is_dirty() {
            let mut block_bitmap = self.block_bitmap.write();
            if block_bitmap.is_dirty() {
                if self
                    .block_device
                    .write_bytes(
                        Bid::new(block_bitmap_bid as u64).to_offset(),
                        block_bitmap.as_bytes(),
                    )
                    .is_err()
                {
                    // SPEC: keep dirty bit set on writeback failure for retry.
                    return_errno_with_message!(Errno::EIO, "failed to write block bitmap");
                }
                block_bitmap.clear_dirty();
            }
        }

        if self.inode_bitmap.read().is_dirty() {
            let mut inode_bitmap = self.inode_bitmap.write();
            if inode_bitmap.is_dirty() {
                if self
                    .block_device
                    .write_bytes(
                        Bid::new(inode_bitmap_bid as u64).to_offset(),
                        inode_bitmap.as_bytes(),
                    )
                    .is_err()
                {
                    // SPEC: keep dirty bit set on writeback failure for retry.
                    return_errno_with_message!(Errno::EIO, "failed to write inode bitmap");
                }
                inode_bitmap.clear_dirty();
            }
        }

        Ok(())
    }

    /// Loads and validates the block bitmap for this group.
    ///
    /// Linux: /root/linux/fs/ext2/balloc.c:129 (read_block_bitmap)
    fn load_block_bitmap(
        block_device: &dyn BlockDevice,
        first_block: u32,
        last_block: u32,
        itb_per_group: u32,
        desc: &GroupDesc,
    ) -> Result<IdBitmap> {
        let bitmap_bid = desc.block_bitmap;

        let mut buf = vec![0u8; BLOCK_SIZE];
        if block_device
            .read_bytes(Bid::new(bitmap_bid as u64).to_offset(), &mut buf)
            .is_err()
        {
            return_errno_with_message!(Errno::EIO, "failed to read block bitmap");
        }

        if last_block < first_block {
            return_errno_with_message!(Errno::EINVAL, "block group has invalid block range");
        }
        let max_bit = last_block - first_block;
        let capacity = (max_bit + 1) as usize;
        if capacity > IdBitmap::capacity() as usize {
            return_errno_with_message!(Errno::EINVAL, "block bitmap capacity overflow");
        }
        let bitmap = IdBitmap::from_buf(buf.into_boxed_slice(), capacity as u16);

        let valid_block_bitmap = |first_block: u32,
                                  max_bit: u32,
                                  desc: &GroupDesc,
                                  bitmap: &IdBitmap|
         -> Result<()> {
            let block_bitmap = desc.block_bitmap;
            let inode_bitmap = desc.inode_bitmap;
            let inode_table = desc.inode_table;

            let mut offset = block_bitmap.wrapping_sub(first_block);
            if block_bitmap < first_block || offset > max_bit {
                return_errno_with_message!(Errno::EINVAL, "block bitmap block out of group range");
            }
            if !bitmap.is_allocated(offset as u16) {
                return_errno_with_message!(
                    Errno::EINVAL,
                    "block bitmap block not marked in bitmap"
                );
            }

            offset = inode_bitmap.wrapping_sub(first_block);
            if inode_bitmap < first_block || offset > max_bit {
                return_errno_with_message!(Errno::EINVAL, "inode bitmap block out of group range");
            }
            if !bitmap.is_allocated(offset as u16) {
                return_errno_with_message!(
                    Errno::EINVAL,
                    "inode bitmap block not marked in bitmap"
                );
            }

            offset = inode_table.wrapping_sub(first_block);
            if inode_table < first_block || offset > max_bit {
                return_errno_with_message!(Errno::EINVAL, "inode table start out of group range");
            }
            let table_last = offset + itb_per_group - 1;
            if table_last > max_bit {
                return_errno_with_message!(Errno::EINVAL, "inode table extends beyond group");
            }

            let end = offset + itb_per_group;
            let mut bit = offset;
            while bit < end {
                if !bitmap.is_allocated(bit as u16) {
                    return_errno_with_message!(
                        Errno::EINVAL,
                        "inode table block not marked in bitmap"
                    );
                }
                bit += 1;
            }
            Ok(())
        };

        valid_block_bitmap(first_block, max_bit, desc, &bitmap)?;

        Ok(bitmap)
    }

    /// Loads the inode bitmap for this group.
    ///
    /// Linux: /root/linux/fs/ext2/ialloc.c:31 (read_inode_bitmap)
    fn load_inode_bitmap(
        block_device: &dyn BlockDevice,
        inodes_per_group: u32,
        desc: &GroupDesc,
    ) -> Result<IdBitmap> {
        let bitmap_bid = desc.inode_bitmap;

        let mut buf = vec![0u8; BLOCK_SIZE];
        if block_device
            .read_bytes(Bid::new(bitmap_bid as u64).to_offset(), &mut buf)
            .is_err()
        {
            return_errno_with_message!(Errno::EIO, "failed to read inode bitmap");
        }

        let capacity = inodes_per_group as usize;
        if capacity > IdBitmap::capacity() as usize {
            return_errno_with_message!(Errno::EINVAL, "inode bitmap capacity overflow");
        }

        Ok(IdBitmap::from_buf(buf.into_boxed_slice(), capacity as u16))
    }

    /// Attempts to allocate up to `count` contiguous blocks within this group.
    ///
    /// Returns `Ok((Some(range), saw_corruption))` with filesystem-wide block numbers
    /// on success, `Ok((None, saw_corruption))` if no allocatable blocks.
    ///
    /// Linux: /root/linux/fs/ext2/balloc.c:682 (ext2_try_to_allocate)
    pub(super) fn alloc_blocks(
        &self,
        count: u32,
        sb_free_blocks: u32,
    ) -> Result<(Option<Range<u32>>, bool)> {
        let group_size = self.last_block - self.first_block + 1;
        if group_size as usize > BLOCK_SIZE * 8 {
            return_errno_with_message!(Errno::EIO, "block group size exceeds bitmap capacity");
        }

        let mut saw_corruption = false;

        let mut bitmap = self.block_bitmap.write();

        // Corruption check: descriptor says free blocks but bitmap disagrees.
        if self.free_blocks_count() > 0 {
            if let Some(range) = bitmap.alloc_consecutive(1) {
                bitmap.free_consecutive(range);
            } else {
                saw_corruption = true;
            }
        }

        let mut rejected = Vec::new();
        let mut req = count.min(group_size) as u16;
        while req > 0 {
            let Some(range) = bitmap.alloc_consecutive(req) else {
                req /= 2;
                continue;
            };
            let alloc_len = range.len() as u32;
            let run_start = range.start as u32;
            let ret_block = self.first_block + run_start;

            if self.overlaps_system_zone(ret_block, alloc_len) {
                saw_corruption = true;
                rejected.push(range);
                continue;
            }
            if self.free_blocks_count() < alloc_len as u16 || sb_free_blocks < alloc_len {
                saw_corruption = true;
                rejected.push(range);
                continue;
            }

            // Restore any previously rejected ranges.
            for rejected_range in rejected.drain(..) {
                bitmap.free_consecutive(rejected_range);
            }

            drop(bitmap);

            // SPEC: persistent in-memory bitmap cache; writeback is deferred to sync_metadata.

            self.dec_free_blocks(alloc_len as u16);

            let range = ret_block..ret_block + alloc_len;
            return Ok((Some(range), saw_corruption));
        }

        // No allocation possible; restore any rejected ranges.
        for rejected_range in rejected.drain(..) {
            bitmap.free_consecutive(rejected_range);
        }

        Ok((None, saw_corruption))
    }

    /// Frees a range of blocks within this group.
    ///
    /// `bit` is the group-relative start index, `group_count` is the number
    /// of blocks to free. Returns the number of blocks actually freed.
    ///
    /// Linux: /root/linux/fs/ext2/balloc.c:482 (ext2_free_blocks, per-group portion)
    pub(super) fn free_blocks(&self, bit: u32, group_count: u32) -> Result<u32> {
        // Validate system zone overlap using filesystem-wide coordinates.
        let abs_start = self.first_block + bit;
        if self.overlaps_system_zone(abs_start, group_count) {
            return_errno_with_message!(Errno::EIO, "freeing blocks in system zone");
        }

        let mut bitmap = self.block_bitmap.write();

        // Linux: balloc.c:542-556 — per-bit clear, only count bits that actually
        // transitioned allocated→free (group_freed pattern).
        let range_start = bit as u16;
        let range_end = (bit + group_count) as u16;
        let mut actually_freed: u32 = 0;
        for idx in range_start..range_end {
            if !bitmap.is_allocated(idx) {
                warn!(
                    "ext2_free_blocks: bit already cleared for block {}",
                    abs_start + (idx - range_start) as u32
                );
            } else {
                bitmap.free(idx);
                actually_freed += 1;
            }
        }

        drop(bitmap);

        // SPEC: persistent in-memory bitmap cache; writeback is deferred to sync_metadata.

        self.inc_free_blocks(actually_freed as u16);
        Ok(actually_freed)
    }

    /// Attempts to allocate one inode within this group.
    ///
    /// Returns `Ok(Some(inode_idx))` with the 0-based group-relative inode index,
    /// or `Ok(None)` if no free inode. Does NOT update counters.
    ///
    /// Linux: /root/linux/fs/ext2/ialloc.c:419 (ext2_new_inode, per-group portion)
    pub(super) fn alloc_inode(&self) -> Result<Option<u16>> {
        let mut bitmap = self.inode_bitmap.write();
        let Some(inode_idx) = bitmap.alloc() else {
            return Ok(None);
        };
        drop(bitmap);

        // SPEC: persistent in-memory bitmap cache; writeback is deferred to sync_metadata.

        Ok(Some(inode_idx))
    }

    /// Frees one inode within this group.
    ///
    /// `bit` is the 0-based group-relative inode index.
    /// Returns `true` if the bit transitioned allocated→free,
    /// `false` if it was already free (logs warning). Does NOT update counters.
    ///
    /// Linux: /root/linux/fs/ext2/ialloc.c:79 (ext2_free_inode, per-group portion)
    pub(super) fn free_inode(&self, bit: u16) -> Result<bool> {
        let mut bitmap = self.inode_bitmap.write();
        if !bitmap.is_allocated(bit) {
            warn!("ext2_free_inode: inode bit {} already freed", bit);
            return Ok(false);
        }
        bitmap.free(bit);
        drop(bitmap);

        // SPEC: persistent in-memory bitmap cache; writeback is deferred to sync_metadata.

        Ok(true)
    }

    /// Reads an inode descriptor from the group's inode table PageCache.
    ///
    /// `index_in_group` is the 0-based inode index within this group.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1314 (ext2_get_inode)
    pub(super) fn read_inode_desc(&self, index_in_group: u32) -> Result<InodeDesc> {
        let offset_bytes = (index_in_group as usize) * self.inode_size;
        let raw: RawInode = self.inode_table_cache.pages().read_val(offset_bytes)?;
        InodeDesc::try_from(&raw)
    }

    /// Writes an inode descriptor to the group's inode table PageCache (deferred writeback).
    ///
    /// `index_in_group` is the 0-based inode index within this group.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1512 (__ext2_write_inode / mark_buffer_dirty)
    pub(super) fn write_inode_desc(&self, index_in_group: u32, raw: &RawInode) -> Result<()> {
        let offset_bytes = (index_in_group as usize) * self.inode_size;
        self.inode_table_cache
            .pages()
            .write_val(offset_bytes, raw)?;
        Ok(())
    }

    /// Checks whether [start, start+count-1] overlaps any system metadata block
    /// (block bitmap, inode bitmap, inode table) of this group.
    ///
    /// Linux: /root/linux/fs/ext2/balloc.c:115 (ext2_bg_has_super + system zone check)
    fn overlaps_system_zone(&self, start: u32, count: u32) -> bool {
        let Some(end) = start.checked_add(count - 1) else {
            return true;
        };

        let desc = self.desc.read();
        let block_bitmap = desc.block_bitmap;
        let inode_bitmap = desc.inode_bitmap;
        let inode_table = desc.inode_table;
        drop(desc);

        if Self::ranges_overlap(start, end, block_bitmap, 1) {
            return true;
        }
        if Self::ranges_overlap(start, end, inode_bitmap, 1) {
            return true;
        }
        if Self::ranges_overlap(start, end, inode_table, self.itb_per_group) {
            return true;
        }
        false
    }

    /// Returns whether [start, end] overlaps [zone_start, zone_start+zone_len-1].
    fn ranges_overlap(start: u32, end: u32, zone_start: u32, zone_len: u32) -> bool {
        if zone_len == 0 {
            return false;
        }
        let Some(zone_end) = zone_start.checked_add(zone_len - 1) else {
            return true;
        };
        !(end < zone_start || start > zone_end)
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::*;
    use crate::fs::ext2::testkit::{
        Ext2FixtureBuilder, make_valid_group_desc, make_valid_super_block,
    };

    #[ktest]
    fn load_and_accessors_ok() {
        let sb = make_valid_super_block(2);
        let descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();
        let fixture = Ext2FixtureBuilder::new(2, 256)
            .with_metadata_block_bitmap()
            .with_free_blocks(32, 32)
            .with_free_inodes(64, 64)
            .with_root()
            .build()
            .unwrap();
        let group = fixture.block_group(1);

        assert_eq!(group.idx(), 1);
        assert_eq!(group.block_bitmap_bid(), descs[1].block_bitmap);
        assert_eq!(group.inode_bitmap_bid(), descs[1].inode_bitmap);
        assert_eq!(group.inode_table_bid(), descs[1].inode_table);
        assert_eq!(group.free_blocks_count(), descs[1].free_blocks_count);
        assert_eq!(group.free_inodes_count(), descs[1].free_inodes_count);
        assert_eq!(group.used_dirs_count(), descs[1].used_dirs_count);
    }

    #[ktest]
    fn free_block_counter_update_marks_dirty() {
        // Counter update helpers should adjust value and mark descriptor dirty.
        let fixture = Ext2FixtureBuilder::new(2, 256)
            .with_metadata_block_bitmap()
            .with_free_blocks(20, 20)
            .with_free_inodes(64, 64)
            .build()
            .unwrap();
        let group = fixture.block_group(0);
        assert_eq!(group.free_blocks_count(), 20);
        assert!(!group.is_desc_dirty());

        group.dec_free_blocks(3);
        assert_eq!(group.free_blocks_count(), 17);
        assert!(group.is_desc_dirty());

        group.inc_free_blocks(2);
        assert_eq!(group.free_blocks_count(), 19);
        assert!(group.is_desc_dirty());
    }
}
