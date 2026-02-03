// SPDX-License-Identifier: MPL-2.0

use core::mem::size_of;

use ostd::const_assert;

use super::prelude::*;

#[derive(Clone, Copy, Debug)]
pub struct FilePerm(u16);

impl FilePerm {
    pub fn from_bits_truncate(bits: u16) -> Self {
        Self(bits)
    }
}

#[derive(Debug)]
pub struct Inode;

impl Inode {
    pub fn create(&self, _name: &str, _type_: InodeType, _perm: FilePerm) -> Result<Arc<Inode>> {
        return_errno!(Errno::ENOSYS);
    }

    pub fn write_at(&self, _offset: usize, _data: &[u8]) -> Result<usize> {
        return_errno!(Errno::ENOSYS);
    }
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
