// SPDX-License-Identifier: MPL-2.0

use alloc::{string::String, sync::Arc, vec, vec::Vec};
use core::{
    fmt,
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use aster_block::{
    bio::{BioEnqueueError, BioStatus, BioType, SubmittedBio},
    id::Bid,
    BlockDevice, BlockDeviceMeta, BLOCK_SIZE, SECTOR_SIZE,
};
use device_id::{DeviceId, MajorId, MinorId};
use ostd::{
    mm::{io::util::HasVmReaderWriter, FrameAllocOptions, Segment, USegment, VmIo, PAGE_SIZE},
    prelude::*,
};

use super::{
    block_group::RawGroupDesc,
    dir::{DOT_BYTE, DOT_DOT_BYTE},
    fs::{Ext2, ROOT_INO},
    inode::{FilePerm, Inode, RawInode},
    super_block::{
        ErrorsBehavior, FsState, OsId, RawSuperBlock, RevLevel, MAGIC_NUM, SUPER_BLOCK_OFFSET,
    },
};
use crate::{
    fs::{
        ext2::dir::DirEntryHeader,
        file::{InodeType, StatusFlags},
        fs_impls::ext2::super_block::SuperBlock,
        utils::DirentVisitor,
        vfs::inode::InodeIo,
    },
    prelude::{return_errno_with_message, Errno, Result, *},
    time::clocks,
};

// ---------------------------------------------------------------------------
// Mock disk types
// ---------------------------------------------------------------------------

pub(super) struct Ext2MemoryDisk {
    segment: Segment<()>,
    flush_count: AtomicUsize,
    fail_flush: AtomicBool,
}

impl Ext2MemoryDisk {
    pub(super) fn new(nblocks: usize) -> Self {
        let npages = (nblocks * BLOCK_SIZE).div_ceil(PAGE_SIZE);
        let segment = FrameAllocOptions::new()
            .zeroed(true)
            .alloc_segment(npages)
            .unwrap();
        Self {
            segment,
            flush_count: AtomicUsize::new(0),
            fail_flush: AtomicBool::new(false),
        }
    }

    pub(super) fn segment(&self) -> &Segment<()> {
        &self.segment
    }

    pub(super) fn write_super_block(&self, raw: &RawSuperBlock) {
        self.segment.write_val(SUPER_BLOCK_OFFSET, raw).unwrap();
    }

    pub(super) fn write_group_desc_table(&self, sb: &SuperBlock, descs: &[RawGroupDesc]) {
        let table_offset = Bid::new(sb.group_descriptors_bid(0) as u64).to_offset();
        for (idx, desc) in descs.iter().enumerate() {
            let offset = table_offset + idx * size_of::<RawGroupDesc>();
            self.segment.write_val(offset, desc).unwrap();
        }
    }

    pub(super) fn flush_count(&self) -> usize {
        self.flush_count.load(Ordering::Relaxed)
    }

    pub(super) fn set_flush_error(&self, should_fail: bool) {
        self.fail_flush.store(should_fail, Ordering::Relaxed);
    }
}

impl fmt::Debug for Ext2MemoryDisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ext2MemoryDisk")
            .field("bytes", &self.segment.size())
            .finish()
    }
}

impl BlockDevice for Ext2MemoryDisk {
    fn enqueue(&self, bio: SubmittedBio) -> core::result::Result<(), BioEnqueueError> {
        if bio.type_() == BioType::Flush {
            self.flush_count.fetch_add(1, Ordering::Relaxed);
            let status = if self.fail_flush.load(Ordering::Relaxed) {
                BioStatus::IoError
            } else {
                BioStatus::Complete
            };
            bio.complete(status);
            return Ok(());
        }

        let mut cur_device_ofs = bio.sid_range().start.to_raw() as usize * SECTOR_SIZE;

        for seg in bio.segments() {
            let io_size = match bio.type_() {
                BioType::Read => seg
                    .inner_dma_slice()
                    .writer()
                    .unwrap()
                    .write(self.segment.reader().skip(cur_device_ofs)),
                BioType::Write => self
                    .segment
                    .writer()
                    .skip(cur_device_ofs)
                    .write(&mut seg.inner_dma_slice().reader().unwrap()),
                _ => {
                    bio.complete(BioStatus::NotSupported);
                    return Ok(());
                }
            };
            cur_device_ofs += io_size;
        }

        bio.complete(BioStatus::Complete);
        Ok(())
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: usize::MAX,
            nr_sectors: self.segment.size() / SECTOR_SIZE,
        }
    }

    fn name(&self) -> &str {
        "ext2-memory-disk"
    }

    fn id(&self) -> DeviceId {
        DeviceId::new(MajorId::new(1), MinorId::new(0))
    }
}

#[derive(Debug)]
pub(super) struct ErrorBioDisk {
    read_status: BioStatus,
    nr_sectors: usize,
    fail_read_offset: Option<usize>,
    inner: Option<Arc<Ext2MemoryDisk>>,
}

impl ErrorBioDisk {
    pub(super) fn new(read_status: BioStatus, nr_sectors: usize) -> Self {
        Self {
            read_status,
            nr_sectors,
            fail_read_offset: None,
            inner: None,
        }
    }

    pub(super) fn with_read_error_at(
        inner: Arc<Ext2MemoryDisk>,
        read_status: BioStatus,
        fail_read_offset: usize,
    ) -> Self {
        Self {
            read_status,
            nr_sectors: inner.segment().size() / SECTOR_SIZE,
            fail_read_offset: Some(fail_read_offset),
            inner: Some(inner),
        }
    }
}

impl BlockDevice for ErrorBioDisk {
    fn enqueue(&self, bio: SubmittedBio) -> core::result::Result<(), BioEnqueueError> {
        let mut cur_device_ofs = bio.sid_range().start.to_raw() as usize * SECTOR_SIZE;

        for seg in bio.segments() {
            let io_size = match bio.type_() {
                BioType::Read => {
                    if let Some(fail_read_offset) = self.fail_read_offset {
                        if cur_device_ofs == fail_read_offset {
                            bio.complete(self.read_status);
                            return Ok(());
                        }
                        if let Some(inner) = self.inner.as_deref() {
                            let mut reader = inner.segment().reader();
                            let reader = reader.skip(cur_device_ofs);
                            seg.inner_dma_slice().writer().unwrap().write(reader)
                        } else {
                            bio.complete(BioStatus::IoError);
                            return Ok(());
                        }
                    } else {
                        bio.complete(self.read_status);
                        return Ok(());
                    }
                }
                BioType::Write => {
                    if let Some(inner) = self.inner.as_deref() {
                        let mut writer = inner.segment().writer();
                        let writer = writer.skip(cur_device_ofs);
                        writer.write(&mut seg.inner_dma_slice().reader().unwrap())
                    } else {
                        bio.complete(BioStatus::Complete);
                        return Ok(());
                    }
                }
                _ => {
                    bio.complete(BioStatus::NotSupported);
                    return Ok(());
                }
            };
            cur_device_ofs += io_size;
        }

        bio.complete(BioStatus::Complete);
        Ok(())
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: usize::MAX,
            nr_sectors: self.nr_sectors,
        }
    }

    fn name(&self) -> &str {
        "ext2-error-disk"
    }

    fn id(&self) -> DeviceId {
        DeviceId::new(MajorId::new(1), MinorId::new(1))
    }
}

// ---------------------------------------------------------------------------
// Factory functions
// ---------------------------------------------------------------------------

pub(super) fn make_valid_raw_super_block(groups_count: u32) -> RawSuperBlock {
    let mut raw = RawSuperBlock::default();
    raw.magic = MAGIC_NUM;
    raw.log_block_size = 2;
    raw.log_frag_size = 2;
    raw.state = FsState::VALID.bits();
    raw.errors = ErrorsBehavior::Continue as u16;
    raw.creator_os = OsId::Linux as u32;
    raw.rev_level = RevLevel::GoodOld as u32;
    raw.first_data_block = 1;
    raw.blocks_per_group = 128;
    raw.frags_per_group = raw.blocks_per_group;
    raw.inodes_per_group = 1024;
    raw.inodes_count = groups_count * raw.inodes_per_group;

    let tail_blocks = 64;
    raw.blocks_count = raw.first_data_block
        + 1
        + (groups_count.saturating_sub(1)) * raw.blocks_per_group
        + tail_blocks;
    raw
}

pub(super) fn make_valid_super_block(groups_count: u32) -> SuperBlock {
    SuperBlock::try_from(make_valid_raw_super_block(groups_count)).unwrap()
}

pub(super) fn make_valid_group_desc(sb: &SuperBlock, group_idx: usize) -> RawGroupDesc {
    let first = sb.group_first_block_no(group_idx);
    RawGroupDesc {
        block_bitmap: first,
        inode_bitmap: first + 1,
        inode_table: first + 2,
        free_blocks_count: 0,
        free_inodes_count: 0,
        used_dirs_count: 0,
        pad: 0,
        reserved: [0; 3],
    }
}

pub(super) fn build_group_desc_segment(sb: &SuperBlock, descs: &[RawGroupDesc]) -> USegment {
    let desc_bytes = (sb.block_groups_count() as usize) * size_of::<RawGroupDesc>();
    let npages = desc_bytes.div_ceil(BLOCK_SIZE);
    let segment = FrameAllocOptions::new()
        .zeroed(true)
        .alloc_segment(npages)
        .unwrap();

    for (idx, desc) in descs.iter().enumerate() {
        let offset = idx * size_of::<RawGroupDesc>();
        segment.write_val(offset, desc).unwrap();
    }

    segment.into()
}

// ---------------------------------------------------------------------------
// RawInodeBuilder
// ---------------------------------------------------------------------------

pub(super) struct RawInodeBuilder {
    mode: u16,
    link_count: u16,
    dtime: u32,
    size_lo: u32,
    sector_count: u32,
    flags: u32,
    block: [u32; 15],
}

impl RawInodeBuilder {
    pub(super) fn new(mode: u16) -> Self {
        Self {
            mode,
            link_count: 1,
            dtime: 0,
            size_lo: 0,
            sector_count: 0,
            flags: 0,
            block: [0; 15],
        }
    }

    pub(super) fn link_count(mut self, v: u16) -> Self {
        self.link_count = v;
        self
    }

    pub(super) fn dtime(mut self, v: u32) -> Self {
        self.dtime = v;
        self
    }

    pub(super) fn size_lo(mut self, v: u32) -> Self {
        self.size_lo = v;
        self
    }

    pub(super) fn sector_count(mut self, v: u32) -> Self {
        self.sector_count = v;
        self
    }

    pub(super) fn block_ptrs(mut self, v: [u32; 15]) -> Self {
        self.block = v;
        self
    }

    pub(super) fn build(self) -> RawInode {
        RawInode {
            mode: self.mode,
            uid: 0,
            size_lo: self.size_lo,
            atime: 0,
            ctime: 0,
            mtime: 0,
            dtime: self.dtime,
            gid: 0,
            link_count: self.link_count,
            sector_count: self.sector_count,
            flags: self.flags,
            osd1: 0,
            block: self.block,
            generation: 0,
            file_acl: 0,
            size_high: 0,
            faddr: 0,
            frag: 0,
            fsize: 0,
            pad1: 0,
            uid_high: 0,
            gid_high: 0,
            reserved2: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Encodes a directory entry into a buffer at the given offset.
///
/// Shared helper for `dir.rs` and `inode.rs` tests.
pub(super) fn encode_dir_entry(
    buf: &mut [u8],
    offset: usize,
    inode: u32,
    rec_len: u16,
    name: &[u8],
    file_type: u8,
) {
    let header_len = size_of::<DirEntryHeader>();
    assert!(offset + rec_len as usize <= buf.len());
    assert!(name.len() <= (rec_len as usize).saturating_sub(header_len));

    buf[offset..offset + 4].copy_from_slice(&inode.to_le_bytes());
    buf[offset + 4..offset + 6].copy_from_slice(&rec_len.to_le_bytes());
    buf[offset + 6] = name.len() as u8;
    buf[offset + 7] = file_type;

    let name_start = offset + header_len;
    let name_end = name_start + name.len();
    buf[name_start..name_end].copy_from_slice(name);
}

#[derive(Default)]
pub(super) struct CollectDirentVisitor {
    pub entries: Vec<(String, u64, InodeType, usize)>,
}

impl DirentVisitor for CollectDirentVisitor {
    fn visit(&mut self, name: &str, ino: u64, type_: InodeType, offset: usize) -> Result<()> {
        self.entries.push((name.to_string(), ino, type_, offset));
        Ok(())
    }
}

pub(super) struct StopAfterVisitor {
    allow_count: usize,
    seen: usize,
}

impl StopAfterVisitor {
    pub(super) fn new(allow_count: usize) -> Self {
        Self {
            allow_count,
            seen: 0,
        }
    }
}

impl DirentVisitor for StopAfterVisitor {
    fn visit(&mut self, _name: &str, _ino: u64, _type_: InodeType, _offset: usize) -> Result<()> {
        if self.seen >= self.allow_count {
            return_errno_with_message!(Errno::EINTR, "operation interrupted");
        }
        self.seen += 1;
        Ok(())
    }
}

/// Writes one u32 pointer into an indirect block slot.
pub(super) fn write_indirect_ptr(disk: &Ext2MemoryDisk, bid: u32, index: u32, next: u32) {
    let offset = Bid::new(bid as u64).to_offset() + (index as usize) * size_of::<u32>();
    disk.segment().write_val(offset, &next).unwrap();
}

pub(super) fn write_raw_inode_to_disk(
    sb: &SuperBlock,
    descs: &[RawGroupDesc],
    ino: u32,
    raw: &RawInode,
    disk: &Ext2MemoryDisk,
) {
    let inodes_per_group = sb.inodes_per_group();
    let group_idx = ((ino - 1) / inodes_per_group) as usize;
    let index_in_group = (ino - 1) % inodes_per_group;

    let inode_size = sb.inode_size();
    let offset_bytes = (index_in_group as usize).saturating_mul(inode_size);
    let block_index = offset_bytes / BLOCK_SIZE;
    let offset_in_block = offset_bytes % BLOCK_SIZE;

    let table_block = descs[group_idx].inode_table + block_index as u32;
    let table_bid = Bid::new(table_block as u64);
    disk.segment()
        .write_val(table_bid.to_offset() + offset_in_block, raw)
        .unwrap();
}

// ---------------------------------------------------------------------------
// Bit manipulation helpers
// ---------------------------------------------------------------------------

pub(super) fn set_bit_lsb0(buf: &mut [u8], bit: usize) {
    let byte = bit / 8;
    let bit_in_byte = bit % 8;
    buf[byte] |= 1u8 << bit_in_byte;
}

// ---------------------------------------------------------------------------
// Bitmap write helpers
// ---------------------------------------------------------------------------

pub(super) fn write_block_bitmap(
    disk: &Ext2MemoryDisk,
    sb: &SuperBlock,
    desc: &RawGroupDesc,
    allocated_blocks: &[u32],
) {
    let first = sb.group_first_block_no(0);
    let last = sb.group_last_block_no(0);

    let mut bitmap_block = [0u8; BLOCK_SIZE];
    let mut mark_block = |block: u32| {
        if block < first || block > last {
            return;
        }
        set_bit_lsb0(&mut bitmap_block, (block - first) as usize);
    };

    // Required metadata/system blocks.
    mark_block(sb.group_descriptors_bid(0));
    mark_block(desc.block_bitmap);
    mark_block(desc.inode_bitmap);
    for block in desc.inode_table
        ..desc
            .inode_table
            .saturating_add(sb.inode_table_blocks_per_group())
    {
        mark_block(block);
    }

    for &block in allocated_blocks {
        mark_block(block);
    }

    disk.segment()
        .write_bytes(
            Bid::new(desc.block_bitmap as u64).to_offset(),
            &bitmap_block,
        )
        .unwrap();
}

pub(super) fn write_inode_bitmap(
    disk: &Ext2MemoryDisk,
    sb: &SuperBlock,
    desc: &RawGroupDesc,
    allocated_inodes: &[u32],
) {
    let mut bitmap = [0u8; BLOCK_SIZE];

    // Reserved inodes [1, first_ino) are always allocated.
    for bit in 0..(sb.first_ino() as usize).saturating_sub(1) {
        set_bit_lsb0(&mut bitmap, bit);
    }

    for &ino in allocated_inodes {
        if ino == 0 || ino > sb.inodes_per_group() {
            continue;
        }
        set_bit_lsb0(&mut bitmap, (ino - 1) as usize);
    }

    disk.segment()
        .write_bytes(Bid::new(desc.inode_bitmap as u64).to_offset(), &bitmap)
        .unwrap();
}

// ---------------------------------------------------------------------------
// Group 0 layout helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub(super) struct Group0Layout {
    pub group_desc_bid: u32,
    pub block_bitmap: u32,
    pub inode_bitmap: u32,
    pub inode_table: u32,
    pub first_data: u32,
}

pub(super) fn group0_layout(sb: &SuperBlock) -> Group0Layout {
    let group_desc_bid = sb.group_descriptors_bid(0);

    let mut next = sb.group_first_block_no(0);
    if next == group_desc_bid {
        next = next.saturating_add(1);
    }
    let block_bitmap = next;
    next = next.saturating_add(1);
    if next == group_desc_bid {
        next = next.saturating_add(1);
    }
    let inode_bitmap = next;
    next = next.saturating_add(1);
    if next == group_desc_bid {
        next = next.saturating_add(1);
    }
    let inode_table = next;
    let first_data = inode_table.saturating_add(sb.inode_table_blocks_per_group());

    Group0Layout {
        group_desc_bid,
        block_bitmap,
        inode_bitmap,
        inode_table,
        first_data,
    }
}

pub(super) fn validate_group0_layout(sb: &SuperBlock, layout: &Group0Layout) -> Result<()> {
    let first = sb.group_first_block_no(0);
    let last = sb.group_last_block_no(0);

    let in_range = |block: u32| block >= first && block <= last;
    if !in_range(layout.group_desc_bid)
        || !in_range(layout.block_bitmap)
        || !in_range(layout.inode_bitmap)
        || !in_range(layout.inode_table)
    {
        return_errno_with_message!(Errno::EINVAL, "test layout block out of group range");
    }

    let inode_table_last = layout
        .inode_table
        .saturating_add(sb.inode_table_blocks_per_group())
        .saturating_sub(1);
    if inode_table_last > last {
        return_errno_with_message!(Errno::EINVAL, "test layout inode table out of range");
    }

    if layout.block_bitmap == layout.inode_bitmap
        || layout.block_bitmap == layout.group_desc_bid
        || layout.inode_bitmap == layout.group_desc_bid
    {
        return_errno_with_message!(Errno::EINVAL, "test layout metadata blocks overlap");
    }

    if layout.block_bitmap >= layout.inode_table && layout.block_bitmap <= inode_table_last {
        return_errno_with_message!(
            Errno::EINVAL,
            "test layout block bitmap overlaps inode table"
        );
    }
    if layout.inode_bitmap >= layout.inode_table && layout.inode_bitmap <= inode_table_last {
        return_errno_with_message!(
            Errno::EINVAL,
            "test layout inode bitmap overlaps inode table"
        );
    }
    if layout.group_desc_bid >= layout.inode_table && layout.group_desc_bid <= inode_table_last {
        return_errno_with_message!(Errno::EINVAL, "test layout group desc overlaps inode table");
    }

    if layout.first_data <= first {
        return_errno_with_message!(Errno::EINVAL, "test layout first_data invalid");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Root directory helpers
// ---------------------------------------------------------------------------

fn make_root_raw_inode(root_bid: u32) -> RawInode {
    RawInodeBuilder::new(InodeType::Dir as u16 | 0o755)
        .link_count(2)
        .size_lo(BLOCK_SIZE as u32)
        .sector_count((BLOCK_SIZE / SECTOR_SIZE) as u32)
        .block_ptrs({
            let mut ptrs = [0u32; 15];
            ptrs[0] = root_bid;
            ptrs
        })
        .build()
}

pub(super) fn write_simple_root_dir_block(disk: &Ext2MemoryDisk, root_bid: u32) {
    let mut block = vec![0u8; BLOCK_SIZE];

    // '.'
    block[0..4].copy_from_slice(&ROOT_INO.to_le_bytes());
    block[4..6].copy_from_slice(&(12u16).to_le_bytes());
    block[6] = 1;
    block[7] = 2;
    block[8] = DOT_BYTE[0];

    // '..'
    block[12..16].copy_from_slice(&ROOT_INO.to_le_bytes());
    block[16..18].copy_from_slice(&((BLOCK_SIZE - 12) as u16).to_le_bytes());
    block[18] = 2;
    block[19] = 2;
    block[20..22].copy_from_slice(DOT_DOT_BYTE);

    disk.segment()
        .write_bytes(Bid::new(root_bid as u64).to_offset(), &block)
        .unwrap();
}

// ---------------------------------------------------------------------------
// Ext2Fixture and builder
// ---------------------------------------------------------------------------

pub(super) struct Ext2Fixture {
    pub disk: Arc<Ext2MemoryDisk>,
    pub ext2: Arc<Ext2>,
    pub sb: SuperBlock,
    pub descs: Vec<RawGroupDesc>,
}

type PreparedFixture = (
    RawSuperBlock,
    SuperBlock,
    Vec<RawGroupDesc>,
    Arc<Ext2MemoryDisk>,
    Group0Layout,
);

impl Ext2Fixture {
    /// Returns the root inode (ino 2).
    pub(super) fn root(&self) -> Arc<Inode> {
        self.ext2.read_inode(ROOT_INO).unwrap()
    }
}

pub(super) struct Ext2FixtureBuilder {
    groups: u32,
    nblocks: usize,
    sb_free_blocks: Option<u32>,
    sb_free_inodes: Option<u32>,
    group0_free_blocks: Option<u16>,
    group0_free_inodes: Option<u16>,
    group0_used_dirs: Option<u16>,
    init_root: bool,
    filled_block_bitmap: bool,
    filled_inode_bitmap: bool,
    init_metadata_block_bitmap: bool,
    init_reserved_inode_bitmap: bool,
    custom_device: Option<Arc<dyn BlockDevice>>,
}

impl Ext2FixtureBuilder {
    pub(super) fn new(groups: u32, nblocks: usize) -> Self {
        Self {
            groups,
            nblocks,
            sb_free_blocks: None,
            sb_free_inodes: None,
            group0_free_blocks: None,
            group0_free_inodes: None,
            group0_used_dirs: None,
            init_root: true,
            filled_block_bitmap: false,
            filled_inode_bitmap: false,
            init_metadata_block_bitmap: false,
            init_reserved_inode_bitmap: false,
            custom_device: None,
        }
    }

    pub(super) fn with_free_blocks(mut self, sb_free_blocks: u32, group0_free_blocks: u16) -> Self {
        self.sb_free_blocks = Some(sb_free_blocks);
        self.group0_free_blocks = Some(group0_free_blocks);
        self
    }

    pub(super) fn with_free_inodes(mut self, sb_free_inodes: u32, group0_free_inodes: u16) -> Self {
        self.sb_free_inodes = Some(sb_free_inodes);
        self.group0_free_inodes = Some(group0_free_inodes);
        self
    }

    pub(super) fn with_group0_used_dirs(mut self, used_dirs: u16) -> Self {
        self.group0_used_dirs = Some(used_dirs);
        self
    }

    pub(super) fn with_root(mut self) -> Self {
        self.init_root = true;
        self
    }

    pub(super) fn with_filled_block_bitmap(mut self, v: bool) -> Self {
        self.filled_block_bitmap = v;
        self
    }

    pub(super) fn with_filled_inode_bitmap(mut self, v: bool) -> Self {
        self.filled_inode_bitmap = v;
        self
    }

    /// Writes a minimal block bitmap marking only metadata blocks
    /// (block bitmap, inode bitmap, inode table).
    pub(super) fn with_metadata_block_bitmap(mut self) -> Self {
        self.init_metadata_block_bitmap = true;
        self
    }

    /// Writes reserved inode bits [0, first_ino) into the inode bitmap.
    pub(super) fn with_reserved_inode_bitmap(mut self) -> Self {
        self.init_reserved_inode_bitmap = true;
        self
    }

    /// Preset for namei (create/link/unlink/rename) tests:
    /// 1 group, 256 blocks, free blocks/inodes, root directory initialized.
    pub(super) fn namei_env() -> Self {
        Self::new(1, 256)
            .with_free_blocks(64, 64)
            .with_free_inodes(1000, 1000)
            .with_group0_used_dirs(1)
            .with_root()
    }

    /// Uses a custom block device for the `Ext2` instance instead of the memory disk.
    /// The memory disk is still used to prepare on-disk metadata.
    pub(super) fn with_device(mut self, device: Arc<dyn BlockDevice>) -> Self {
        self.custom_device = Some(device);
        self
    }

    /// Common setup: creates raw superblock, descriptors, and disk.
    fn prepare(&self) -> Result<PreparedFixture> {
        let mut raw_sb = make_valid_raw_super_block(self.groups);
        if let Some(sb_free_blocks) = self.sb_free_blocks {
            raw_sb.free_blocks_count = sb_free_blocks;
        }
        if let Some(sb_free_inodes) = self.sb_free_inodes {
            raw_sb.free_inodes_count = sb_free_inodes;
        }

        let sb = SuperBlock::try_from(raw_sb)?;
        let mut descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();

        // Stabilize group 0 layout so metadata never collides with group desc table.
        let layout = group0_layout(&sb);
        validate_group0_layout(&sb, &layout)?;
        descs[0].block_bitmap = layout.block_bitmap;
        descs[0].inode_bitmap = layout.inode_bitmap;
        descs[0].inode_table = layout.inode_table;

        if let Some(v) = self.group0_free_blocks {
            descs[0].free_blocks_count = v;
        }
        if let Some(v) = self.group0_free_inodes {
            descs[0].free_inodes_count = v;
        }
        if let Some(v) = self.group0_used_dirs {
            descs[0].used_dirs_count = v;
        }

        // Keep fixture disk capacity consistent with superblock geometry so
        // mount-time bitmap loading/validation can read all declared groups.
        let disk_blocks = self.nblocks.max(sb.total_blocks() as usize);
        let disk = Arc::new(Ext2MemoryDisk::new(disk_blocks));
        disk.write_super_block(&raw_sb);
        disk.write_group_desc_table(&sb, &descs);

        Ok((raw_sb, sb, descs, disk, layout))
    }

    /// Writes bitmaps to disk according to builder configuration.
    fn write_bitmaps(
        &self,
        sb: &SuperBlock,
        descs: &[RawGroupDesc],
        disk: &Ext2MemoryDisk,
        layout: &Group0Layout,
    ) {
        let root_bid = layout.first_data.saturating_add(1);

        // Initialize per-group block bitmaps with required metadata bits so mount-time
        // `ext2_valid_block_bitmap` checks pass for every group.
        for (group_idx, desc) in descs.iter().enumerate() {
            let first = sb.group_first_block_no(group_idx);
            let last = sb.group_last_block_no(group_idx);
            let mut bitmap_block = [0u8; BLOCK_SIZE];
            set_bit_lsb0(&mut bitmap_block, (desc.block_bitmap - first) as usize);
            set_bit_lsb0(&mut bitmap_block, (desc.inode_bitmap - first) as usize);

            // Group descriptor table block(s) in group 0 must stay allocated.
            if group_idx == 0 {
                let group_desc_bid = sb.group_descriptors_bid(0);
                if group_desc_bid >= first && group_desc_bid <= last {
                    set_bit_lsb0(&mut bitmap_block, (group_desc_bid - first) as usize);
                }
            }

            let itb = sb.inode_table_blocks_per_group();
            for i in 0..itb {
                set_bit_lsb0(&mut bitmap_block, (desc.inode_table + i - first) as usize);
            }
            disk.segment()
                .write_bytes(
                    Bid::new(desc.block_bitmap as u64).to_offset(),
                    &bitmap_block,
                )
                .unwrap();
        }

        if self.init_root {
            write_block_bitmap(disk, sb, &descs[0], &[root_bid]);
            write_inode_bitmap(disk, sb, &descs[0], &[ROOT_INO]);
            write_simple_root_dir_block(disk, root_bid);
        }

        if self.init_metadata_block_bitmap {
            let first = sb.group_first_block_no(0);
            let mut bitmap_block = [0u8; BLOCK_SIZE];
            // Mark actual metadata block positions relative to group start.
            set_bit_lsb0(&mut bitmap_block, (descs[0].block_bitmap - first) as usize);
            set_bit_lsb0(&mut bitmap_block, (descs[0].inode_bitmap - first) as usize);
            let itb = sb.inode_table_blocks_per_group();
            for i in 0..itb {
                set_bit_lsb0(
                    &mut bitmap_block,
                    (descs[0].inode_table + i - first) as usize,
                );
            }
            disk.segment()
                .write_bytes(
                    Bid::new(descs[0].block_bitmap as u64).to_offset(),
                    &bitmap_block,
                )
                .unwrap();
        }

        if self.filled_block_bitmap {
            let first = sb.group_first_block_no(0);
            let last = sb.group_last_block_no(0);
            let group_size = (last - first + 1) as usize;
            let mut bitmap_block = [0u8; BLOCK_SIZE];
            for bit in 0..group_size {
                set_bit_lsb0(&mut bitmap_block, bit);
            }
            disk.segment()
                .write_bytes(
                    Bid::new(descs[0].block_bitmap as u64).to_offset(),
                    &bitmap_block,
                )
                .unwrap();
        }

        if self.init_reserved_inode_bitmap {
            let mut bitmap = [0u8; BLOCK_SIZE];
            for bit in 0..(sb.first_ino() as usize).saturating_sub(1) {
                set_bit_lsb0(&mut bitmap, bit);
            }
            disk.segment()
                .write_bytes(Bid::new(descs[0].inode_bitmap as u64).to_offset(), &bitmap)
                .unwrap();
        }

        if self.filled_inode_bitmap {
            let mut bitmap = [0u8; BLOCK_SIZE];
            for bit in 0..(sb.inodes_per_group() as usize) {
                set_bit_lsb0(&mut bitmap, bit);
            }
            disk.segment()
                .write_bytes(Bid::new(descs[0].inode_bitmap as u64).to_offset(), &bitmap)
                .unwrap();
        }
    }

    /// Builds a fixture that goes through `Ext2::open`.
    pub(super) fn build(self) -> Result<Ext2Fixture> {
        let (_, sb, descs, disk, layout) = self.prepare()?;
        self.write_bitmaps(&sb, &descs, &disk, &layout);

        // Ext2::open does not read ROOT_INO during mount, but fixtures that
        // request root initialization must still place a valid root inode on
        // disk so later root lookups succeed.
        let root_bid = layout.first_data.saturating_add(1);
        if self.init_root {
            let root_raw = make_root_raw_inode(root_bid);
            write_raw_inode_to_disk(&sb, &descs, ROOT_INO, &root_raw, &disk);
        }

        let device: Arc<dyn BlockDevice> = self
            .custom_device
            .unwrap_or_else(|| disk.clone() as Arc<dyn BlockDevice>);
        let ext2 = Ext2::open(device, None)?;

        Ok(Ext2Fixture {
            disk,
            ext2,
            sb,
            descs,
        })
    }
}

// ---------------------------------------------------------------------------
// Assertion helpers
// ---------------------------------------------------------------------------

/// Asserts that an expression returns `Err` with the expected `Errno`.
macro_rules! assert_errno {
    ($expr:expr, $errno:expr) => {
        assert_eq!($expr.unwrap_err().error(), $errno)
    };
    ($expr:expr, $errno:expr, $($msg:tt)+) => {
        assert_eq!($expr.unwrap_err().error(), $errno, $($msg)+)
    };
}
pub(super) use assert_errno;

// ---------------------------------------------------------------------------
// Convenience test helpers
// ---------------------------------------------------------------------------

/// Creates a regular file (mode 0o644) inside `dir`.
pub(super) fn create_file(dir: &Arc<Inode>, name: &str) -> Arc<Inode> {
    dir.create(name, InodeType::File, FilePerm::from_bits_truncate(0o644))
        .unwrap()
}

/// Creates a directory (mode 0o755) inside `dir`.
pub(super) fn create_dir(dir: &Arc<Inode>, name: &str) -> Arc<Inode> {
    dir.create(name, InodeType::Dir, FilePerm::from_bits_truncate(0o755))
        .unwrap()
}

/// Creates a symlink (mode 0o777) inside `dir`.
pub(super) fn create_symlink(dir: &Arc<Inode>, name: &str) -> Arc<Inode> {
    dir.create(
        name,
        InodeType::SymLink,
        FilePerm::from_bits_truncate(0o777),
    )
    .unwrap()
}

/// Builds a `namei_env` fixture and returns `(fixture, root_inode)` with clocks initialized.
pub(super) fn namei_fixture() -> (Ext2Fixture, Arc<Inode>) {
    clocks::init_for_ktest();
    let f = Ext2FixtureBuilder::namei_env().build().unwrap();
    let root = f.root();
    (f, root)
}

/// Writes `data` to `inode` at `offset` with the given status flags.
pub(super) fn write_file_at(
    inode: &Arc<Inode>,
    offset: usize,
    data: &[u8],
    flags: StatusFlags,
) -> Result<usize> {
    let mut reader = VmReader::from(data).to_fallible();
    InodeIo::write_at(inode.as_ref(), offset, &mut reader, flags)
}

/// Reads `len` bytes from `inode` at `offset` with the given status flags.
pub(super) fn read_file_at(
    inode: &Arc<Inode>,
    offset: usize,
    len: usize,
    flags: StatusFlags,
) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut writer = VmWriter::from(buf.as_mut_slice()).to_fallible();
    let n = InodeIo::read_at(inode.as_ref(), offset, &mut writer, flags)?;
    buf.truncate(n);
    Ok(buf)
}

/// Returns the `ino` field of the inode found by `dir.lookup(name)`.
pub(super) fn lookup_ino(dir: &Arc<Inode>, name: &str) -> Result<u32> {
    Ok(dir.lookup(name)?.ino())
}

/// Returns the VFS-visible file size of `inode`.
pub(super) fn inode_size(inode: &Arc<Inode>) -> usize {
    crate::fs::vfs::inode::Inode::size(inode.as_ref())
}

/// Returns the VFS-visible hard-link count of `inode`.
pub(super) fn inode_nlinks(inode: &Arc<Inode>) -> usize {
    crate::fs::vfs::inode::Inode::metadata(inode.as_ref()).nr_hard_links
}
