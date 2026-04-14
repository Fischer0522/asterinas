// SPDX-License-Identifier: MPL-2.0

//! Ext2 directory entry parsing and iteration.

use core::mem::size_of;

use ostd::const_assert;

use super::prelude::*;

pub(super) const DOT_BYTE: &[u8] = b".";
pub(super) const DOT_DOT_BYTE: &[u8] = b"..";

/// On-disk directory entry header.
///
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct DirEntryHeader {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
}

/// Parsed ext2 directory entry (header + file name).
#[derive(Clone, Debug)]
pub(super) struct DirEntry {
    pub header: DirEntryHeader,
    pub name: CStr256,
}

const_assert!(size_of::<DirEntryHeader>() == 8);

/// Directory entry type mapping for the ext2 `file_type` field.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum DirEntryFileType {
    /// Unknown file type.
    Unknown = 0,
    /// Regular file.
    File = 1,
    /// Directory.
    Dir = 2,
    /// Character device.
    Char = 3,
    /// Block device.
    Block = 4,
    /// FIFO.
    Fifo = 5,
    /// Socket.
    Socket = 6,
    /// Symlink.
    Symlink = 7,
}

impl From<u8> for DirEntryFileType {
    fn from(value: u8) -> Self {
        match value {
            1 => Self::File,
            2 => Self::Dir,
            3 => Self::Char,
            4 => Self::Block,
            5 => Self::Fifo,
            6 => Self::Socket,
            7 => Self::Symlink,
            _ => Self::Unknown,
        }
    }
}

impl From<DirEntryFileType> for InodeType {
    fn from(file_type: DirEntryFileType) -> Self {
        match file_type {
            DirEntryFileType::Unknown => Self::Unknown,
            DirEntryFileType::File => Self::File,
            DirEntryFileType::Dir => Self::Dir,
            DirEntryFileType::Char => Self::CharDevice,
            DirEntryFileType::Block => Self::BlockDevice,
            DirEntryFileType::Fifo => Self::NamedPipe,
            DirEntryFileType::Socket => Self::Socket,
            DirEntryFileType::Symlink => Self::SymLink,
        }
    }
}

impl From<InodeType> for DirEntryFileType {
    fn from(type_: InodeType) -> Self {
        match type_ {
            InodeType::File => DirEntryFileType::File,
            InodeType::Dir => DirEntryFileType::Dir,
            InodeType::CharDevice => DirEntryFileType::Char,
            InodeType::BlockDevice => DirEntryFileType::Block,
            InodeType::NamedPipe => DirEntryFileType::Fifo,
            InodeType::Socket => DirEntryFileType::Socket,
            InodeType::SymLink => DirEntryFileType::Symlink,
            _ => DirEntryFileType::Unknown,
        }
    }
}

impl DirEntryHeader {
    /// Returns the minimal record length for a given name length.
    pub(super) fn dir_rec_len(name_len: usize) -> u16 {
        ((name_len + 8 + 3) & !3) as u16
    }
}

/// A view over a directory region in the page cache.
///
/// Reads and writes entry headers and names directly on the page cache.
pub(super) struct DirBlock<'a> {
    page_cache: &'a PageCache,
    /// Absolute byte offset of this block in the page cache.
    offset: usize,
    /// Valid data length within this block.
    limit: usize,
}

impl<'a> DirBlock<'a> {
    const HEADER_LEN: usize = size_of::<DirEntryHeader>();

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
        file_size: usize,
    ) -> Self {
        let offset = block_idx * BLOCK_SIZE;
        let limit = file_size.saturating_sub(offset).min(BLOCK_SIZE);
        Self::new(page_cache, offset, limit)
    }

    /// Returns an iterator over entries in this block.
    pub(super) fn iter_entries(&self) -> DirBlockIter<'a> {
        DirBlockIter {
            page_cache: self.page_cache,
            cursor: self.offset,
            end: self.offset + self.limit,
        }
    }

    /// Writes a complete directory entry (header + name) at `entry_offset`.
    pub(super) fn write_entry(
        &self,
        entry_offset: usize,
        ino: u32,
        rec_len: u16,
        name: &[u8],
        ft: DirEntryFileType,
    ) -> Result<()> {
        let header = DirEntryHeader {
            inode: ino.to_le(),
            rec_len: rec_len.to_le(),
            name_len: name.len() as u8,
            file_type: ft as u8,
        };
        let abs = self.offset + entry_offset;
        self.page_cache.write_val(abs, &header)?;
        if !name.is_empty() {
            self.page_cache.write_bytes(abs + Self::HEADER_LEN, name)?;
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
    pub(super) fn set_file_type(&self, entry_offset: usize, ft: DirEntryFileType) -> Result<()> {
        let abs = self.offset + entry_offset + 7;
        self.page_cache.write_bytes(abs, &[ft as u8])?;
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
            let header: DirEntryHeader = self
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
            let merged_len = (to - prev) as u16;
            self.set_rec_len(prev, merged_len)?;
        }

        self.set_inode(entry_offset, 0)?;
        Ok(())
    }
}

/// Iterates directory entries directly from the page cache.
pub(super) struct DirBlockIter<'a> {
    page_cache: &'a PageCache,
    /// Absolute offset of the next entry in the page cache.
    cursor: usize,
    /// Absolute end offset (block_offset + limit).
    end: usize,
}

impl DirBlockIter<'_> {
    /// Reads the next entry (header + name). Returns `(offset_within_block, DirEntry)`.
    ///
    /// The `offset_within_block` is relative to the `DirBlock`'s start, not absolute.
    pub(super) fn next_entry(&mut self, block_offset: usize) -> Result<Option<(usize, DirEntry)>> {
        if self.cursor >= self.end {
            return Ok(None);
        }

        let header: DirEntryHeader = self
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

        let name_len = header.name_len as usize;
        let name = if name_len > 0 && header.inode != 0 {
            let name_abs = self.cursor + Self::HEADER_LEN;
            let mut buf = [0u8; u8::MAX as usize];
            self.page_cache.read_bytes(name_abs, &mut buf[..name_len])?;
            CStr256::from(&buf[..name_len])
        } else {
            CStr256::from(&[] as &[u8])
        };

        let entry_offset = self.cursor - block_offset;
        self.cursor += rec_len;

        Ok(Some((entry_offset, DirEntry { header, name })))
    }

    const HEADER_LEN: usize = size_of::<DirEntryHeader>();
    const MIN_REC_LEN: usize = 12; // dir_rec_len(1) = (1 + 8 + 3) & !3
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::*;
    use crate::fs::utils::NAME_MAX;

    #[ktest]
    fn dir_rec_len_ok() {
        assert_eq!(DirEntryHeader::dir_rec_len(0), 8);
        assert_eq!(DirEntryHeader::dir_rec_len(1), 12);
        assert_eq!(DirEntryHeader::dir_rec_len(NAME_MAX), 264);
    }

    #[ktest]
    fn dir_rec_len_boundary_values() {
        // 4-byte alignment: name_len 1..4 all round to 12.
        assert_eq!(DirEntryHeader::dir_rec_len(1), 12);
        assert_eq!(DirEntryHeader::dir_rec_len(2), 12);
        assert_eq!(DirEntryHeader::dir_rec_len(3), 12);
        assert_eq!(DirEntryHeader::dir_rec_len(4), 12);
        // name_len=5 crosses to next alignment bucket.
        assert_eq!(DirEntryHeader::dir_rec_len(5), 16);
        // Maximum name length (255).
        assert_eq!(DirEntryHeader::dir_rec_len(255), 264);
        // Verify alignment: result is always 4-byte aligned.
        for name_len in 0..=NAME_MAX {
            assert_eq!(DirEntryHeader::dir_rec_len(name_len) % 4, 0);
        }
    }
}
