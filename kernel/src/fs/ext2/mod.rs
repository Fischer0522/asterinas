// SPDX-License-Identifier: MPL-2.0

//! A safe Rust Ext2 filesystem.
//!
//! The Second Extended File System(Ext2) is a major rewrite of the Ext filesystem.
//! It is the predominant filesystem in use by Linux from the early 1990s to the early 2000s.
//! The structures of Ext3 and Ext4 are based on Ext2 and add some additional options
//! such as journaling.
//!
//! The features of this version of Ext2 are as follows:
//! 1. No unsafe Rust. The filesystem is written is Rust without any unsafe code,
//!    ensuring that there are no memory safety issues in the code.
//! 2. Deep integration with PageCache. The data and metadata of the filesystem are
//!    stored in PageCache, which accelerates the performance of data access.
//! 3. Compatible with queue-based block device. The filesystem can submits multiple
//!    BIO requests to be block device at once, thereby enhancing I/O performance.
//!
//! # Example
//!
//! ```no_run
//! // Opens an Ext2 from the block device.
//! let ext2 = Ext2::open(block_device)?;
//! // Lookup the root inode.
//! let root = ext2.root_inode()?;
//! // Create a file inside root directory.
//! let file = root.create("file", InodeType::File, FilePerm::from_bits_truncate(0o666))?;
//! // Write data into the file.
//! const WRITE_DATA: &[u8] = b"Hello, World";
//! let len = file.write_at(0, WRITE_DATA)?;
//! assert!(len == WRITE_DATA.len());
//! ```
//!
//! # Limitation
//!
//! Here we summarizes the features that need to be implemented in the future.
//! 1. Supports merging small read/write operations.
//! 2. Handles the intermediate failure status correctly.

pub use fs::Ext2;
pub use inode::{FilePerm, Inode};
pub use super_block::{MAGIC_NUM, SuperBlock};

use self::fs_type::Ext2Type;

pub(super) fn init() {
    super::registry::register(&Ext2Type).unwrap();
}
mod block_group;
mod dir;
mod fs;
mod fs_type;
mod inode;
mod prelude;
mod super_block;
mod utils;

#[cfg(ktest)]
pub(super) mod test {
    use alloc::sync::Arc;
    use core::{fmt, mem::size_of};

    use aster_block::{
        BLOCK_SIZE, BlockDevice, BlockDeviceMeta, SECTOR_SIZE,
        bio::{BioEnqueueError, BioStatus, BioType, SubmittedBio},
    };
    use device_id::{DeviceId, MajorId, MinorId};
    use ostd::{
        mm::{FrameAllocOptions, PAGE_SIZE, Segment, USegment, VmIo, io_util::HasVmReaderWriter},
        prelude::*,
    };

    use super::{
        block_group::RawGroupDesc,
        super_block::{
            ErrorsBehaviour, FsState, MAGIC_NUM, OsId, RawSuperBlock, RevLevel, SUPER_BLOCK_OFFSET,
            SuperBlock,
        },
    };

    pub(super) struct Ext2MemoryDisk {
        segment: Segment<()>,
    }

    impl Ext2MemoryDisk {
        pub(super) fn new(nblocks: usize) -> Self {
            let npages = (nblocks * BLOCK_SIZE).div_ceil(PAGE_SIZE);
            let segment = FrameAllocOptions::new()
                .zeroed(true)
                .alloc_segment(npages)
                .unwrap();
            Self { segment }
        }

        pub(super) fn segment(&self) -> &Segment<()> {
            &self.segment
        }

        pub(super) fn write_super_block(&self, raw: &RawSuperBlock) {
            self.segment.write_val(SUPER_BLOCK_OFFSET, raw).unwrap();
        }

        pub(super) fn write_group_desc_table(&self, sb: &SuperBlock, descs: &[RawGroupDesc]) {
            let table_offset = sb.group_descriptors_bid(0).to_offset();
            for (idx, desc) in descs.iter().enumerate() {
                let offset = table_offset + idx * size_of::<RawGroupDesc>();
                self.segment.write_val(offset, desc).unwrap();
            }
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
            let mut cur_device_ofs = bio.sid_range().start.to_raw() as usize * SECTOR_SIZE;

            for seg in bio.segments() {
                let io_size = match bio.type_() {
                    BioType::Read => seg
                        .writer()
                        .unwrap()
                        .write(self.segment.reader().skip(cur_device_ofs)),
                    BioType::Write => self
                        .segment
                        .writer()
                        .skip(cur_device_ofs)
                        .write(&mut seg.reader().unwrap()),
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
                                seg.writer().unwrap().write(reader)
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
                            writer.write(&mut seg.reader().unwrap())
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

    pub(super) fn make_valid_raw_super_block(groups_count: u32) -> RawSuperBlock {
        let mut raw = RawSuperBlock::default();
        raw.magic = MAGIC_NUM;
        raw.log_block_size = 2;
        raw.log_frag_size = 2;
        raw.state = FsState::VALID.bits();
        raw.errors = ErrorsBehaviour::Continue as u16;
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
}
