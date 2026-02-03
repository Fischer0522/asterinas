// SPDX-License-Identifier: MPL-2.0

use core::{mem::size_of, time::Duration};

use ostd::const_assert;

use super::{
    block_group::BlockGroupDescTable,
    prelude::*,
    super_block::{SuperBlock, SUPER_BLOCK_OFFSET},
};

/// The root inode number.
pub const ROOT_INO: u32 = 2;

/// Number of block pointers in a raw inode.
pub const EXT2_N_BLOCKS: usize = 15;

/// Block pointer array in raw inode.
pub type BlockPtrs = [u32; EXT2_N_BLOCKS];

/// OS-dependent fields (Linux ext2_inode::osd2.linux2).
///
/// Linux: fs/ext2/ext2.h:319-341 (struct ext2_inode, linux2).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct Osd2 {
    pub frag: u8,
    pub fsize: u8,
    pub pad1: u16,
    pub uid_high: u16,
    pub gid_high: u16,
    pub reserved2: u32,
}

/// On-disk inode (struct ext2_inode).
///
/// Linux: fs/ext2/ext2.h:290-342.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct RawInode {
    pub mode: u16,
    pub uid_low: u16,
    pub size_low: u32,
    pub atime: UnixTime,
    pub ctime: UnixTime,
    pub mtime: UnixTime,
    pub dtime: UnixTime,
    pub gid_low: u16,
    pub links_count: u16,
    pub blocks: u32,
    pub flags: u32,
    pub osd1: u32,
    pub block_ptrs: BlockPtrs,
    pub generation: u32,
    pub file_acl: u32,
    pub dir_acl: u32,
    pub faddr: u32,
    pub osd2: Osd2,
}

const_assert!(size_of::<RawInode>() == 128);

bitflags! {
    /// File permission bits.
    ///
    /// Linux: include/uapi/linux/stat.h (S_IFMT bits in i_mode).
    pub struct FilePerm: u16 {
        const S_ISUID = 0o4000;
        const S_ISGID = 0o2000;
        const S_ISVTX = 0o1000;
        const S_IRUSR = 0o0400;
        const S_IWUSR = 0o0200;
        const S_IXUSR = 0o0100;
        const S_IRGRP = 0o0040;
        const S_IWGRP = 0o0020;
        const S_IXGRP = 0o0010;
        const S_IROTH = 0o0004;
        const S_IWOTH = 0o0002;
        const S_IXOTH = 0o0001;
    }
}

impl FilePerm {
    pub fn from_raw_mode(mode: u16) -> Result<Self> {
        const PERM_MASK: u16 = 0o7777;
        Self::from_bits(mode & PERM_MASK)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "invalid file perm"))
    }
}

/// In-memory inode descriptor (Asterinas adaptation of ext2_inode_info).
///
/// Linux: fs/ext2/ext2.h:632-680.
#[derive(Clone, Copy, Debug)]
pub struct InodeDesc {
    pub type_: InodeType,
    pub perm: FilePerm,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: Duration,
    pub ctime: Duration,
    pub mtime: Duration,
    pub dtime: Duration,
    pub links_count: u16,
    pub blocks: u32,
    pub flags: u32,
    pub block_ptrs: BlockPtrs,
    pub generation: u32,
    pub file_acl: u32,
    pub dir_acl: u32,
    pub faddr: u32,
    pub frag: u8,
    pub fsize: u8,
    pub block_group: u32,
}

impl InodeDesc {
    /// Parses a raw inode into an in-memory descriptor.
    ///
    /// Linux: fs/ext2/inode.c:1394-1510 (ext2_iget).
    pub fn from_raw(
        sb: &SuperBlock,
        raw: RawInode,
        block_group: u32,
        no_uid32: bool,
    ) -> Result<Self> {
        let type_ = InodeType::from_raw_mode(raw.mode)?;
        let perm = FilePerm::from_raw_mode(raw.mode)?;

        let mut uid = raw.uid_low as u32;
        let mut gid = raw.gid_low as u32;
        if !no_uid32 {
            uid |= (raw.osd2.uid_high as u32) << 16;
            gid |= (raw.osd2.gid_high as u32) << 16;
        }

        let dtime = Duration::from(raw.dtime);
        if raw.links_count == 0 && (raw.mode == 0 || dtime != Duration::ZERO) {
            return_errno_with_message!(Errno::ESTALE, "deleted inode");
        }

        let (size, dir_acl) = if type_ == InodeType::File {
            (
                (raw.size_low as u64) | ((raw.dir_acl as u64) << 32),
                0,
            )
        } else {
            (raw.size_low as u64, raw.dir_acl)
        };

        if size > i64::MAX as u64 {
            return_errno_with_message!(Errno::EUCLEAN, "inode size overflow");
        }

        if raw.file_acl != 0 && !data_block_valid(sb, raw.file_acl, 1) {
            return_errno_with_message!(Errno::EUCLEAN, "bad inode file acl block");
        }

        Ok(Self {
            type_,
            perm,
            uid,
            gid,
            size,
            atime: Duration::from(raw.atime),
            ctime: Duration::from(raw.ctime),
            mtime: Duration::from(raw.mtime),
            dtime: Duration::ZERO,
            links_count: raw.links_count,
            blocks: raw.blocks,
            flags: raw.flags,
            block_ptrs: raw.block_ptrs,
            generation: raw.generation,
            file_acl: raw.file_acl,
            dir_acl,
            faddr: raw.faddr,
            frag: raw.osd2.frag,
            fsize: raw.osd2.fsize,
            block_group,
        })
    }

    /// Returns whether the inode is a fast symlink.
    ///
    /// Linux: fs/ext2/inode.c:48-55 (ext2_inode_is_fast_symlink).
    pub fn is_fast_symlink(&self) -> bool {
        let ea_blocks = if self.file_acl != 0 {
            (BLOCK_SIZE / SECTOR_SIZE) as i64
        } else {
            0
        };
        self.type_ == InodeType::SymLink && (self.blocks as i64 - ea_blocks == 0)
    }
}

/// Provides PageCache-backed inode tables for block groups.
pub trait InodeTableProvider: Send + Sync {
    fn inode_table_cache(
        &self,
        group_idx: usize,
        inode_table_block: u32,
        inodes_per_group: u32,
        inode_size: usize,
    ) -> Result<Arc<PageCache>>;
}

/// Reads and parses an inode from disk.
///
/// Linux: fs/ext2/inode.c:1314-1510 (ext2_get_inode, ext2_iget).
pub fn ext2_iget(
    sb: &SuperBlock,
    bg_table: &BlockGroupDescTable,
    inode_tables: &dyn InodeTableProvider,
    ino: u32,
    no_uid32: bool,
) -> Result<InodeDesc> {
    if (ino != ROOT_INO && ino < sb.first_ino()) || ino > sb.total_inodes() {
        return_errno_with_message!(Errno::EINVAL, "bad inode number");
    }

    let inode_size = sb.inode_size();
    if inode_size == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid inode size");
    }

    let inodes_per_group = sb.inodes_per_group();
    if inodes_per_group == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid inodes per group");
    }

    let ino_index = ino
        .checked_sub(1)
        .ok_or_else(|| Error::with_message(Errno::EINVAL, "bad inode number"))?;
    let block_group = ino_index / inodes_per_group;
    let inode_idx = ino_index % inodes_per_group;

    let offset = (inode_idx as usize)
        .checked_mul(inode_size)
        .ok_or_else(|| Error::with_message(Errno::EINVAL, "inode offset overflow"))?;

    let inode_table = block_group_desc_inode_table(bg_table, block_group)?;
    let cache = inode_tables
        .inode_table_cache(
            block_group as usize,
            inode_table,
            inodes_per_group,
            inode_size,
        )
        .map_err(|_| Error::with_message(Errno::EIO, "inode table cache unavailable"))?;

    let raw = cache
        .pages()
        .read_val::<RawInode>(offset)
        .map_err(|_| Error::with_message(Errno::EIO, "inode read failed"))?;

    InodeDesc::from_raw(sb, raw, block_group, no_uid32)
}

fn block_group_desc_inode_table(
    bg_table: &BlockGroupDescTable,
    block_group: u32,
) -> Result<u32> {
    let desc = bg_table
        .group_desc(block_group as usize)
        .map_err(|_| Error::with_message(Errno::EIO, "block group descriptor missing"))?;
    Ok(desc.inode_table)
}

/// Returns whether a data block range is valid.
///
/// Linux: fs/ext2/balloc.c:1177-1196 (ext2_data_block_valid).
pub fn data_block_valid(sb: &SuperBlock, start_block: u32, count: u32) -> bool {
    if count == 0 {
        return false;
    }

    let first_data_block = sb.first_data_block();
    let total_blocks = sb.total_blocks();

    let end_block = match start_block.checked_add(count - 1) {
        Some(end) => end,
        None => return false,
    };

    if start_block <= first_data_block || end_block < start_block || end_block >= total_blocks {
        return false;
    }

    let sb_block = (SUPER_BLOCK_OFFSET / BLOCK_SIZE) as u32;
    if start_block <= sb_block && end_block >= sb_block {
        return false;
    }

    true
}

/// Minimal inode wrapper for Phase 3.
#[derive(Clone, Debug)]
pub struct Inode {
    ino: u32,
    desc: InodeDesc,
}

impl Inode {
    pub fn new(ino: u32, desc: InodeDesc) -> Self {
        Self { ino, desc }
    }

    pub fn ino(&self) -> u32 {
        self.ino
    }

    pub fn desc(&self) -> &InodeDesc {
        &self.desc
    }
}
