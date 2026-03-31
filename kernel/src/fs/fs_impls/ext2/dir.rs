// SPDX-License-Identifier: MPL-2.0

use core::mem::size_of;

use super::{inode::RawDirEntry, prelude::*};
use crate::fs::utils::NAME_MAX;

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
    pub(super) fn rec_len_from_disk(rec_len: u16) -> u16 {
        u16::from_le(rec_len)
    }

    /// Returns the minimal record length for a given name length.
    ///
    pub(super) fn dir_rec_len(name_len: usize) -> u16 {
        ((name_len + 8 + 3) & !3) as u16
    }

    /// Validates a directory entry layout.
    ///
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
            return_errno_with_message!(
                Errno::EIO,
                "Invalid record length: rec_len  is smaller than minimal"
            );
        }
        if (rec_len & 3) != 0 {
            return_errno_with_message!(Errno::EIO, "Invalid record length: rec_len is not aligned");
        }
        if name_len as usize > NAME_MAX {
            return_errno_with_message!(Errno::EIO, "Invalid name length: name_len is too long");
        }
        let need = Self::dir_rec_len(name_len as usize) as usize;
        if rec_len_usize < need {
            return_errno_with_message!(
                Errno::EIO,
                "Invalid record length: rec_len is smaller than needed"
            );
        }
        if offset.saturating_add(rec_len_usize) > limit {
            return_errno_with_message!(
                Errno::EIO,
                "Invalid offset: offset + rec_len is out of limit"
            );
        }
        if inode > max_inumber {
            return_errno_with_message!(Errno::EIO, "Invalid inode: inode is out of max_inumber");
        }
        Ok(())
    }

    /// Parses a directory entry at a given offset.
    ///
    pub(super) fn parse_at(
        buf: &[u8],
        offset: usize,
        limit: usize,
        max_inumber: u32,
    ) -> Result<DirEntry> {
        if offset >= limit {
            return_errno_with_message!(Errno::EIO, "dir entry offset beyond block limit");
        }
        let header_len = size_of::<RawDirEntry>();
        if offset.saturating_add(header_len) > limit {
            return_errno_with_message!(Errno::EIO, "dir entry header crosses block limit");
        }

        let raw = {
            let mut reader = VmReader::from(&buf[offset..limit]);
            reader
                .read_val::<RawDirEntry>()
                .map_err(|_| Error::with_message(Errno::EIO, "failed to read dir entry header"))?
        };

        let rec_len = Self::rec_len_from_disk(raw.rec_len);
        Self::validate(rec_len, raw.name_len, offset, limit, max_inumber, raw.inode)?;

        let name_len = raw.name_len as usize;
        let name_start = offset + header_len;
        let name_end = name_start.saturating_add(name_len);
        if name_end > limit {
            return_errno_with_message!(Errno::EIO, "dir entry name extends beyond block limit");
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
#[derive(Debug)]
pub(super) struct DirEntryIter<'a> {
    buf: &'a [u8],
    offset: usize,
    limit: usize,
    max_inumber: u32,
}

impl<'a> DirEntryIter<'a> {
    pub(super) fn new(buf: &'a [u8], limit: usize, max_inumber: u32) -> Result<Self> {
        if limit == 0 || limit > buf.len() {
            return_errno_with_message!(Errno::EIO, "invalid dir block limit");
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
            return_errno_with_message!(Errno::EIO, "dir iterator offset past limit");
        }

        let entry = DirEntry::parse_at(self.buf, self.offset, self.limit, self.max_inumber)?;
        let rec_len = entry.rec_len as usize;
        if self.offset.saturating_add(rec_len) > self.limit {
            return_errno_with_message!(Errno::EIO, "dir entry rec_len exceeds block limit");
        }
        self.offset += rec_len;
        Ok(Some(entry))
    }
}

/// A mutable view over a single directory block buffer.
pub(super) struct DirBlock {
    buf: Vec<u8>,
    /// Valid data length within `buf` (may be less than `buf.len()` for the last block).
    limit: usize,
    max_inumber: u32,
}

impl DirBlock {
    /// Creates a new zeroed directory block.
    pub(super) fn new_zeroed(block_size: usize, max_inumber: u32) -> Self {
        Self {
            buf: vec![0u8; block_size],
            limit: block_size,
            max_inumber,
        }
    }

    /// Creates a `DirBlock` from existing data (e.g. read from page cache).
    pub(super) fn new(buf: Vec<u8>, limit: usize, max_inumber: u32) -> Self {
        Self {
            buf,
            limit,
            max_inumber,
        }
    }

    /// Returns the underlying buffer for writing back to page cache.
    pub(super) fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Returns an iterator over directory entries in this block.
    pub(super) fn iter(&self) -> Result<DirEntryIter<'_>> {
        DirEntryIter::new(&self.buf, self.limit, self.max_inumber)
    }

    /// Collects all entries with their byte offsets.
    pub(super) fn entries(&self) -> Result<Vec<(usize, DirEntry)>> {
        let mut iter = self.iter()?;
        let mut entries = Vec::new();
        let mut entry_offset = 0usize;

        while let Some(entry) = iter.next_entry()? {
            let rec_len = entry.rec_len as usize;
            entries.push((entry_offset, entry));
            entry_offset = entry_offset.saturating_add(rec_len);
        }

        Ok(entries)
    }

    /// Serializes a directory entry at the given offset.
    pub(super) fn write_entry(
        &mut self,
        offset: usize,
        ino: u32,
        rec_len: u16,
        name: &[u8],
        ft: u8,
    ) -> Result<()> {
        let header_len = size_of::<RawDirEntry>();
        if name.len() > u8::MAX as usize {
            return_errno_with_message!(Errno::EIO, "dir entry name too long");
        }

        let rec_len_usize = rec_len as usize;
        if rec_len_usize < DirEntry::dir_rec_len(name.len()) as usize {
            return_errno_with_message!(Errno::EIO, "dir entry rec_len too small");
        }
        if rec_len_usize & 3 != 0 {
            return_errno_with_message!(Errno::EIO, "dir entry rec_len not aligned");
        }
        if offset.saturating_add(rec_len_usize) > self.buf.len() {
            return_errno_with_message!(Errno::EIO, "dir entry exceeds buffer");
        }

        let name_start = offset + header_len;
        let name_end = name_start.saturating_add(name.len());
        if name_end > offset.saturating_add(rec_len_usize) {
            return_errno_with_message!(Errno::EIO, "dir entry name exceeds rec_len");
        }

        self.buf[offset..offset + 4].copy_from_slice(&ino.to_le_bytes());
        self.buf[offset + 4..offset + 6].copy_from_slice(&rec_len.to_le_bytes());
        self.buf[offset + 6] = name.len() as u8;
        self.buf[offset + 7] = ft;
        self.buf[name_start..name_end].copy_from_slice(name);
        Ok(())
    }

    /// Deletes an entry by zeroing its inode and merging `rec_len` with the predecessor.
    pub(super) fn delete_entry(
        &mut self,
        chunk_size: usize,
        entry_offset: usize,
        entry_rec_len: usize,
    ) -> Result<()> {
        let to = entry_offset.saturating_add(entry_rec_len);
        if entry_rec_len == 0 || to > self.limit {
            return_errno_with_message!(Errno::EIO, "invalid dir entry rec_len for delete");
        }

        let chunk_mask = !(chunk_size.saturating_sub(1));
        let from = entry_offset & chunk_mask;
        let mut de_offset = from;
        let mut prev_offset = None;

        while de_offset < entry_offset {
            if de_offset.saturating_add(size_of::<RawDirEntry>()) > self.limit {
                return_errno_with_message!(Errno::EIO, "dir entry header out of bounds");
            }

            let rec_len =
                u16::from_le_bytes([self.buf[de_offset + 4], self.buf[de_offset + 5]]);
            if rec_len == 0 {
                return_errno_with_message!(Errno::EIO, "zero rec_len in dir entry chain");
            }

            let next = de_offset.saturating_add(rec_len as usize);
            if next > self.limit {
                return_errno_with_message!(Errno::EIO, "dir entry chain exceeds block limit");
            }

            prev_offset = Some(de_offset);
            de_offset = next;
        }

        if de_offset != entry_offset {
            return_errno_with_message!(Errno::EIO, "dir entry chain offset mismatch");
        }

        if let Some(prev) = prev_offset {
            let merged_len = to.saturating_sub(prev);
            self.set_rec_len(prev, merged_len as u16)?;
        }

        self.set_inode(entry_offset, 0)?;
        Ok(())
    }

    /// Overwrites the inode number field at an entry offset.
    pub(super) fn set_inode(&mut self, offset: usize, ino: u32) -> Result<()> {
        if offset.saturating_add(size_of::<RawDirEntry>()) > self.buf.len() {
            return_errno_with_message!(Errno::EIO, "dir entry header out of bounds");
        }
        self.buf[offset..offset + 4].copy_from_slice(&ino.to_le_bytes());
        Ok(())
    }

    /// Overwrites the `rec_len` field at an entry offset.
    pub(super) fn set_rec_len(&mut self, offset: usize, rec_len: u16) -> Result<()> {
        if rec_len == 0 || rec_len & 3 != 0 {
            return_errno_with_message!(Errno::EIO, "invalid rec_len value");
        }
        if offset.saturating_add(size_of::<RawDirEntry>()) > self.buf.len() {
            return_errno_with_message!(Errno::EIO, "dir entry header out of bounds");
        }
        if offset.saturating_add(rec_len as usize) > self.buf.len() {
            return_errno_with_message!(Errno::EIO, "rec_len exceeds buffer");
        }
        self.buf[offset + 4..offset + 6].copy_from_slice(&rec_len.to_le_bytes());
        Ok(())
    }

    /// Overwrites the file type byte at an entry offset.
    pub(super) fn set_file_type(&mut self, offset: usize, ft: u8) -> Result<()> {
        if offset.saturating_add(size_of::<RawDirEntry>()) > self.buf.len() {
            return_errno_with_message!(Errno::EIO, "dir entry header out of bounds");
        }
        self.buf[offset + 7] = ft;
        Ok(())
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::*;
    use crate::fs::fs_impls::ext2::testkit::encode_dir_entry;

    fn encode_entry(
        buf: &mut [u8],
        offset: usize,
        inode: u32,
        rec_len: u16,
        name: &[u8],
        file_type: u8,
    ) {
        encode_dir_entry(buf, offset, inode, rec_len, name, file_type);
    }

    #[ktest]
    fn parse_valid_entries_ok() {
        // Validate ext2 record-length decoding and 4-byte alignment rules.
        assert_eq!(DirEntry::rec_len_from_disk(0x1234), 0x1234);
        assert_eq!(DirEntry::dir_rec_len(0), 8);
        assert_eq!(DirEntry::dir_rec_len(1), 12);
        assert_eq!(DirEntry::dir_rec_len(NAME_MAX), 264);

        let rec_len_first = DirEntry::dir_rec_len(2);
        let rec_len_second = DirEntry::dir_rec_len(NAME_MAX);
        let limit = rec_len_first as usize + rec_len_second as usize;
        let mut buf = vec![0u8; limit];
        let long_name = [b'x'; NAME_MAX];

        // Build one short entry and one NAME_MAX entry in a single block slice.
        encode_entry(&mut buf, 0, 7, rec_len_first, b"ab", 1);
        encode_entry(
            &mut buf,
            rec_len_first as usize,
            9,
            rec_len_second,
            &long_name,
            2,
        );

        // Both entries pass ext2_check_folio-style validation.
        DirEntry::validate(rec_len_first, 2, 0, limit, 1024, 7).unwrap();
        DirEntry::validate(
            rec_len_second,
            NAME_MAX as u8,
            rec_len_first as usize,
            limit,
            1024,
            9,
        )
        .unwrap();

        // Parse both offsets and verify parsed fields.
        let first = DirEntry::parse_at(&buf, 0, limit, 1024).unwrap();
        assert_eq!(first.inode, 7);
        assert_eq!(first.rec_len, rec_len_first);
        assert_eq!(first.name_len, 2);
        assert_eq!(first.file_type, 1);
        assert_eq!(first.name.as_bytes(), b"ab");

        let second = DirEntry::parse_at(&buf, rec_len_first as usize, limit, 1024).unwrap();
        assert_eq!(second.inode, 9);
        assert_eq!(second.rec_len, rec_len_second);
        assert_eq!(second.name_len, NAME_MAX as u8);
        assert_eq!(second.file_type, 2);
        assert_eq!(second.name.as_bytes(), long_name.as_slice());

        // Iterator consumes both entries then reaches end exactly at limit.
        let mut iter = DirEntryIter::new(&buf, limit, 1024).unwrap();
        assert_eq!(iter.next_entry().unwrap().unwrap().inode, 7);
        assert_eq!(
            iter.next_entry().unwrap().unwrap().name.as_bytes(),
            long_name.as_slice()
        );
        assert!(iter.next_entry().unwrap().is_none());
    }

    #[ktest]
    fn parse_invalid_entries_returns_err() {
        // validate() failures: short record, unaligned rec_len, name/len mismatch,
        // record spanning limit, and inode out of range.
        assert_eq!(
            DirEntry::validate(8, 1, 0, 64, 128, 1).unwrap_err().error(),
            Errno::EIO
        );
        assert_eq!(
            DirEntry::validate(14, 1, 0, 64, 128, 1)
                .unwrap_err()
                .error(),
            Errno::EIO
        );
        assert_eq!(
            DirEntry::validate(12, 10, 0, 64, 128, 1)
                .unwrap_err()
                .error(),
            Errno::EIO
        );
        assert_eq!(
            DirEntry::validate(16, 4, 56, 64, 128, 1)
                .unwrap_err()
                .error(),
            Errno::EIO
        );
        assert_eq!(
            DirEntry::validate(12, 1, 0, 64, 8, 9).unwrap_err().error(),
            Errno::EIO
        );

        // parse_at() boundary checks: offset==limit and header crossing limit.
        let mut valid_buf = [0u8; 16];
        encode_entry(&mut valid_buf, 0, 3, 12, b"ab", 1);
        assert_eq!(
            DirEntry::parse_at(&valid_buf, 12, 12, 32)
                .unwrap_err()
                .error(),
            Errno::EIO
        );
        assert_eq!(
            DirEntry::parse_at(&valid_buf, 8, 12, 32)
                .unwrap_err()
                .error(),
            Errno::EIO
        );

        // Corner case: raw entry exists but rec_len is below minimal legal size.
        let mut short_rec_len_buf = [0u8; 12];
        short_rec_len_buf[0..4].copy_from_slice(&1u32.to_le_bytes());
        short_rec_len_buf[4..6].copy_from_slice(&8u16.to_le_bytes());
        short_rec_len_buf[6] = 1;
        short_rec_len_buf[7] = 1;
        assert_eq!(
            DirEntry::parse_at(&short_rec_len_buf, 0, 12, 32)
                .unwrap_err()
                .error(),
            Errno::EIO
        );

        // Iterator construction rejects empty limit and limit beyond buffer.
        let empty = [0u8; 8];
        assert_eq!(
            DirEntryIter::new(&empty, 0, 16).unwrap_err().error(),
            Errno::EIO
        );
        assert_eq!(
            DirEntryIter::new(&empty, 9, 16).unwrap_err().error(),
            Errno::EIO
        );

        // Corner case: first record valid, trailing bytes cannot hold next header.
        let mut malformed_tail = [0u8; 20];
        encode_entry(&mut malformed_tail, 0, 1, 16, b"a", 1);
        let mut iter = DirEntryIter::new(&malformed_tail, 20, 64).unwrap();
        assert!(iter.next_entry().unwrap().is_some());
        assert_eq!(iter.next_entry().unwrap_err().error(), Errno::EIO);

        // Corner case: first entry malformed immediately (unaligned rec_len).
        let mut bad_first = [0u8; 16];
        bad_first[0..4].copy_from_slice(&5u32.to_le_bytes());
        bad_first[4..6].copy_from_slice(&14u16.to_le_bytes());
        bad_first[6] = 1;
        bad_first[7] = 1;
        let mut iter = DirEntryIter::new(&bad_first, 16, 64).unwrap();
        assert_eq!(iter.next_entry().unwrap_err().error(), Errno::EIO);
    }
}
