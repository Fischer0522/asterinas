// SPDX-License-Identifier: MPL-2.0

use core::mem::size_of;

use super::inode::RawDirEntry;
use super::prelude::*;
use crate::fs::utils::{CStr256, NAME_MAX};

/// A parsed directory entry.
#[derive(Clone, Debug)]
pub(super) struct DirEntry {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
    pub name: CStr256,
}

impl DirEntry {
    /// Converts on-disk rec_len to host-endian.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:38 (ext2_rec_len_from_disk)
    pub(super) fn rec_len_from_disk(rec_len: u16) -> u16 {
        rec_len
    }

    /// Returns the minimal record length for a given name length.
    ///
    /// Linux: /root/linux/fs/ext2/ext2.h:607 (EXT2_DIR_REC_LEN)
    pub(super) fn dir_rec_len(name_len: usize) -> u16 {
        (((name_len + 8 + 3) & !3) as u16)
    }

    /// Validates a directory entry layout.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:99 (ext2_check_folio)
    pub(super) fn validate(
        rec_len: u16,
        name_len: u8,
        offset: usize,
        limit: usize,
        max_inumber: u32,
        inode: u32,
    ) -> Result<()> {
        let rec_len_usize = rec_len as usize;
        let min_rec_len = Self::dir_rec_len(1) as usize;
        if rec_len_usize < min_rec_len {
            return_errno!(Errno::EIO);
        }
        if (rec_len & 3) != 0 {
            return_errno!(Errno::EIO);
        }
        if name_len as usize > NAME_MAX {
            return_errno!(Errno::EIO);
        }
        let need = Self::dir_rec_len(name_len as usize) as usize;
        if rec_len_usize < need {
            return_errno!(Errno::EIO);
        }
        if offset.saturating_add(rec_len_usize) > limit {
            return_errno!(Errno::EIO);
        }
        if inode > max_inumber {
            return_errno!(Errno::EIO);
        }
        Ok(())
    }

    /// Parses a directory entry at a given offset.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:99 (ext2_check_folio)
    pub(super) fn parse_at(
        buf: &[u8],
        offset: usize,
        limit: usize,
        max_inumber: u32,
    ) -> Result<DirEntry> {
        if offset >= limit {
            return_errno!(Errno::EIO);
        }
        let header_len = size_of::<RawDirEntry>();
        if offset.saturating_add(header_len) > limit {
            return_errno!(Errno::EIO);
        }

        let raw = {
            let mut reader = VmReader::from(&buf[offset..limit]);
            reader.read_val::<RawDirEntry>().map_err(|_| Error::new(Errno::EIO))?
        };

        let rec_len = Self::rec_len_from_disk(raw.rec_len);
        Self::validate(rec_len, raw.name_len, offset, limit, max_inumber, raw.inode)?;

        let name_len = raw.name_len as usize;
        let name_start = offset + header_len;
        let name_end = name_start.saturating_add(name_len);
        if name_end > limit {
            return_errno!(Errno::EIO);
        }
        let name = CStr256::from(&buf[name_start..name_end]);

        Ok(DirEntry {
            inode: raw.inode,
            rec_len,
            name_len: raw.name_len,
            file_type: raw.file_type,
            name,
        })
    }
}

/// Directory entry iterator over a single block buffer.
pub(super) struct DirEntryIter<'a> {
    buf: &'a [u8],
    offset: usize,
    limit: usize,
    max_inumber: u32,
}

impl<'a> DirEntryIter<'a> {
    pub(super) fn new(buf: &'a [u8], limit: usize, max_inumber: u32) -> Result<Self> {
        if limit == 0 || limit > buf.len() {
            return_errno!(Errno::EIO);
        }
        Ok(Self {
            buf,
            offset: 0,
            limit,
            max_inumber,
        })
    }

    pub(super) fn next_entry(&mut self) -> Result<Option<DirEntry>> {
        if self.offset == self.limit {
            return Ok(None);
        }
        if self.offset > self.limit {
            return_errno!(Errno::EIO);
        }

        let entry = DirEntry::parse_at(self.buf, self.offset, self.limit, self.max_inumber)?;
        let rec_len = entry.rec_len as usize;
        if self.offset.saturating_add(rec_len) > self.limit {
            return_errno!(Errno::EIO);
        }
        self.offset += rec_len;
        Ok(Some(entry))
    }
}
