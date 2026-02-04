// SPDX-License-Identifier: MPL-2.0

use core::mem::size_of;

use ostd::const_assert;

use super::prelude::*;
use super::fs::Ext2;

#[derive(Clone, Copy, Debug)]
pub struct FilePerm(u16);

impl FilePerm {
    pub fn from_bits_truncate(bits: u16) -> Self {
        Self(bits)
    }
}

#[derive(Debug)]
pub struct Inode {
    ino: u32,
    type_: InodeType,
    perm: FilePerm,
    uid: u32,
    gid: u32,
    size: u64,
    atime: UnixTime,
    ctime: UnixTime,
    mtime: UnixTime,
    dtime: UnixTime,
    links_count: u16,
    blocks: u32,
    flags: u32,
    faddr: u32,
    frag: u8,
    fsize: u8,
    file_acl: u32,
    dir_acl: u32,
    generation: u32,
    block_group_idx: usize,
    dir_start_lookup: u32,
    block_ptrs: [u32; 15],
    fs: Weak<Ext2>,
}

impl Inode {
    pub fn create(&self, _name: &str, _type_: InodeType, _perm: FilePerm) -> Result<Arc<Inode>> {
        return_errno!(Errno::ENOSYS);
    }

    pub fn write_at(&self, _offset: usize, _data: &[u8]) -> Result<usize> {
        return_errno!(Errno::ENOSYS);
    }

    // TODO: Implement inode_from_desc caching.
    pub(super) fn from_desc(ino: u32, desc: InodeDesc, fs: Weak<Ext2>) -> Result<Arc<Inode>> {
        let raw = desc.raw;
        let mode = raw.mode;

        if raw.links_count == 0 && (mode == 0 || raw.dtime != 0) {
            return_errno!(Errno::ESTALE);
        }

        // TODO: Different from Linux
        let type_ = InodeType::from_raw_mode(mode)?;

        let perm = FilePerm::from_bits_truncate(mode);

        let uid = (raw.uid as u32) | ((raw.uid_high as u32) << 16);
        let gid = (raw.gid as u32) | ((raw.gid_high as u32) << 16);

        let atime = UnixTime::from(Duration::from_secs(raw.atime as u64));
        let ctime = UnixTime::from(Duration::from_secs(raw.ctime as u64));
        let mtime = UnixTime::from(Duration::from_secs(raw.mtime as u64));

        let blocks = raw.blocks;
        let flags = raw.flags;
        let faddr = raw.faddr;
        let frag = raw.frag;
        let fsize = raw.fsize;
        let file_acl = raw.file_acl;
        let generation = raw.generation;
        let mut dir_acl = 0u32;

        let mut size = raw.size_lo as u64;
        if type_ == InodeType::File {
            size |= (raw.size_high as u64) << 32;
        } else {
            dir_acl = raw.size_high;
        }
        if size > i64::MAX as u64 {
            return_errno!(Errno::EUCLEAN);
        }

        let fs_arc = fs.upgrade().ok_or_else(|| Error::new(Errno::EIO))?;
        let sb = fs_arc.super_block();
        if file_acl != 0 && !sb.data_block_valid(file_acl, 1) {
            return_errno!(Errno::EUCLEAN);
        }

        let block_group_idx = ((ino - 1) / sb.inodes_per_group()) as usize;
        let dir_start_lookup = 0;
        let block_ptrs = raw.block;

        let inode = Inode {
            ino,
            type_,
            perm,
            uid,
            gid,
            size,
            atime,
            ctime,
            mtime,
            dtime: UnixTime::from(Duration::from_secs(0)),
            links_count: raw.links_count,
            blocks,
            flags,
            faddr,
            frag,
            fsize,
            file_acl,
            dir_acl,
            generation,
            block_group_idx,
            dir_start_lookup,
            block_ptrs,
            fs,
        };

        Ok(Arc::new(inode))
    }

    /// Translates a logical block number into a path of block pointer offsets.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:163 (ext2_block_to_path)
    pub(super) fn block_to_path(&self, iblock: u32) -> Result<BlockPath> {
        let fs = self.fs.upgrade().ok_or_else(|| Error::new(Errno::EIO))?;
        let sb = fs.super_block();
        let ptrs = (sb.block_size() / size_of::<u32>()) as u32;
        if ptrs == 0 {
            return_errno!(Errno::EINVAL);
        }

        let ptrs_bits = ptrs.trailing_zeros();
        let direct_blocks = 12u32;
        let indirect_blocks = ptrs;
        let double_blocks = 1u32
            .checked_shl(ptrs_bits.saturating_mul(2))
            .ok_or_else(|| Error::new(Errno::EINVAL))?;

        let mut offsets = [0u32; 4];
        let depth;
        let boundary;
        let mut block = iblock;

        if block < direct_blocks {
            offsets[0] = block;
            depth = 1usize;
            boundary = direct_blocks - 1 - block;
        } else if {
            block = block.saturating_sub(direct_blocks);
            block < indirect_blocks
        } {
            offsets[0] = 12;
            offsets[1] = block;
            depth = 2usize;
            boundary = ptrs - 1 - (block & (ptrs - 1));
        } else if {
            block = block.saturating_sub(indirect_blocks);
            block < double_blocks
        } {
            offsets[0] = 13;
            offsets[1] = block >> ptrs_bits;
            offsets[2] = block & (ptrs - 1);
            depth = 3usize;
            boundary = ptrs - 1 - (block & (ptrs - 1));
        } else if {
            block = block.saturating_sub(double_blocks);
            (block >> (ptrs_bits.saturating_mul(2))) < ptrs
        } {
            offsets[0] = 14;
            offsets[1] = block >> (ptrs_bits * 2);
            offsets[2] = (block >> ptrs_bits) & (ptrs - 1);
            offsets[3] = block & (ptrs - 1);
            depth = 4usize;
            boundary = ptrs - 1 - (block & (ptrs - 1));
        } else {
            return_errno!(Errno::EINVAL);
        }

        Ok(BlockPath {
            depth,
            offsets,
            boundary,
        })
    }

    /// Maps a logical block to a physical block (read-only path).
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:783 (ext2_get_block)
    pub(super) fn get_block(&self, iblock: u32) -> Result<Option<Bid>> {
        let path = self.block_to_path(iblock)?;
        if path.depth == 0 {
            return Ok(None);
        }

        let mut bid = self.block_ptrs[path.offsets[0] as usize];
        if bid == 0 {
            return Ok(None);
        }

        let fs = self.fs.upgrade().ok_or_else(|| Error::new(Errno::EIO))?;
        for level in 1..path.depth {
            let mut buf = vec![0u8; BLOCK_SIZE];
            if fs
                .block_device()
                .read_bytes(Bid::new(bid as u64).to_offset(), &mut buf)
                .is_err()
            {
                return_errno!(Errno::EIO);
            }

            let mut reader = VmReader::from(buf.as_slice());
            let offset_bytes = (path.offsets[level] as usize)
                .saturating_mul(size_of::<u32>());
            let next = reader
                .skip(offset_bytes)
                .read_val::<u32>()
                .map_err(|_| Error::new(Errno::EIO))?;
            if next == 0 {
                return Ok(None);
            }
            bid = next;
        }

        Ok(Some(Bid::new(bid as u64)))
    }
}

///TODO: Refactor this with a more rusty approach (e.g. enum).
/// Block path offsets for direct/indirect traversal.
#[derive(Clone, Copy, Debug)]
pub(super) struct BlockPath {
    pub depth: usize,
    pub offsets: [u32; 4],
    pub boundary: u32,
}

/// In-memory inode descriptor (raw on-disk view).
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeDesc {
    pub raw: RawInode,
}

/// On-disk inode structure (128 bytes for GOOD_OLD_REV).
///
/// Linux: /root/linux/fs/ext2/ext2.h:290 (struct ext2_inode)
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct RawInode {
    pub mode: u16,         // i_mode
    pub uid: u16,          // i_uid (low 16 bits)
    pub size_lo: u32,      // i_size
    pub atime: u32,        // i_atime
    pub ctime: u32,        // i_ctime
    pub mtime: u32,        // i_mtime
    pub dtime: u32,        // i_dtime
    pub gid: u16,          // i_gid (low 16 bits)
    pub links_count: u16,  // i_links_count
    pub blocks: u32,       // i_blocks (512-byte sectors)
    pub flags: u32,        // i_flags
    pub osd1: u32,         // osd1.linux1.l_i_reserved1
    pub block: [u32; 15],  // i_block
    pub generation: u32,   // i_generation
    pub file_acl: u32,     // i_file_acl
    pub size_high: u32,    // i_dir_acl (size high)
    pub faddr: u32,        // i_faddr
    pub frag: u8,          // osd2.linux2.l_i_frag
    pub fsize: u8,         // osd2.linux2.l_i_fsize
    pub pad1: u16,         // osd2.linux2.i_pad1
    pub uid_high: u16,     // osd2.linux2.l_i_uid_high
    pub gid_high: u16,     // osd2.linux2.l_i_gid_high
    pub reserved2: u32,    // osd2.linux2.l_i_reserved2
}

const_assert!(size_of::<RawInode>() == 128);

/// On-disk directory entry with file_type (header only; name follows on disk).
///
/// Linux: /root/linux/fs/ext2/ext2.h:615 (struct ext2_dir_entry_2)
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct RawDirEntry {
    pub inode: u32,    // inode
    pub rec_len: u16,  // rec_len
    pub name_len: u8,  // name_len
    pub file_type: u8, // file_type
}

const_assert!(size_of::<RawDirEntry>() == 8);
