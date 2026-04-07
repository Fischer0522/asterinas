// SPDX-License-Identifier: MPL-2.0

use core::mem::size_of;

use ostd::const_assert;

use super::{block_group::RawGroupDesc, block_ptr_tree::Ext2Bid, prelude::*};

/// The magic number of Ext2.
pub const MAGIC_NUM: u16 = 0xef53;

/// The main superblock is located at byte 1024 from the beginning of the device.
pub const SUPER_BLOCK_OFFSET: usize = 1024;

const SUPER_BLOCK_SIZE: usize = 1024;

/// The in-memory Rust superblock.
///
/// It contains the layout information for the Ext2 filesystem.
#[derive(Clone, Copy, Debug)]
pub(super) struct SuperBlock {
    /// Total number of inodes.
    inodes_count: u32,
    /// Total number of blocks.
    blocks_count: u32,
    /// Total number of reserved blocks.
    reserved_blocks_count: u32,
    /// Total number of free blocks.
    free_blocks_count: u32,
    /// Total number of free inodes.
    free_inodes_count: u32,
    /// First data block.
    first_data_block: Ext2Bid,
    /// Block size.
    block_size: usize,
    /// Fragment size.
    frag_size: usize,
    /// Number of blocks in each block group.
    blocks_per_group: u32,
    /// Number of fragments in each block group.
    frags_per_group: u32,
    /// Number of inodes in each block group.
    inodes_per_group: u32,
    /// Number of inode table blocks in each group.
    inode_table_blocks_per_group: u32,
    /// Mount time.
    mtime: Duration,
    /// Write time.
    wtime: Duration,
    /// Mount count.
    mnt_count: u16,
    /// Maximal mount count.
    max_mnt_count: u16,
    /// Magic signature.
    magic: u16,
    /// Filesystem state.
    state: FsState,
    /// Behavior when detecting errors.
    errors_behaviour: ErrorsBehavior,
    /// Time of last check.
    last_check_time: Duration,
    /// Interval between checks.
    check_interval: Duration,
    /// Creator OS ID.
    creator_os: OsId,
    /// Revision level.
    rev_level: RevLevel,
    /// Default UID for reserved blocks.
    default_reserved_uid: u32,
    /// Default GID for reserved blocks.
    default_reserved_gid: u32,
    //
    // These fields are valid for RevLevel::Dynamic only.
    //
    /// First non-reserved inode number.
    first_ino: u32,
    /// Size of inode structure.
    inode_size: usize,
    /// Block group that this superblock is part of (if backup copy).
    block_group_idx: usize,
    /// Compatible feature set.
    feature_compat: FeatureCompatSet,
    /// Incompatible feature set.
    feature_incompat: FeatureInCompatSet,
    /// Read-only-compatible feature set.
    feature_ro_compat: FeatureRoCompatSet,
    /// 128-bit UUID for the volume.
    uuid: [u8; 16],
    /// Volume name.
    volume_name: Str16,
    /// Directory where last mounted.
    last_mounted_dir: Str64,
    ///
    /// These fields are valid if the FeatureCompatSet::DIR_PREALLOC is set.
    ///
    /// Number of blocks to preallocate for files.
    prealloc_file_blocks: u8,
    /// Number of blocks to preallocate for directories.
    prealloc_dir_blocks: u8,
    ///
    /// These fields are reserved and currently serve no purpose.
    ///
    min_rev_level: u16,
    algorithm_usage_bitmap: u32,
    padding1: u16,
    journal_uuid: [u8; 16],
    journal_ino: u32,
    journal_dev: u32,
    last_orphan: u32,
    hash_seed: [u32; 4],
    def_hash_version: u8,
    reserved_char_pad: u8,
    reserved_word_pad: u16,
    default_mount_opts: u32,
    first_meta_bg: u32,
    reserved: Reserved,
}

impl TryFrom<RawSuperBlock> for SuperBlock {
    type Error = crate::error::Error;

    fn try_from(sb: RawSuperBlock) -> Result<Self> {
        if sb.magic != MAGIC_NUM {
            return_errno_with_message!(Errno::EINVAL, "bad ext2 magic number");
        }

        if sb.log_block_size != 2 {
            return_errno_with_message!(Errno::EINVAL, "unsupported block size");
        }
        if sb.log_frag_size != sb.log_block_size {
            return_errno_with_message!(Errno::EINVAL, "invalid fragment size");
        }

        let block_size = BLOCK_SIZE;
        let frag_size = BLOCK_SIZE;

        let state = FsState::from_bits(sb.state)
            .ok_or(Error::with_message(Errno::EINVAL, "invalid fs state"))?;

        let errors_behaviour = ErrorsBehavior::try_from(sb.errors)
            .map_err(|_| Error::with_message(Errno::EINVAL, "invalid errors behaviour"))?;
        if errors_behaviour != ErrorsBehavior::Continue {
            return_errno_with_message!(Errno::EINVAL, "unsupported errors behaviour");
        }

        let creator_os = OsId::try_from(sb.creator_os)
            .map_err(|_| Error::with_message(Errno::EINVAL, "invalid creator os"))?;
        if creator_os != OsId::Linux {
            return_errno_with_message!(Errno::EINVAL, "not supported os id");
        }

        let rev_level = RevLevel::try_from(sb.rev_level)
            .map_err(|_| Error::with_message(Errno::EINVAL, "invalid revision level"))?;
        let (first_ino, inode_size) = match rev_level {
            RevLevel::GoodOld => (11, 128usize),
            RevLevel::Dynamic => {
                let inode_size = sb.inode_size as usize;
                if inode_size < 128 {
                    return_errno_with_message!(Errno::EINVAL, "inode size is too small");
                }
                if inode_size > BLOCK_SIZE {
                    return_errno_with_message!(Errno::EINVAL, "inode size is too large");
                }
                if !inode_size.is_power_of_two() {
                    return_errno_with_message!(Errno::EINVAL, "inode size is not power of two");
                }
                (sb.first_ino, inode_size)
            }
        };

        let inodes_per_group = sb.inodes_per_group;
        let blocks_per_group = sb.blocks_per_group;
        if inodes_per_group == 0 || blocks_per_group == 0 {
            return_errno_with_message!(Errno::EINVAL, "invalid group sizes");
        }

        let inodes_per_block = (block_size / inode_size) as u32;
        if inodes_per_block == 0 {
            return_errno_with_message!(Errno::EINVAL, "invalid inode size");
        }

        if inodes_per_group < inodes_per_block {
            return_errno_with_message!(Errno::EINVAL, "inodes per group is too small");
        }

        let max_per_group = (block_size as u32) * 8;
        if inodes_per_group > max_per_group {
            return_errno_with_message!(Errno::EINVAL, "inodes per group is too large");
        }
        if blocks_per_group > max_per_group {
            return_errno_with_message!(Errno::EINVAL, "blocks per group is too large");
        }

        let inode_table_blocks_per_group = inodes_per_group / inodes_per_block;
        if blocks_per_group <= inode_table_blocks_per_group + 3 {
            return_errno_with_message!(Errno::EINVAL, "blocks per group is too small");
        }

        let blocks_count = sb.blocks_count as u64;
        let first_data_block = sb.first_data_block as u64;
        if blocks_count <= first_data_block + 1 {
            return_errno_with_message!(Errno::EINVAL, "invalid blocks count");
        }
        let blocks_after = blocks_count - first_data_block - 1;
        let groups_count = (blocks_after / blocks_per_group as u64) + 1;

        // The last block group may legitimately contain fewer inodes than the
        // full per-group capacity.
        let max_inodes = groups_count * (inodes_per_group as u64);
        let min_inodes = (groups_count - 1) * (inodes_per_group as u64);
        let inodes_count = sb.inodes_count as u64;
        if inodes_count <= min_inodes || inodes_count > max_inodes {
            return_errno_with_message!(Errno::EINVAL, "invalid inodes count");
        }

        let feature_compat = FeatureCompatSet::from_bits_truncate(sb.feature_compat);

        let allowed_incompat = FeatureInCompatSet::FILETYPE.bits();
        if (sb.feature_incompat & !allowed_incompat) != 0 {
            return_errno_with_message!(Errno::EINVAL, "unsupported incompat feature");
        }
        let feature_incompat = FeatureInCompatSet::from_bits_truncate(sb.feature_incompat);

        let feature_ro_compat = FeatureRoCompatSet::from_bits_truncate(sb.feature_ro_compat);

        Ok(Self {
            inodes_count: sb.inodes_count,
            blocks_count: sb.blocks_count,
            reserved_blocks_count: sb.reserved_blocks_count,
            free_blocks_count: sb.free_blocks_count,
            free_inodes_count: sb.free_inodes_count,
            first_data_block: sb.first_data_block,
            block_size,
            frag_size,
            blocks_per_group: sb.blocks_per_group,
            frags_per_group: sb.frags_per_group,
            inodes_per_group: sb.inodes_per_group,
            inode_table_blocks_per_group: inode_table_blocks_per_group,
            mtime: Duration::from(sb.mtime),
            wtime: Duration::from(sb.wtime),
            mnt_count: sb.mnt_count,
            max_mnt_count: sb.max_mnt_count,
            magic: MAGIC_NUM,
            state,
            errors_behaviour,
            last_check_time: Duration::from(sb.last_check_time),
            check_interval: Duration::from_secs(sb.check_interval as _),
            creator_os,
            rev_level,
            default_reserved_uid: sb.default_reserved_uid as _,
            default_reserved_gid: sb.default_reserved_gid as _,
            first_ino,
            inode_size,
            block_group_idx: sb.block_group_idx as _,
            feature_compat,
            feature_incompat,
            feature_ro_compat,
            uuid: sb.uuid,
            volume_name: sb.volume_name,
            last_mounted_dir: sb.last_mounted_dir,
            prealloc_file_blocks: sb.prealloc_file_blocks,
            prealloc_dir_blocks: sb.prealloc_dir_blocks,
            min_rev_level: sb.min_rev_level,
            algorithm_usage_bitmap: sb.algorithm_usage_bitmap,
            padding1: sb.padding1,
            journal_uuid: sb.journal_uuid,
            journal_ino: sb.journal_ino,
            journal_dev: sb.journal_dev,
            last_orphan: sb.last_orphan,
            hash_seed: sb.hash_seed,
            def_hash_version: sb.def_hash_version,
            reserved_char_pad: sb.reserved_char_pad,
            reserved_word_pad: sb.reserved_word_pad,
            default_mount_opts: sb.default_mount_opts,
            first_meta_bg: sb.first_meta_bg,
            reserved: sb.reserved,
        })
    }
}

impl SuperBlock {
    /// Returns the block size.
    pub(super) fn block_size(&self) -> usize {
        self.block_size
    }

    /// Returns the maximum regular file size supported by this ext2 instance.
    pub(super) fn max_file_size(&self) -> usize {
        const DIRECT_BLOCKS: u64 = 12;

        let block_size_bits = self.block_size.trailing_zeros();
        let ptrs_per_block = 1u64 << (block_size_bits - 2);
        let mut max_blocks = DIRECT_BLOCKS;
        let mut metadata_blocks = 1u64;
        let mut upper_limit = (1u64 << 32) - 1;

        // `i_blocks` stores the total 512-byte sector count for data and
        // indirect metadata. Linux keeps ext2 one filesystem block below the
        // 32-bit sector accounting limit.
        upper_limit >>= block_size_bits - 9;

        max_blocks += 1u64 << (block_size_bits - 2);
        max_blocks += 1u64 << (2 * (block_size_bits - 2));
        max_blocks += 1u64 << (3 * (block_size_bits - 2));

        metadata_blocks += 1 + ptrs_per_block;
        metadata_blocks += 1 + ptrs_per_block + ptrs_per_block * ptrs_per_block;

        if max_blocks + metadata_blocks > upper_limit {
            max_blocks = upper_limit;

            upper_limit -= DIRECT_BLOCKS;

            metadata_blocks = 1;
            upper_limit -= ptrs_per_block;

            if upper_limit < ptrs_per_block * ptrs_per_block {
                metadata_blocks += 1 + upper_limit.div_ceil(ptrs_per_block);
                max_blocks -= metadata_blocks;
            } else {
                metadata_blocks += 1 + ptrs_per_block;
                upper_limit -= ptrs_per_block * ptrs_per_block;
                metadata_blocks += 1
                    + upper_limit.div_ceil(ptrs_per_block)
                    + upper_limit.div_ceil(ptrs_per_block * ptrs_per_block);
                max_blocks -= metadata_blocks;
            }
        }

        let max_bytes = (max_blocks << block_size_bits).min(i64::MAX as u64);
        max_bytes as usize
    }

    /// Returns the size of inode structure.
    pub(super) fn inode_size(&self) -> usize {
        self.inode_size
    }

    /// Returns the fragment size.
    pub(super) fn fragment_size(&self) -> usize {
        self.frag_size
    }

    /// Returns total number of inodes.
    pub(super) fn total_inodes(&self) -> u32 {
        self.inodes_count
    }

    /// Returns total number of blocks.
    pub(super) fn total_blocks(&self) -> u32 {
        self.blocks_count
    }

    /// Returns the number of blocks in each block group.
    pub(super) fn blocks_per_group(&self) -> u32 {
        self.blocks_per_group
    }

    /// Returns the first block number of a block group.
    ///
    pub(super) fn group_first_block_no(&self, group_idx: usize) -> u32 {
        (group_idx as u32) * self.blocks_per_group + self.first_data_block()
    }

    /// Returns the last block number of a block group.
    ///
    pub(super) fn group_last_block_no(&self, group_idx: usize) -> u32 {
        let groups_count = self.block_groups_count();
        if group_idx as u32 == groups_count - 1 {
            self.total_blocks() - 1
        } else {
            self.group_first_block_no(group_idx) + self.blocks_per_group - 1
        }
    }

    /// Returns whether a data block range is valid.
    ///
    pub(super) fn is_data_block_valid(&self, start_blk: u32, count: u32) -> bool {
        if count == 0 {
            return false;
        }

        let first_data_block = self.first_data_block();
        let blocks_count = self.total_blocks();

        let Some(end_blk) = start_blk.checked_add(count - 1) else {
            return false;
        };

        if start_blk <= first_data_block || end_blk < start_blk || end_blk >= blocks_count {
            return false;
        }

        let sb_block = if self.block_size == SUPER_BLOCK_SIZE {
            1u32
        } else {
            0u32
        };
        if start_blk <= sb_block && end_blk >= sb_block {
            return false;
        }

        true
    }

    /// Returns the first data block number.
    pub(super) fn first_data_block(&self) -> Ext2Bid {
        self.first_data_block
    }

    /// Returns the number of inodes in each block group.
    pub(super) fn inodes_per_group(&self) -> u32 {
        self.inodes_per_group
    }

    /// Returns the number of inode table blocks in each block group.
    pub(super) fn inode_table_blocks_per_group(&self) -> u32 {
        self.inode_table_blocks_per_group
    }

    /// Returns the first non-reserved inode number.
    pub(super) fn first_ino(&self) -> u32 {
        self.first_ino
    }

    /// Returns the number of block groups.
    pub(super) fn block_groups_count(&self) -> u32 {
        self.blocks_count.div_ceil(self.blocks_per_group)
    }

    /// Returns the number of group descriptor blocks in each superblock copy.
    ///
    pub(super) fn group_descriptor_blocks_count(&self) -> u32 {
        let descriptor_bytes = (self.block_groups_count() as usize) * size_of::<RawGroupDesc>();
        descriptor_bytes.div_ceil(self.block_size) as u32
    }

    /// Returns the number of free blocks.
    pub(super) fn free_blocks_count(&self) -> u32 {
        self.free_blocks_count
    }

    /// Returns the number of reserved blocks.
    pub(super) fn reserved_blocks_count(&self) -> u32 {
        self.reserved_blocks_count
    }

    /// Returns the default UID for reserved blocks.
    ///
    pub(super) fn default_reserved_uid(&self) -> u32 {
        self.default_reserved_uid
    }

    /// Returns the default GID for reserved blocks.
    ///
    pub(super) fn default_reserved_gid(&self) -> u32 {
        self.default_reserved_gid
    }

    /// Increases the number of free blocks.
    pub(super) fn inc_free_blocks(&mut self, count: u32) {
        self.free_blocks_count += count;
    }

    /// Overwrites the free blocks counter with a recomputed value.
    pub(super) fn set_free_blocks_count(&mut self, count: u32) {
        self.free_blocks_count = count;
    }

    /// Decreases the number of free blocks.
    pub(super) fn dec_free_blocks(&mut self, count: u32) {
        if self.free_blocks_count < count {
            warn!(
                "free block counter underflow detected: free_blocks_count={}, count={}",
                self.free_blocks_count, count
            );
        }
        self.free_blocks_count = self.free_blocks_count.saturating_sub(count);
    }

    /// Returns the number of free inodes.
    pub(super) fn free_inodes_count(&self) -> u32 {
        self.free_inodes_count
    }

    /// Overwrites the free inodes counter with a recomputed value.
    pub(super) fn set_free_inodes_count(&mut self, count: u32) {
        self.free_inodes_count = count;
    }

    /// Increases the number of free inodes.
    pub(super) fn inc_free_inodes(&mut self) {
        self.free_inodes_count += 1;
    }

    pub(super) fn set_wtime(&mut self, time: Duration) {
        self.wtime = time;
    }

    /// Decreases the number of free inodes.
    pub(super) fn dec_free_inodes(&mut self) {
        debug_assert!(self.free_inodes_count > 0);
        self.free_inodes_count = self.free_inodes_count.saturating_sub(1);
    }

    /// Checks if the block group will backup the super block.
    pub(super) fn is_backup_group(&self, block_group_idx: usize) -> bool {
        if block_group_idx == 0 {
            false
        } else if self
            .feature_ro_compat
            .contains(FeatureRoCompatSet::SPARSE_SUPER)
        {
            // The backup groups chosen are 1 and powers of 3, 5 and 7.
            block_group_idx == 1
                || block_group_idx.is_power_of(3)
                || block_group_idx.is_power_of(5)
                || block_group_idx.is_power_of(7)
        } else {
            true
        }
    }

    /// Returns whether the given group stores a superblock copy.
    ///
    pub(super) fn has_super_block(&self, block_group_idx: usize) -> bool {
        block_group_idx == 0 || self.is_backup_group(block_group_idx)
    }

    /// Computes the metadata overhead subtracted from `statfs` totals.
    pub(super) fn statfs_overhead_blocks(&self) -> u32 {
        let groups_count = self.block_groups_count() as usize;
        let gdb_count = self.group_descriptor_blocks_count();
        let mut overhead = self.first_data_block();

        for group_idx in 0..groups_count {
            if self.has_super_block(group_idx) {
                overhead = overhead.saturating_add(1 + gdb_count);
            }
        }

        overhead.saturating_add(self.block_groups_count() * (2 + self.inode_table_blocks_per_group))
    }

    /// Returns the starting block ID of the superblock copy
    /// inside the block group identified by `block_group_idx`.
    ///
    /// # Panics
    ///
    /// If `block_group_idx` is neither 0 nor a backup block group index,
    /// then the method panics.
    pub(super) fn bid(&self, block_group_idx: usize) -> Ext2Bid {
        if block_group_idx == 0 {
            let bid = (SUPER_BLOCK_OFFSET / self.block_size) as u32;
            return bid;
        }

        assert!(self.is_backup_group(block_group_idx));
        let super_block_bid = block_group_idx * (self.blocks_per_group as usize);
        super_block_bid as u32
    }

    /// Returns the starting block ID of the block-group descriptor table
    /// inside the block group identified by `block_group_idx`.
    ///
    /// # Panics
    ///
    /// If `block_group_idx` is neither 0 nor a backup block group index,
    /// then the method panics.
    pub(super) fn group_descriptors_bid(&self, block_group_idx: usize) -> Ext2Bid {
        let super_block_bid = self.bid(block_group_idx);
        super_block_bid + (SUPER_BLOCK_SIZE.div_ceil(self.block_size) as u32)
    }

    #[expect(dead_code)]
    fn state(&self) -> FsState {
        self.state
    }

    #[expect(dead_code)]
    fn rev_level(&self) -> RevLevel {
        self.rev_level
    }

    #[expect(dead_code)]
    fn feature_compat(&self) -> FeatureCompatSet {
        self.feature_compat
    }

    #[expect(dead_code)]
    fn feature_incompat(&self) -> FeatureInCompatSet {
        self.feature_incompat
    }

    #[expect(dead_code)]
    fn feature_ro_compat(&self) -> FeatureRoCompatSet {
        self.feature_ro_compat
    }
}

bitflags! {
    /// Compatible feature set.
    struct FeatureCompatSet: u32 {
        /// Preallocate some number of blocks to a directory when creating a new one
        const DIR_PREALLOC = 1 << 0;
        /// AFS server inodes exist
        const IMAGIC_INODES = 1 << 1;
        /// Filesystem has a journal
        const HAS_JOURNAL = 1 << 2;
        /// Inodes have extended attributes
        const EXT_ATTR = 1 << 3;
        /// Filesystem can resize itself for larger partitions
        const RESIZE_INO = 1 << 4;
        /// Directories use hash index
        const DIR_INDEX = 1 << 5;
    }
}

bitflags! {
    /// Incompatible feature set.
    struct FeatureInCompatSet: u32 {
        /// Compression is used
        const COMPRESSION = 1 << 0;
        /// Directory entries contain a type field
        const FILETYPE = 1 << 1;
        /// Filesystem needs to replay its journal
        const RECOVER = 1 << 2;
        /// Filesystem uses a journal device
        const JOURNAL_DEV = 1 << 3;
        /// Metablock block group
        const META_BG = 1 << 4;
    }
}

bitflags! {
    /// Read-only-compatible feature set.
    struct FeatureRoCompatSet: u32 {
        /// Sparse superblocks and group descriptor tables
        const SPARSE_SUPER = 1 << 0;
        /// Filesystem uses a 64-bit file size
        const LARGE_FILE = 1 << 1;
        /// Directory contents are stored in a binary tree.
        const BTREE_DIR = 1 << 2;
    }
}

bitflags! {
    /// Filesystem state.
    ///
    /// Reference: <https://www.nongnu.org/ext2-doc/ext2.html#s-state>
    pub(super) struct FsState: u16 {
        /// Unmounted cleanly
        const VALID = 1 << 0;
        /// Errors detected
        const ERROR = 1 << 1;
    }
}

/// Action the filesystem driver takes when an error is detected.
#[repr(u16)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, TryFromInt)]
pub(super) enum ErrorsBehavior {
    /// Continues execution.
    #[default]
    Continue = 1,
    /// Remounts the filesystem read-only.
    RemountReadonly = 2,
    /// Panics.
    Panic = 3,
}

/// OS that created the filesystem (`s_creator_os`).
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, TryFromInt)]
pub(super) enum OsId {
    Linux = 0,
    Hurd = 1,
    Masix = 2,
    FreeBSD = 3,
    Lites = 4,
}

/// Ext2 revision level (`s_rev_level`).
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, TryFromInt)]
pub(super) enum RevLevel {
    /// The good old (original) format.
    GoodOld = 0,
    /// V2 format with dynamic inode size.
    Dynamic = 1,
}

const_assert!(size_of::<RawSuperBlock>() == SUPER_BLOCK_SIZE);

/// The on-disk superblock structure.
///
/// This structure represents the raw layout of the Ext2 superblock as it appears
/// on disk. It must be exactly 1024 bytes in length to match the Ext2 specification.
/// The in-memory representation is provided by [`SuperBlock`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(super) struct RawSuperBlock {
    pub inodes_count: u32,
    pub blocks_count: u32,
    pub reserved_blocks_count: u32,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
    pub first_data_block: u32,
    /// The number to left-shift 1024 to obtain the block size.
    pub log_block_size: u32,
    /// The number to left-shift 1024 to obtain the fragment size.
    pub log_frag_size: u32,
    pub blocks_per_group: u32,
    pub frags_per_group: u32,
    pub inodes_per_group: u32,
    /// Mount time.
    pub mtime: UnixTime,
    /// Write time.
    pub wtime: UnixTime,
    pub mnt_count: u16,
    pub max_mnt_count: u16,
    pub magic: u16,
    pub state: u16,
    pub errors: u16,
    pub min_rev_level: u16,
    /// Time of last check.
    pub last_check_time: UnixTime,
    pub check_interval: u32,
    pub creator_os: u32,
    pub rev_level: u32,
    pub default_reserved_uid: u16,
    pub default_reserved_gid: u16,
    pub first_ino: u32,
    pub inode_size: u16,
    pub block_group_idx: u16,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub uuid: [u8; 16],
    pub volume_name: Str16,
    pub last_mounted_dir: Str64,
    pub algorithm_usage_bitmap: u32,
    pub prealloc_file_blocks: u8,
    pub prealloc_dir_blocks: u8,
    padding1: u16,
    ///
    /// These fields are for journaling support in Ext3.
    ///
    /// UUID of the journal superblock.
    pub journal_uuid: [u8; 16],
    /// Inode number of journal file.
    pub journal_ino: u32,
    /// Device number of journal file.
    pub journal_dev: u32,
    /// Start of list of inodes to delete.
    pub last_orphan: u32,
    /// HTREE hash seed.
    pub hash_seed: [u32; 4],
    /// Default hash version to use.
    pub def_hash_version: u8,
    reserved_char_pad: u8,
    reserved_word_pad: u16,
    /// Default mount options.
    pub default_mount_opts: u32,
    /// First metablock block group.
    pub first_meta_bg: u32,
    reserved: Reserved,
}

impl From<&SuperBlock> for RawSuperBlock {
    fn from(sb: &SuperBlock) -> Self {
        Self {
            inodes_count: sb.inodes_count,
            blocks_count: sb.blocks_count,
            reserved_blocks_count: sb.reserved_blocks_count,
            free_blocks_count: sb.free_blocks_count,
            free_inodes_count: sb.free_inodes_count,
            first_data_block: sb.first_data_block,
            log_block_size: (sb.block_size / SUPER_BLOCK_SIZE).trailing_zeros(),
            log_frag_size: (sb.frag_size / SUPER_BLOCK_SIZE).trailing_zeros(),
            blocks_per_group: sb.blocks_per_group,
            frags_per_group: sb.frags_per_group,
            inodes_per_group: sb.inodes_per_group,
            mtime: UnixTime::from(sb.mtime),
            wtime: UnixTime::from(sb.wtime),
            mnt_count: sb.mnt_count,
            max_mnt_count: sb.max_mnt_count,
            magic: sb.magic,
            state: sb.state.bits(),
            errors: sb.errors_behaviour as u16,
            min_rev_level: sb.min_rev_level,
            last_check_time: UnixTime::from(sb.last_check_time),
            check_interval: sb.check_interval.as_secs() as u32,
            creator_os: sb.creator_os as u32,
            rev_level: sb.rev_level as u32,
            default_reserved_uid: sb.default_reserved_uid as u16,
            default_reserved_gid: sb.default_reserved_gid as u16,
            first_ino: sb.first_ino,
            inode_size: sb.inode_size as u16,
            block_group_idx: sb.block_group_idx as u16,
            feature_compat: sb.feature_compat.bits(),
            feature_incompat: sb.feature_incompat.bits(),
            feature_ro_compat: sb.feature_ro_compat.bits(),
            uuid: sb.uuid,
            volume_name: sb.volume_name,
            last_mounted_dir: sb.last_mounted_dir,
            algorithm_usage_bitmap: sb.algorithm_usage_bitmap,
            prealloc_file_blocks: sb.prealloc_file_blocks,
            prealloc_dir_blocks: sb.prealloc_dir_blocks,
            padding1: sb.padding1,
            journal_uuid: sb.journal_uuid,
            journal_ino: sb.journal_ino,
            journal_dev: sb.journal_dev,
            last_orphan: sb.last_orphan,
            hash_seed: sb.hash_seed,
            def_hash_version: sb.def_hash_version,
            reserved_char_pad: sb.reserved_char_pad,
            reserved_word_pad: sb.reserved_word_pad,
            default_mount_opts: sb.default_mount_opts,
            first_meta_bg: sb.first_meta_bg,
            reserved: sb.reserved,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct Reserved([u32; 190]);

impl Default for Reserved {
    fn default() -> Self {
        Self([0u32; 190])
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::*;
    use crate::fs::fs_impls::ext2::testkit::make_valid_raw_super_block;

    #[ktest]
    fn try_from_valid_raw_ok() {
        let raw = make_valid_raw_super_block(2);
        let sb = SuperBlock::try_from(raw).unwrap();
        assert_eq!(sb.total_blocks(), raw.blocks_count);
        assert_eq!(sb.total_inodes(), raw.inodes_count);
        assert_eq!(sb.block_groups_count(), 2);
    }

    #[ktest]
    fn try_from_bad_magic_returns_einval() {
        let mut raw = make_valid_raw_super_block(1);
        raw.magic = 0;

        let err = SuperBlock::try_from(raw).unwrap_err();
        assert_eq!(err.error(), Errno::EINVAL);
    }
}
