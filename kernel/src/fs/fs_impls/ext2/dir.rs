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

/// A view over a directory region in the page cache.
///
/// Reads and writes entry headers and names directly on the page cache
/// without copying entire blocks into temporary buffers.
pub(super) struct DirBlock<'a> {
    page_cache: &'a PageCache,
    /// Absolute byte offset of this block in the page cache.
    offset: usize,
    /// Valid data length within this block.
    limit: usize,
}

impl<'a> DirBlock<'a> {
    const HEADER_LEN: usize = size_of::<RawDirEntry>();

    /// Creates a `DirBlock` view over a region of the page cache.
    pub(super) fn new(page_cache: &'a PageCache, offset: usize, limit: usize) -> Self {
        Self {
            page_cache,
            offset,
            limit,
        }
    }

    /// Creates a `DirBlock` from a block index.
    pub(super) fn from_index(
        page_cache: &'a PageCache,
        block_idx: usize,
        block_size: usize,
        file_size: usize,
    ) -> Self {
        let offset = block_idx * block_size;
        let limit = file_size.saturating_sub(offset).min(block_size);
        Self::new(page_cache, offset, limit)
    }

    /// Returns an iterator over entry headers in this block.
    ///
    /// Each item yields `(offset_within_block, RawDirEntry)`. Names are not
    /// read — use [`read_name`] on demand to avoid unnecessary copies.
    pub(super) fn iter_entries(&self) -> DirBlockIter<'a> {
        DirBlockIter {
            page_cache: self.page_cache,
            cursor: self.offset,
            end: self.offset + self.limit,
        }
    }

    /// Reads the name bytes of an entry at `entry_offset` within this block.
    pub(super) fn read_name(
        &self,
        entry_offset: usize,
        name_len: usize,
        buf: &mut [u8],
    ) -> Result<()> {
        let abs = self.offset + entry_offset + Self::HEADER_LEN;
        self.page_cache.read_bytes(abs, &mut buf[..name_len])?;
        Ok(())
    }

    /// Writes a complete directory entry (header + name) at `entry_offset`.
    pub(super) fn write_entry(
        &self,
        entry_offset: usize,
        ino: u32,
        rec_len: u16,
        name: &[u8],
        ft: u8,
    ) -> Result<()> {
        let header = RawDirEntry {
            inode: ino.to_le(),
            rec_len: rec_len.to_le(),
            name_len: name.len() as u8,
            file_type: ft,
        };
        let abs = self.offset + entry_offset;
        self.page_cache.write_val(abs, &header)?;
        if !name.is_empty() {
            self.page_cache
                .write_bytes(abs + Self::HEADER_LEN, name)?;
        }
        Ok(())
    }

    /// Overwrites the inode number field at an entry offset.
    pub(super) fn set_inode(&self, entry_offset: usize, ino: u32) -> Result<()> {
        let abs = self.offset + entry_offset;
        self.page_cache.write_bytes(abs, &ino.to_le_bytes())?;
        Ok(())
    }

    /// Overwrites the `rec_len` field at an entry offset.
    pub(super) fn set_rec_len(&self, entry_offset: usize, rec_len: u16) -> Result<()> {
        let abs = self.offset + entry_offset + 4;
        self.page_cache.write_bytes(abs, &rec_len.to_le_bytes())?;
        Ok(())
    }

    /// Overwrites the file type byte at an entry offset.
    pub(super) fn set_file_type(&self, entry_offset: usize, ft: u8) -> Result<()> {
        let abs = self.offset + entry_offset + 7;
        self.page_cache.write_bytes(abs, &[ft])?;
        Ok(())
    }

    /// Deletes an entry by zeroing its inode and merging `rec_len` with the predecessor.
    pub(super) fn delete_entry(
        &self,
        chunk_size: usize,
        entry_offset: usize,
        entry_rec_len: usize,
    ) -> Result<()> {
        let to = entry_offset.saturating_add(entry_rec_len);
        if entry_rec_len == 0 || to > self.limit {
            return_errno_with_message!(Errno::EIO, "invalid dir entry rec_len for delete");
        }

        // Walk from chunk-aligned start to find the predecessor entry.
        let chunk_mask = !(chunk_size.saturating_sub(1));
        let from = entry_offset & chunk_mask;
        let mut de_offset = from;
        let mut prev_offset = None;

        while de_offset < entry_offset {
            let header: RawDirEntry = self
                .page_cache
                .read_val(self.offset + de_offset)
                .map_err(|_| Error::with_message(Errno::EIO, "dir entry header out of bounds"))?;
            let rec_len = u16::from_le(header.rec_len) as usize;
            if rec_len == 0 {
                return_errno_with_message!(Errno::EIO, "zero rec_len in dir entry chain");
            }
            let next = de_offset.saturating_add(rec_len);
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
            let merged_len = to.saturating_sub(prev) as u16;
            self.set_rec_len(prev, merged_len)?;
        }

        self.set_inode(entry_offset, 0)?;
        Ok(())
    }
}

/// Iterates directory entry headers directly from the page cache.
///
/// Each call to `next_entry` reads one 8-byte `RawDirEntry` header.
/// Names are not read — callers use [`DirBlock::read_name`] on demand.
pub(super) struct DirBlockIter<'a> {
    page_cache: &'a PageCache,
    /// Absolute offset of the next entry in the page cache.
    cursor: usize,
    /// Absolute end offset (block_offset + limit).
    end: usize,
}

impl DirBlockIter<'_> {
    /// Reads the next entry header. Returns `(offset_within_block, header)`.
    ///
    /// The `offset_within_block` is relative to the `DirBlock`'s start, not absolute.
    pub(super) fn next_entry(
        &mut self,
        block_offset: usize,
    ) -> Result<Option<(usize, RawDirEntry)>> {
        if self.cursor >= self.end {
            return Ok(None);
        }

        let header: RawDirEntry = self
            .page_cache
            .read_val(self.cursor)
            .map_err(|_| Error::with_message(Errno::EIO, "failed to read dir entry header"))?;

        let rec_len = u16::from_le(header.rec_len) as usize;
        if rec_len < Self::MIN_REC_LEN || (rec_len & 3) != 0 {
            return_errno_with_message!(Errno::EIO, "invalid dir entry rec_len");
        }
        if self.cursor.saturating_add(rec_len) > self.end {
            return_errno_with_message!(Errno::EIO, "dir entry rec_len exceeds block limit");
        }

        let entry_offset = self.cursor - block_offset;
        self.cursor += rec_len;

        Ok(Some((entry_offset, header)))
    }

    const MIN_REC_LEN: usize = 12; // dir_rec_len(1) = (1 + 8 + 3) & !3
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
