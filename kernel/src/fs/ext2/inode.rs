// SPDX-License-Identifier: MPL-2.0

use core::mem::size_of;

use aster_virtio::device::socket::error;
use ostd::const_assert;

use super::{fs::Ext2, prelude::*};
use crate::fs::ext2::dir::{DirEntry, DirEntryIter};

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
    inner: RwMutex<InodeInner>,
    block_group_idx: usize,
    fs: Weak<Ext2>,
}

impl Inode {
    pub fn new(
        ino: u32,
        type_: InodeType,
        desc: Dirty<InodeDesc>,
        block_group_idx: usize,
        fs: Weak<Ext2>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak_self| Self {
            ino,
            type_,
            inner: RwMutex::new(InodeInner::new(desc, weak_self.clone(), fs.clone())),
            block_group_idx,
            fs,
        })
    }
}

#[derive(Debug)]
pub struct InodeInner {
    desc: Dirty<InodeDesc>,
    is_freed: bool,
    weak_self: Weak<Inode>,
    fs: Weak<Ext2>,
}

#[derive(Debug)]
struct DeleteTarget {
    block_bid: Bid,
    block_buf: Vec<u8>,
    limit: usize,
    entry_offset: usize,
    entry_rec_len: usize,
}

impl InodeInner {
    pub fn new(desc: Dirty<InodeDesc>, weak_self: Weak<Inode>, fs: Weak<Ext2>) -> Self {
        Self {
            desc,
            is_freed: false,
            weak_self,
            fs,
        }
    }

    pub fn create(&self, _name: &str, _type_: InodeType, _perm: FilePerm) -> Result<Arc<Inode>> {
        return_errno!(Errno::ENOSYS);
    }

    pub fn write_at(&self, _offset: usize, _data: &[u8]) -> Result<usize> {
        return_errno!(Errno::ENOSYS);
    }

    /// Finds a directory entry by name and returns its inode number.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:342 (ext2_find_entry)
    pub(super) fn find_entry(&self, name: &str) -> Result<u32> {
        if self.desc.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let fs = self.fs.upgrade().ok_or_else(|| Error::new(Errno::EIO))?;
        let sb = fs.super_block();
        let block_size = fs.block_size();
        let size = self.desc.size;
        let max_inumber = sb.total_inodes();
        let max_blocks = (self.desc.blocks as u64) >> 3;
        let mut block_idx = 0usize;

        while (block_idx as u64).saturating_mul(block_size as u64) < size {
            if (block_idx as u64) > max_blocks {
                return_errno!(Errno::ENOENT);
            }
            let block_offset = (block_idx as u64).saturating_mul(block_size as u64);
            let remain = size.saturating_sub(block_offset);
            let limit = (remain.min(block_size as u64)) as usize;
            if limit == 0 {
                break;
            }

            let bid = self
                .get_block(block_idx as u32)?
                .ok_or_else(|| Error::new(Errno::EIO))?;
            let mut buf = vec![0u8; block_size];
            if fs
                .block_device()
                .read_bytes(bid.to_offset(), &mut buf)
                .is_err()
            {
                return_errno!(Errno::EIO);
            }

            let mut iter = DirEntryIter::new(&buf, limit, max_inumber)?;
            while let Some(entry) = iter.next_entry()? {
                if entry.inode == 0 {
                    continue;
                }
                if entry.name_len as usize != name.len() {
                    continue;
                }
                let entry_name = entry.name.as_bytes();
                if entry_name.len() == name.len() && entry_name == name.as_bytes() {
                    return Ok(entry.inode);
                }
            }

            block_idx += 1;
        }

        return_errno!(Errno::ENOENT);
    }

    /// Reads directory entries starting at byte offset and feeds visitor.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:257 (ext2_readdir)
    pub(super) fn readdir_at(
        &self,
        offset: usize,
        visitor: &mut dyn DirentVisitor,
    ) -> Result<usize> {
        if self.desc.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let size = self.desc.size as usize;
        let min_rec_len = DirEntry::dir_rec_len(1) as usize;
        if size < min_rec_len || offset > size - min_rec_len {
            return Ok(0);
        }

        let fs = self.fs.upgrade().ok_or_else(|| Error::new(Errno::EIO))?;
        let sb = fs.super_block();
        let block_size = fs.block_size();
        let max_inumber = sb.total_inodes();

        let start_block = offset / block_size;
        let mut current_offset = offset;
        let mut advanced = 0usize;

        let total_blocks = (size + block_size - 1) / block_size;
        for block_idx in start_block..total_blocks {
            let block_offset = block_idx.saturating_mul(block_size);
            if block_offset >= size {
                break;
            }
            let remain = size.saturating_sub(block_offset);
            let limit = remain.min(block_size);
            if limit == 0 {
                break;
            }

            let bid = self
                .get_block(block_idx as u32)?
                .ok_or_else(|| Error::new(Errno::EIO))?;
            let mut buf = vec![0u8; block_size];
            if fs
                .block_device()
                .read_bytes(bid.to_offset(), &mut buf)
                .is_err()
            {
                return_errno!(Errno::EIO);
            }

            let mut iter = DirEntryIter::new(&buf, limit, max_inumber)?;
            let mut inner_off = 0usize;
            while let Some(entry) = iter.next_entry()? {
                let entry_offset = block_offset.saturating_add(inner_off);
                let next_offset = entry_offset.saturating_add(entry.rec_len as usize);

                if next_offset <= current_offset {
                    inner_off = next_offset.saturating_sub(block_offset);
                    continue;
                }
                if entry_offset < current_offset {
                    current_offset = next_offset;
                    inner_off = next_offset.saturating_sub(block_offset);
                    continue;
                }

                if entry.inode != 0 {
                    let dtype = DirEntryFileType::from(entry.file_type);
                    let inode_type = InodeType::from(dtype);
                    if visitor
                        .visit(
                            entry.name.as_str()?,
                            entry.inode as u64,
                            inode_type,
                            entry_offset,
                        )
                        .is_err()
                    {
                        advanced = current_offset.saturating_sub(offset);
                        return Ok(advanced);
                    }
                }

                current_offset = next_offset;
                inner_off = next_offset.saturating_sub(block_offset);
            }

            advanced = current_offset.saturating_sub(offset);
        }

        Ok(advanced)
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

        let mut bid = self.desc.block_ptrs[path.offsets[0] as usize];
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
            let offset_bytes = (path.offsets[level] as usize).saturating_mul(size_of::<u32>());

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

    /// Adds a new directory entry to this directory inode.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:476 (ext2_add_link)
    pub(super) fn add_entry(
        &mut self,
        name: &str,
        ino: u32,
        file_type: DirEntryFileType,
    ) -> Result<()> {
        if self.desc.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > u8::MAX as usize {
            return_errno!(Errno::EINVAL);
        }

        let fs = self.fs.upgrade().ok_or_else(|| Error::new(Errno::EIO))?;
        let max_inumber = fs.super_block().total_inodes();
        if ino == 0 || ino > max_inumber {
            return_errno!(Errno::EINVAL);
        }

        let chunk_size = fs.block_size();
        let reclen = DirEntry::dir_rec_len(name_bytes.len()) as usize;
        if reclen > chunk_size {
            return_errno!(Errno::ENOSPC);
        }

        let size = self.desc.size as usize;
        let data_blocks = size.div_ceil(chunk_size);

        // Candidate insertion slot selected during the ext2_add_link-style scan.
        struct InsertSlot {
            // Physical block that will be rewritten.
            block_bid: Bid,
            // Full block buffer containing the candidate slot.
            block_buf: Vec<u8>,
            // Byte offset of the candidate dirent slot within `block_buf`.
            slot_offset: usize,
            // Current rec_len of the candidate slot.
            slot_rec_len: usize,
            // Minimal occupied length of the existing entry head.
            used_rec_len: usize,
            // True when we insert by splitting an occupied entry.
            split_used_entry: bool,
            // True when the slot comes from a newly allocated directory block.
            from_new_block: bool,
        }

        let mut selected: Option<InsertSlot> = None;
        // SPEC: scan in ascending logical block order and include one growth slot.
        for block_idx in 0..=data_blocks {
            if block_idx == data_blocks {
                // No reusable slot found in existing blocks: grow directory by one block.
                let allocated = fs.alloc_blocks(1)?;
                if allocated.end != allocated.start.saturating_add(1) {
                    return_errno!(Errno::EIO);
                }

                if let Err(err) = self.link_new_data_block(block_idx as u32, allocated.start) {
                    let _ = fs.free_blocks(allocated.start, 1);
                    return Err(err);
                }

                let mut buf = vec![0u8; chunk_size];
                Self::write_dir_entry_bytes(&mut buf, 0, 0, chunk_size as u16, b"", 0)?;
                selected = Some(InsertSlot {
                    block_bid: Bid::new(allocated.start as u64),
                    block_buf: buf,
                    slot_offset: 0,
                    slot_rec_len: chunk_size,
                    used_rec_len: 0,
                    split_used_entry: false,
                    from_new_block: true,
                });
                break;
            }

            let bid = self
                .get_block(block_idx as u32)?
                .ok_or_else(|| Error::new(Errno::EIO))?;

            let mut buf = vec![0u8; chunk_size];
            if fs.block_device().read_bytes(bid.to_offset(), &mut buf).is_err() {
                return_errno!(Errno::EIO);
            }

            let block_offset = block_idx.saturating_mul(chunk_size);
            let limit = size.saturating_sub(block_offset).min(chunk_size);
            if limit == 0 {
                continue;
            }

            let entries = Self::collect_dir_entries_with_offsets(&buf, limit, max_inumber)?;
            for (entry_offset, entry) in entries {
                let rec_len = entry.rec_len as usize;
                let used_len = DirEntry::dir_rec_len(entry.name_len as usize) as usize;

                if entry.inode != 0
                    && entry.name_len as usize == name_bytes.len()
                    && entry.name.as_bytes() == name_bytes
                {
                    return_errno!(Errno::EEXIST);
                }

                if (entry.inode == 0 && rec_len >= reclen)
                    || (entry.inode != 0 && rec_len >= used_len.saturating_add(reclen))
                {
                    // Reuse free slot or split an occupied slot with enough tail space.
                    selected = Some(InsertSlot {
                        block_bid: bid,
                        block_buf: buf,
                        slot_offset: entry_offset,
                        slot_rec_len: rec_len,
                        used_rec_len: used_len,
                        split_used_entry: entry.inode != 0,
                        from_new_block: false,
                    });
                    break;
                }
            }

            if selected.is_some() {
                break;
            }
        }

        let Some(InsertSlot {
            block_bid,
            mut block_buf,
            slot_offset,
            slot_rec_len,
            used_rec_len,
            split_used_entry,
            from_new_block,
        }) = selected
        else {
            return_errno!(Errno::ENOSPC);
        };

        let (new_offset, new_rec_len) = if split_used_entry {
            if used_rec_len < DirEntry::dir_rec_len(1) as usize || used_rec_len >= slot_rec_len {
                return_errno!(Errno::EIO);
            }
            Self::write_rec_len(&mut block_buf, slot_offset, used_rec_len as u16)?;
            (slot_offset + used_rec_len, slot_rec_len - used_rec_len)
        } else {
            (slot_offset, slot_rec_len)
        };

        Self::write_dir_entry_bytes(
            &mut block_buf,
            new_offset,
            ino,
            new_rec_len as u16,
            name_bytes,
            file_type as u8,
        )?;

        if fs
            .block_device()
            .write_bytes(block_bid.to_offset(), &block_buf)
            .is_err()
        {
            // SPEC: cleanup newly allocated data block if writing the new chunk fails.
            if from_new_block {
                let _ = fs.free_blocks(block_bid.to_raw() as u32, 1);
            }
            return_errno!(Errno::EIO);
        }

        if from_new_block {
            let sectors_per_block = (chunk_size / SECTOR_SIZE) as u32;
            self.desc.size = self
                .desc
                .size
                .checked_add(chunk_size as u64)
                .ok_or_else(|| Error::new(Errno::EIO))?;
            self.desc.blocks = self
                .desc
                .blocks
                .checked_add(sectors_per_block)
                .ok_or_else(|| Error::new(Errno::EIO))?;
        }


        self.update_dir_timestamps_and_flags()?;
        self.persist_inode_and_sync(&fs)?;
        Ok(())
    }

    /// Deletes a directory entry by name.
    ///
    /// Linux: /root/linux/fs/ext2/namei.c:272 (ext2_unlink)
    pub(super) fn delete_entry(&mut self, name: &str) -> Result<()> {
        if self.desc.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > u8::MAX as usize {
            return_errno!(Errno::EINVAL);
        }

        let fs = self.fs.upgrade().ok_or_else(|| Error::new(Errno::EIO))?;
        let max_inumber = fs.super_block().total_inodes();
        let chunk_size = fs.block_size();
        let size = self.desc.size as usize;

        // Linux split: ext2_find_entry() locates, ext2_delete_entry() mutates one folio/chunk.
        let Some(mut target) =
            self.find_entry_slot(name_bytes, &fs, max_inumber, chunk_size, size)?
        else {
            return_errno!(Errno::EIO);
        };

        Self::delete_entry_in_block(
            &mut target.block_buf,
            target.limit,
            chunk_size,
            target.entry_offset,
            target.entry_rec_len,
        )?;

        if fs
            .block_device()
            .write_bytes(target.block_bid.to_offset(), &target.block_buf)
            .is_err()
        {
            return_errno!(Errno::EIO);
        }

        self.update_dir_timestamps_and_flags()?;
        self.persist_inode_and_sync(&fs)?;
        Ok(())
    }

    fn link_new_data_block(&mut self, iblock: u32, new_bid: u32) -> Result<()> {
        // TODO:
        // Current mutation path supports direct block growth only.
        if iblock >= 12 {
            return_errno!(Errno::ENOSPC);
        }

        let slot = &mut self.desc.block_ptrs[iblock as usize];
        if *slot != 0 {
            return_errno!(Errno::EIO);
        }
        *slot = new_bid;
        Ok(())
    }

    fn collect_dir_entries_with_offsets(
        buf: &[u8],
        limit: usize,
        max_inumber: u32,
    ) -> Result<Vec<(usize, DirEntry)>> {
        let mut iter = DirEntryIter::new(buf, limit, max_inumber)?;
        let mut entries = Vec::new();
        let mut entry_offset = 0usize;

        while let Some(entry) = iter.next_entry()? {
            let rec_len = entry.rec_len as usize;
            entries.push((entry_offset, entry));
            entry_offset = entry_offset.saturating_add(rec_len);
        }

        Ok(entries)
    }

    fn find_entry_slot(
        &self,
        name_bytes: &[u8],
        fs: &Ext2,
        max_inumber: u32,
        chunk_size: usize,
        size: usize,
    ) -> Result<Option<DeleteTarget>> {
        let data_blocks = size.div_ceil(chunk_size);

        for block_idx in 0..data_blocks {
            let block_bid = self
                .get_block(block_idx as u32)?
                .ok_or_else(|| Error::new(Errno::EIO))?;

            let mut block_buf = vec![0u8; chunk_size];
            if fs
                .block_device()
                .read_bytes(block_bid.to_offset(), &mut block_buf)
                .is_err()
            {
                return_errno!(Errno::EIO);
            }

            let block_offset = block_idx.saturating_mul(chunk_size);
            let limit = size.saturating_sub(block_offset).min(chunk_size);
            if limit == 0 {
                continue;
            }

            let entries = Self::collect_dir_entries_with_offsets(&block_buf, limit, max_inumber)?;
            for (entry_offset, entry) in entries {
                if entry.inode == 0 {
                    continue;
                }
                if entry.name_len as usize != name_bytes.len() {
                    continue;
                }
                if entry.name.as_bytes() != name_bytes {
                    continue;
                }

                return Ok(Some(DeleteTarget {
                    block_bid,
                    block_buf,
                    limit,
                    entry_offset,
                    entry_rec_len: entry.rec_len as usize,
                }));
            }
        }

        Ok(None)
    }

    fn delete_entry_in_block(
        block_buf: &mut [u8],
        limit: usize,
        chunk_size: usize,
        entry_offset: usize,
        entry_rec_len: usize,
    ) -> Result<()> {
        // `to` is the end offset of the entry being removed.
        let to = entry_offset.saturating_add(entry_rec_len);
        if entry_rec_len == 0 || to > limit {
            return_errno!(Errno::EIO);
        }

        // TODO: Maybe we can simplify the mask logic here.
        // Linux ext2_delete_entry aligns `from` to the start of the chunk that
        // contains `entry_offset` via: from &= ~(chunk_size - 1).
        // For power-of-two chunk sizes, this mask clears low offset bits and keeps
        // the chunk base. In our block-buffer path, `entry_offset` is block-local,
        // so this usually becomes 0, but we keep the same alignment logic.
        let chunk_mask = !(chunk_size.saturating_sub(1));
        let mut from = entry_offset & chunk_mask;
        // Walk from chunk base to target entry to find its previous dirent.
        let mut de_offset = from;
        let mut prev_offset = None;

        while de_offset < entry_offset {
            if de_offset.saturating_add(size_of::<RawDirEntry>()) > limit {
                return_errno!(Errno::EIO);
            }

            let rec_len = u16::from_le_bytes([block_buf[de_offset + 4], block_buf[de_offset + 5]]);
            if rec_len == 0 {
                return_errno!(Errno::EIO);
            }

            let next = de_offset.saturating_add(rec_len as usize);
            if next > limit {
                return_errno!(Errno::EIO);
            }

            prev_offset = Some(de_offset);
            de_offset = next;
        }

        // If traversal does not land exactly on the target entry, layout is corrupt.
        if de_offset != entry_offset {
            return_errno!(Errno::EIO);
        }

        if let Some(prev) = prev_offset {
            // Merge the removed entry range into the previous entry by extending
            // previous rec_len from `prev` to `to`, matching Linux behavior.
            from = prev;
            let merged_len = to.saturating_sub(from);
            Self::write_rec_len(block_buf, prev, merged_len as u16)?;
        }

        // Mark removed entry as unused.
        Self::write_inode_number(block_buf, entry_offset, 0)?;
        Ok(())
    }

    fn write_dir_entry_bytes(
        buf: &mut [u8],
        offset: usize,
        inode: u32,
        rec_len: u16,
        name: &[u8],
        file_type: u8,
    ) -> Result<()> {
        let header_len = size_of::<RawDirEntry>();
        if name.len() > u8::MAX as usize {
            return_errno!(Errno::EIO);
        }

        let rec_len_usize = rec_len as usize;
        if rec_len_usize < DirEntry::dir_rec_len(name.len()) as usize {
            return_errno!(Errno::EIO);
        }
        if rec_len_usize & 3 != 0 {
            return_errno!(Errno::EIO);
        }
        if offset.saturating_add(rec_len_usize) > buf.len() {
            return_errno!(Errno::EIO);
        }

        let name_start = offset + header_len;
        let name_end = name_start.saturating_add(name.len());
        if name_end > offset.saturating_add(rec_len_usize) {
            return_errno!(Errno::EIO);
        }

        buf[offset..offset + 4].copy_from_slice(&inode.to_le_bytes());
        buf[offset + 4..offset + 6].copy_from_slice(&rec_len.to_le_bytes());
        buf[offset + 6] = name.len() as u8;
        buf[offset + 7] = file_type;
        buf[name_start..name_end].copy_from_slice(name);
        Ok(())
    }

    fn write_rec_len(buf: &mut [u8], offset: usize, rec_len: u16) -> Result<()> {
        if rec_len == 0 || rec_len & 3 != 0 {
            return_errno!(Errno::EIO);
        }
        if offset.saturating_add(size_of::<RawDirEntry>()) > buf.len() {
            return_errno!(Errno::EIO);
        }
        if offset.saturating_add(rec_len as usize) > buf.len() {
            return_errno!(Errno::EIO);
        }
        buf[offset + 4..offset + 6].copy_from_slice(&rec_len.to_le_bytes());
        Ok(())
    }

    fn write_inode_number(buf: &mut [u8], offset: usize, inode: u32) -> Result<()> {
        if offset.saturating_add(size_of::<RawDirEntry>()) > buf.len() {
            return_errno!(Errno::EIO);
        }
        buf[offset..offset + 4].copy_from_slice(&inode.to_le_bytes());
        Ok(())
    }

    fn update_dir_timestamps_and_flags(&mut self) -> Result<()> {
        // TODO: update the timestamp
        // if crate::time::START_TIME.get().is_none() {
        //     return_errno!(Errno::EIO);
        // }

        // let now = crate::time::SystemTime::now()
        //     .duration_since(&crate::time::SystemTime::UNIX_EPOCH)
        //     .map(UnixTime::from)
        //     .map_err(|_| Error::new(Errno::EIO))?;
        // self.desc.ctime = now;
        // self.desc.mtime = now;
        self.desc.flags.remove(FileFlags::INDEX_DIR);
        Ok(())
    }

    fn persist_inode_and_sync(&self, fs: &Ext2) -> Result<()> {
        let inode = self.weak_self.upgrade().ok_or_else(|| Error::new(Errno::EIO))?;
        let raw = RawInode::from(&*self.desc);
        fs.write_inode_desc(inode.ino, &raw)?;
        fs.sync_metadata()?;
        Ok(())
    }
}

impl Inode {}

/// Directory entry type mapping (ext2 file_type field).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DirEntryFileType {
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

///TODO: Refactor this with a more rusty approach (e.g. enum).
/// Block path offsets for direct/indirect traversal.
#[derive(Clone, Copy, Debug)]
pub(super) struct BlockPath {
    pub depth: usize,
    pub offsets: [u32; 4],
    pub boundary: u32,
}

bitflags! {
    pub struct FileFlags: u32 {
        /// Secure deletion.
        const SECURE_DEL = 1 << 0;
        /// Undelete.
        const UNDELETE = 1 << 1;
        /// Compress file.
        const COMPRESS = 1 << 2;
        /// Synchronous updates.
        const SYNC_UPDATE = 1 << 3;
        /// Immutable file.
        const IMMUTABLE = 1 << 4;
        /// Append only.
        const APPEND_ONLY = 1 << 5;
        /// Do not dump file.
        const NO_DUMP = 1 << 6;
        /// Do not update atime.
        const NO_ATIME = 1 << 7;
        /// Dirty.
        const DIRTY = 1 << 8;
        /// One or more compressed clusters.
        const COMPRESS_BLK = 1 << 9;
        /// Do not compress.
        const NO_COMPRESS = 1 << 10;
        /// Encrypted file.
        const ENCRYPT = 1 << 11;
        /// Hash-indexed directory.
        const INDEX_DIR = 1 << 12;
        /// AFS directory.
        const IMAGIC = 1 << 13;
        /// Journal file data.
        const JOURNAL_DATA = 1 << 14;
        /// File tail should not be merged.
        const NO_TAIL = 1 << 15;
        /// Dirsync behaviour (directories only).
        const DIR_SYNC = 1 << 16;
        /// Top of directory hierarchies.
        const TOP_DIR = 1 << 17;
        /// Reserved for ext2 lib.
        const RESERVED = 1 << 31;
    }
}

/// In-memory inode descriptor (raw on-disk view).
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeDesc {
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
    flags: FileFlags,
    file_acl: u32,
    block_ptrs: [u32; 15],
}

impl InodeDesc {
    pub fn type_(&self) -> InodeType {
        self.type_
    }
}

impl TryFrom<&RawInode> for InodeDesc {
    type Error = Error;
    fn try_from(raw: &RawInode) -> Result<Self> {
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

        let mut size = raw.size_lo as u64;
        if type_ == InodeType::File {
            size |= (raw.size_high as u64) << 32;
        }
        if size > i64::MAX as u64 {
            return_errno!(Errno::EUCLEAN);
        }

        let file_acl = raw.file_acl;

        let block_ptrs = raw.block;

        let flags = FileFlags::from_bits(raw.flags).ok_or_else(|| Error::new(Errno::EIO))?;

        Ok(InodeDesc {
            type_,
            perm,
            uid,
            gid,
            size,
            atime,
            ctime,
            mtime,
            dtime: UnixTime::from(Duration::from_secs(raw.dtime as u64)),
            links_count: raw.links_count,
            blocks,
            flags,
            file_acl,
            block_ptrs,
        })
    }
}

impl From<&InodeDesc> for RawInode {
    fn from(desc: &InodeDesc) -> Self {
        let mode = desc.perm.0;
        let uid = desc.uid as u16;
        let gid = desc.gid as u16;
        let uid_high = (desc.uid >> 16) as u16;
        let gid_high = (desc.gid >> 16) as u16;

        let atime: Duration = desc.atime.into();
        let ctime: Duration = desc.ctime.into();
        let mtime: Duration = desc.mtime.into();
        let dtime: Duration = desc.dtime.into();

        let (size_lo, size_high) = if desc.type_ == InodeType::File {
            (desc.size as u32, (desc.size >> 32) as u32)
        } else {
            (desc.size as u32, 0)
        };

        Self {
            mode,
            uid,
            size_lo,
            atime: atime.as_secs() as u32,
            ctime: ctime.as_secs() as u32,
            mtime: mtime.as_secs() as u32,
            dtime: dtime.as_secs() as u32,
            gid,
            links_count: desc.links_count,
            blocks: desc.blocks,
            flags: desc.flags.bits(),
            osd1: 0,
            block: desc.block_ptrs,
            generation: 0,
            file_acl: desc.file_acl,
            size_high,
            faddr: 0,
            frag: 0,
            fsize: 0,
            pad1: 0,
            uid_high,
            gid_high,
            reserved2: 0,
        }
    }
}

/// On-disk inode structure (128 bytes for GOOD_OLD_REV).
///
/// Linux: /root/linux/fs/ext2/ext2.h:290 (struct ext2_inode)
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct RawInode {
    pub mode: u16,        // i_mode
    pub uid: u16,         // i_uid (low 16 bits)
    pub size_lo: u32,     // i_size
    pub atime: u32,       // i_atime
    pub ctime: u32,       // i_ctime
    pub mtime: u32,       // i_mtime
    pub dtime: u32,       // i_dtime
    pub gid: u16,         // i_gid (low 16 bits)
    pub links_count: u16, // i_links_count
    pub blocks: u32,      // i_blocks (512-byte sectors)
    pub flags: u32,       // i_flags
    pub osd1: u32,        // osd1.linux1.l_i_reserved1
    pub block: [u32; 15], // i_block
    pub generation: u32,  // i_generation
    pub file_acl: u32,    // i_file_acl
    pub size_high: u32,   // i_dir_acl (size high)
    pub faddr: u32,       // i_faddr
    pub frag: u8,         // osd2.linux2.l_i_frag
    pub fsize: u8,        // osd2.linux2.l_i_fsize
    pub pad1: u16,        // osd2.linux2.i_pad1
    pub uid_high: u16,    // osd2.linux2.l_i_uid_high
    pub gid_high: u16,    // osd2.linux2.l_i_gid_high
    pub reserved2: u32,   // osd2.linux2.l_i_reserved2
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

#[cfg(ktest)]
mod test {
    use core::{mem::size_of, time::Duration};

    use ostd::{mm::VmIo, prelude::ktest};
    use crate::prelude::*;

    use super::*;
    use crate::fs::ext2::{
        SuperBlock,
        block_group::RawGroupDesc,
        test::{ErrorBioDisk, Ext2MemoryDisk, make_valid_group_desc, make_valid_raw_super_block},
    };

    fn make_raw_inode(mode: u16) -> RawInode {
        RawInode {
            mode,
            uid: 0,
            size_lo: 0,
            atime: 0,
            ctime: 0,
            mtime: 0,
            dtime: 0,
            gid: 0,
            links_count: 1,
            blocks: 0,
            flags: 0,
            osd1: 0,
            block: [0; 15],
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

    fn prepare_disk(
        groups: u32,
        nblocks: usize,
    ) -> (Arc<Ext2MemoryDisk>, SuperBlock, Vec<RawGroupDesc>) {
        let raw_sb = make_valid_raw_super_block(groups);
        let sb = SuperBlock::try_from(raw_sb).unwrap();
        let descs = (0..sb.block_groups_count() as usize)
            .map(|idx| make_valid_group_desc(&sb, idx))
            .collect::<Vec<_>>();

        let disk = Arc::new(Ext2MemoryDisk::new(nblocks));
        disk.write_super_block(&raw_sb);
        disk.write_group_desc_table(&sb, &descs);

        (disk, sb, descs)
    }

    fn make_inode_inner(fs: Weak<Ext2>, block_ptrs: [u32; 15]) -> InodeInner {
        let mut raw = make_raw_inode(0o100644);
        raw.block = block_ptrs;
        let desc = InodeDesc::try_from(&raw).unwrap();
        InodeInner::new(Dirty::new(desc), Weak::new(), fs)
    }

    fn make_dir_inode_inner(fs: Weak<Ext2>, size: usize, blocks: u32, block_ptrs: [u32; 15]) -> InodeInner {
        let mut raw = make_raw_inode(0o040755);
        raw.size_lo = size as u32;
        raw.blocks = blocks;
        raw.block = block_ptrs;
        let desc = InodeDesc::try_from(&raw).unwrap();
        InodeInner::new(Dirty::new(desc), Weak::new(), fs)
    }

    fn write_dir_entry(
        buf: &mut [u8],
        offset: usize,
        inode: u32,
        rec_len: u16,
        name: &[u8],
        file_type: u8,
    ) {
        let header_len = size_of::<RawDirEntry>();
        assert!(offset + rec_len as usize <= buf.len());
        assert!(name.len() <= (rec_len as usize).saturating_sub(header_len));

        buf[offset..offset + 4].copy_from_slice(&inode.to_le_bytes());
        buf[offset + 4..offset + 6].copy_from_slice(&rec_len.to_le_bytes());
        buf[offset + 6] = name.len() as u8;
        buf[offset + 7] = file_type;
        buf[offset + header_len..offset + header_len + name.len()].copy_from_slice(name);
    }

    #[derive(Default)]
    struct CollectDirentVisitor {
        entries: Vec<(String, u64, InodeType, usize)>,
    }

    impl DirentVisitor for CollectDirentVisitor {
        fn visit(&mut self, name: &str, ino: u64, type_: InodeType, offset: usize) -> Result<()> {
            self.entries.push((name.to_string(), ino, type_, offset));
            Ok(())
        }
    }

    struct StopAfterVisitor {
        allow_count: usize,
        seen: usize,
    }

    impl StopAfterVisitor {
        fn new(allow_count: usize) -> Self {
            Self {
                allow_count,
                seen: 0,
            }
        }
    }

    impl DirentVisitor for StopAfterVisitor {
        fn visit(&mut self, _name: &str, _ino: u64, _type_: InodeType, _offset: usize) -> Result<()> {
            if self.seen >= self.allow_count {
                return_errno!(Errno::EINTR);
            }
            self.seen += 1;
            Ok(())
        }
    }

    // Writes one u32 pointer into an indirect block slot.
    fn write_indirect_ptr(disk: &Ext2MemoryDisk, bid: u32, index: u32, next: u32) {
        let offset = Bid::new(bid as u64).to_offset() + (index as usize) * size_of::<u32>();
        disk.segment().write_val(offset, &next).unwrap();
    }

    fn set_bit_lsb0(buf: &mut [u8], bit: usize) {
        let byte = bit / 8;
        let bit_in_byte = bit % 8;
        buf[byte] |= 1u8 << bit_in_byte;
    }

    fn make_live_dir_inode(
        ext2: &Arc<Ext2>,
        ino: u32,
        size: usize,
        blocks: u32,
        flags: FileFlags,
        block_ptrs: [u32; 15],
    ) -> Arc<Inode> {
        let mut raw = make_raw_inode(0o040755);
        raw.size_lo = size as u32;
        raw.blocks = blocks;
        raw.flags = flags.bits();
        raw.block = block_ptrs;
        let desc = InodeDesc::try_from(&raw).unwrap();
        Inode::new(ino, InodeType::Dir, Dirty::new(desc), 0, Arc::downgrade(ext2))
    }

    fn prepare_disk_with_block_accounting(
        nblocks: usize,
        sb_free_blocks: u32,
        group_free_blocks: u16,
    ) -> (Arc<Ext2MemoryDisk>, SuperBlock, Vec<RawGroupDesc>) {
        let mut raw_sb = make_valid_raw_super_block(1);
        raw_sb.free_blocks_count = sb_free_blocks;
        let sb = SuperBlock::try_from(raw_sb).unwrap();

        let mut descs = vec![make_valid_group_desc(&sb, 0)];
        descs[0].free_blocks_count = group_free_blocks;

        let disk = Arc::new(Ext2MemoryDisk::new(nblocks));
        disk.write_super_block(&raw_sb);
        disk.write_group_desc_table(&sb, &descs);

        (disk, sb, descs)
    }

    fn write_valid_block_bitmap(
        disk: &Ext2MemoryDisk,
        sb: &SuperBlock,
        desc: &RawGroupDesc,
        allocated_blocks: &[u32],
    ) {
        let mut bitmap_block = [0u8; BLOCK_SIZE];
        let itb = sb.itb_per_group() as usize;
        for bit in 0..(2 + itb) {
            set_bit_lsb0(&mut bitmap_block, bit);
        }

        let first = sb.group_first_block_no(0);
        for &block in allocated_blocks {
            let bit = (block.saturating_sub(first)) as usize;
            set_bit_lsb0(&mut bitmap_block, bit);
        }

        disk.segment()
            .write_bytes(Bid::new(desc.block_bitmap as u64).to_offset(), &bitmap_block)
            .unwrap();
    }

    #[ktest]
    fn inode_desc_try_from_success() {
        let mut raw = make_raw_inode(0o100644);
        raw.size_lo = 0x1122_3344;
        raw.size_high = 0x5566_7788;
        raw.uid = 0x1234;
        raw.uid_high = 0x5678;
        raw.gid = 0x4321;
        raw.gid_high = 0x8765;
        raw.blocks = 99;
        raw.block[0] = 42;
        raw.dtime = 123;

        let desc = InodeDesc::try_from(&raw).unwrap();
        assert_eq!(desc.size, 0x5566_7788_1122_3344);
        assert_eq!(desc.uid, 0x5678_1234);
        assert_eq!(desc.gid, 0x8765_4321);
        assert_eq!(desc.blocks, 99);
        assert_eq!(desc.block_ptrs[0], 42);

        let dtime: Duration = desc.dtime.into();
        assert_eq!(dtime.as_secs(), 123);

        let mut raw_dir = make_raw_inode(0o040755);
        raw_dir.size_lo = 7;
        raw_dir.size_high = u32::MAX;
        let dir_desc = InodeDesc::try_from(&raw_dir).unwrap();
        assert_eq!(dir_desc.size, 7);
    }

    #[ktest]
    fn inode_desc_try_from_error_cases() {
        let mut deleted_inode = make_raw_inode(0);
        deleted_inode.links_count = 0;
        deleted_inode.dtime = 1;
        let deleted_err = InodeDesc::try_from(&deleted_inode).unwrap_err();
        assert_eq!(deleted_err.error(), Errno::ESTALE);

        let mut invalid_flags_inode = make_raw_inode(0o100644);
        invalid_flags_inode.flags = 1 << 30;
        let flags_err = InodeDesc::try_from(&invalid_flags_inode).unwrap_err();
        assert_eq!(flags_err.error(), Errno::EIO);

        let mut size_overflow_inode = make_raw_inode(0o100644);
        size_overflow_inode.size_lo = u32::MAX;
        size_overflow_inode.size_high = u32::MAX;
        let size_err = InodeDesc::try_from(&size_overflow_inode).unwrap_err();
        assert_eq!(size_err.error(), Errno::EUCLEAN);

        let invalid_mode_inode = make_raw_inode(0o030000);
        let mode_err = InodeDesc::try_from(&invalid_mode_inode).unwrap_err();
        assert_eq!(mode_err.error(), Errno::EINVAL);
    }

    #[ktest]
    fn dir_lookup_readdir_ok() {
        let (disk, _sb, _descs) = prepare_disk(2, 256);
        let ext2 = Ext2::open(disk.clone() as Arc<dyn BlockDevice>).unwrap();
        let block_size = ext2.block_size();

        let mut block = vec![0u8; block_size];
        write_dir_entry(&mut block, 0, 2, 12, b".", 2);
        write_dir_entry(&mut block, 12, 11, 12, b"foo", 1);
        write_dir_entry(&mut block, 24, 14, 12, b"unk", 0);
        write_dir_entry(&mut block, 36, 0, 12, b"hid", 1);
        write_dir_entry(&mut block, 48, 13, 16, b"subdir", 2);

        let data_bid = 80u32;
        disk.segment()
            .write_bytes(Bid::new(data_bid as u64).to_offset(), &block)
            .unwrap();

        let mut block_ptrs = [0u32; 15];
        block_ptrs[0] = data_bid;
        let inode_inner = make_dir_inode_inner(Arc::downgrade(&ext2), 64, 8, block_ptrs);

        assert_eq!(inode_inner.find_entry("foo").unwrap(), 11);
        assert_eq!(inode_inner.find_entry("subdir").unwrap(), 13);
        let miss = inode_inner.find_entry("missing").unwrap_err();
        assert_eq!(miss.error(), Errno::ENOENT);

        let mut visitor = CollectDirentVisitor::default();
        let advanced = inode_inner.readdir_at(0, &mut visitor).unwrap();
        assert_eq!(advanced, 64);
        assert_eq!(visitor.entries.len(), 4);
        assert_eq!(visitor.entries[0], (".".to_string(), 2, InodeType::Dir, 0));
        assert_eq!(visitor.entries[1], ("foo".to_string(), 11, InodeType::File, 12));
        assert_eq!(visitor.entries[2], ("unk".to_string(), 14, InodeType::Unknown, 24));
        assert_eq!(visitor.entries[3], ("subdir".to_string(), 13, InodeType::Dir, 48));

        let mut stop_visitor = StopAfterVisitor::new(2);
        let stop_advanced = inode_inner.readdir_at(0, &mut stop_visitor).unwrap();
        assert_eq!(stop_advanced, 24);

        let mut offset_visitor = CollectDirentVisitor::default();
        let offset_advanced = inode_inner.readdir_at(5, &mut offset_visitor).unwrap();
        assert_eq!(offset_advanced, 59);
        assert_eq!(offset_visitor.entries.len(), 3);
        assert_eq!(offset_visitor.entries[0].0, "foo");
    }

    #[ktest]
    fn dir_lookup_readdir_error_cases() {
        let (disk, _sb, _descs) = prepare_disk(2, 256);
        let ext2 = Ext2::open(disk.clone() as Arc<dyn BlockDevice>).unwrap();
        let block_size = ext2.block_size();

        let mut file_ptrs = [0u32; 15];
        file_ptrs[0] = 80;
        let file_inode = make_inode_inner(Arc::downgrade(&ext2), file_ptrs);
        assert_eq!(file_inode.find_entry("foo").unwrap_err().error(), Errno::ENOTDIR);
        let mut vec_visitor = Vec::<String>::new();
        assert_eq!(file_inode.readdir_at(0, &mut vec_visitor).unwrap_err().error(), Errno::ENOTDIR);

        let hole_inode = make_dir_inode_inner(Arc::downgrade(&ext2), 12, 8, [0u32; 15]);
        assert_eq!(hole_inode.find_entry("foo").unwrap_err().error(), Errno::EIO);
        let mut vec_visitor = Vec::<String>::new();
        assert_eq!(hole_inode.readdir_at(0, &mut vec_visitor).unwrap_err().error(), Errno::EIO);

        let mut one_block = vec![0u8; block_size];
        write_dir_entry(&mut one_block, 0, 2, 12, b".", 2);
        write_dir_entry(
            &mut one_block,
            12,
            0,
            (block_size - 12) as u16,
            b"",
            0,
        );
        let data_bid = 81u32;
        disk.segment()
            .write_bytes(Bid::new(data_bid as u64).to_offset(), &one_block)
            .unwrap();

        let mut ptrs = [0u32; 15];
        ptrs[0] = data_bid;
        let limited_blocks_inode = make_dir_inode_inner(
            Arc::downgrade(&ext2),
            block_size * 2,
            0,
            ptrs,
        );
        assert_eq!(
            limited_blocks_inode.find_entry("missing").unwrap_err().error(),
            Errno::ENOENT
        );

        let tiny_dir_inode = make_dir_inode_inner(Arc::downgrade(&ext2), 11, 8, ptrs);
        let mut vec_visitor = Vec::<String>::new();
        assert_eq!(tiny_dir_inode.readdir_at(0, &mut vec_visitor).unwrap(), 0);

        let mut vec_visitor = Vec::<String>::new();
        assert_eq!(
            limited_blocks_inode
                .readdir_at(2 * block_size - 11, &mut vec_visitor)
                .unwrap(),
            0
        );

        let mut bad_block = vec![0u8; block_size];
        bad_block[0..4].copy_from_slice(&2u32.to_le_bytes());
        bad_block[4..6].copy_from_slice(&8u16.to_le_bytes());
        bad_block[6] = 1;
        bad_block[7] = 2;
        bad_block[8] = b'.';
        let bad_bid = 82u32;
        disk.segment()
            .write_bytes(Bid::new(bad_bid as u64).to_offset(), &bad_block)
            .unwrap();

        let mut bad_ptrs = [0u32; 15];
        bad_ptrs[0] = bad_bid;
        let bad_inode = make_dir_inode_inner(Arc::downgrade(&ext2), 12, 8, bad_ptrs);
        assert_eq!(bad_inode.find_entry(".").unwrap_err().error(), Errno::EIO);
        let mut vec_visitor = Vec::<String>::new();
        assert_eq!(bad_inode.readdir_at(0, &mut vec_visitor).unwrap_err().error(), Errno::EIO);
    }

    #[ktest]
    fn dir_add_delete_entry_ok() {

        let (disk, _sb, _descs) = prepare_disk(2, 256);
        let ext2 = Ext2::open(disk.clone() as Arc<dyn BlockDevice>).unwrap();
        let block_size = ext2.block_size();

        let mut block = vec![0u8; block_size];
        write_dir_entry(&mut block, 0, 2, 12, b".", 2);
        write_dir_entry(&mut block, 12, 13, (block_size - 12) as u16, b"bar", 1);

        let data_bid = 80u32;
        disk.segment()
            .write_bytes(Bid::new(data_bid as u64).to_offset(), &block)
            .unwrap();

        let mut block_ptrs = [0u32; 15];
        block_ptrs[0] = data_bid;
        let inode = make_live_dir_inode(
            &ext2,
            2,
            block_size,
            8,
            FileFlags::INDEX_DIR,
            block_ptrs,
        );

        {
            let mut inner = inode.inner.write();
            inner.add_entry("foo", 11, DirEntryFileType::File).unwrap();
            let dup = inner
                .add_entry("foo", 12, DirEntryFileType::File)
                .unwrap_err();
            assert_eq!(dup.error(), Errno::EEXIST);

            assert!(!inner.desc.flags.contains(FileFlags::INDEX_DIR));
            inner.delete_entry("foo").unwrap();
        }

        let mut post = vec![0u8; block_size];
        disk.segment()
            .read_bytes(Bid::new(data_bid as u64).to_offset(), &mut post)
            .unwrap();

        let dot = DirEntry::parse_at(&post, 0, block_size, ext2.super_block().total_inodes()).unwrap();
        assert_eq!(dot.inode, 2);
        assert_eq!(dot.rec_len, 12);

        let bar = DirEntry::parse_at(&post, 12, block_size, ext2.super_block().total_inodes()).unwrap();
        assert_eq!(bar.inode, 13);
        assert_eq!(bar.name.as_bytes(), b"bar");
        assert_eq!(bar.rec_len, (block_size - 12) as u16);

        let inner = inode.inner.read();
        assert_eq!(inner.find_entry("foo").unwrap_err().error(), Errno::ENOENT);
    }

    #[ktest]
    fn dir_add_entry_grow_by_new_block_ok() {

        let (disk, sb, descs) = prepare_disk_with_block_accounting(256, 32, 32);
        let block_size = sb.block_size();

        let ext2 = Ext2::open(disk.clone() as Arc<dyn BlockDevice>).unwrap();

        // Pick a data block inside group 0 and outside metadata area:
        // [block_bitmap, inode_bitmap, inode_table...].
        let first = sb.group_first_block_no(0);
        let last = sb.group_last_block_no(0);
        let data_bid = first
            .saturating_add(2)
            .saturating_add(sb.itb_per_group())
            .saturating_add(1);
        assert!(data_bid <= last);
        write_valid_block_bitmap(disk.as_ref(), &sb, &descs[0], &[data_bid]);

        let mut first_block = vec![0u8; block_size];
        write_dir_entry(&mut first_block, 0, 2, 12, b".", 2);
        write_dir_entry(&mut first_block, 12, 2, 12, b"..", 2);
        disk.segment()
            .write_bytes(Bid::new(data_bid as u64).to_offset(), &first_block)
            .unwrap();

        let mut block_ptrs = [0u32; 15];
        block_ptrs[0] = data_bid;
        let inode = make_live_dir_inode(
            &ext2,
            2,
            24,
            8,
            FileFlags::INDEX_DIR,
            block_ptrs,
        );

        let new_bid = {
            let mut inner = inode.inner.write();
            inner.add_entry("foo", 11, DirEntryFileType::File).unwrap();
            assert_eq!(inner.desc.size, (24 + block_size) as u64);
            assert_eq!(inner.desc.blocks, 16);
            assert_ne!(inner.desc.block_ptrs[1], 0);
            inner.desc.block_ptrs[1]
        };

        let mut new_block = vec![0u8; block_size];
        disk.segment()
            .read_bytes(Bid::new(new_bid as u64).to_offset(), &mut new_block)
            .unwrap();
        let entry = DirEntry::parse_at(&new_block, 0, block_size, ext2.super_block().total_inodes()).unwrap();
        assert_eq!(entry.inode, 11);
        assert_eq!(entry.name.as_bytes(), b"foo");
    }

    #[ktest]
    fn dir_mutation_error_cases() {
        let (disk, _sb, _descs) = prepare_disk(2, 256);
        let ext2 = Ext2::open(disk as Arc<dyn BlockDevice>).unwrap();

        let mut file_inode = make_inode_inner(Arc::downgrade(&ext2), [0u32; 15]);
        assert_eq!(
            file_inode
                .add_entry("foo", 2, DirEntryFileType::File)
                .unwrap_err()
                .error(),
            Errno::ENOTDIR
        );
        assert_eq!(file_inode.delete_entry("foo").unwrap_err().error(), Errno::ENOTDIR);

        let mut dir_ptrs = [0u32; 15];
        dir_ptrs[0] = 80;
        let mut dir_inode = make_dir_inode_inner(Arc::downgrade(&ext2), 0, 8, dir_ptrs);
        assert_eq!(
            dir_inode
                .add_entry("", 2, DirEntryFileType::File)
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
        assert_eq!(dir_inode.delete_entry("").unwrap_err().error(), Errno::EINVAL);
    }

    #[ktest]
    fn block_mapping_ok() {
        let (disk, _sb, _descs) = prepare_disk(2, 256);
        let ext2 = Ext2::open(disk.clone() as Arc<dyn BlockDevice>).unwrap();

        let ptrs = (ext2.block_size() / size_of::<u32>()) as u32;
        let ptrs_bits = ptrs.trailing_zeros();
        let double_blocks = 1u32 << (ptrs_bits * 2);

        let indirect_bid = 40u32;
        let indirect_index = 5u32;
        let mapped_bid = 77u32;
        write_indirect_ptr(disk.as_ref(), indirect_bid, indirect_index, mapped_bid);

        let double_l1_bid = 41u32;
        let double_l2_bid = 42u32;
        let double_data_bid = 78u32;
        write_indirect_ptr(disk.as_ref(), double_l1_bid, 3, double_l2_bid);
        write_indirect_ptr(disk.as_ref(), double_l2_bid, 4, double_data_bid);

        let triple_l1_bid = 43u32;
        let triple_l2_bid = 44u32;
        let triple_l3_bid = 45u32;
        let triple_data_bid = 79u32;
        write_indirect_ptr(disk.as_ref(), triple_l1_bid, 2, triple_l2_bid);
        write_indirect_ptr(disk.as_ref(), triple_l2_bid, 3, triple_l3_bid);
        write_indirect_ptr(disk.as_ref(), triple_l3_bid, 4, triple_data_bid);

        let mut block_ptrs = [0u32; 15];
        block_ptrs[0] = 11;
        block_ptrs[12] = indirect_bid;
        block_ptrs[13] = double_l1_bid;
        block_ptrs[14] = triple_l1_bid;
        let inode_inner = make_inode_inner(Arc::downgrade(&ext2), block_ptrs);

        // Cover exact transition boundaries across all mapping levels.
        let direct_path = inode_inner.block_to_path(0).unwrap();
        assert_eq!(direct_path.depth, 1);
        assert_eq!(direct_path.offsets[0], 0);
        assert_eq!(direct_path.boundary, 11);

        let direct_last_path = inode_inner.block_to_path(11).unwrap();
        assert_eq!(direct_last_path.depth, 1);
        assert_eq!(direct_last_path.offsets[0], 11);
        assert_eq!(direct_last_path.boundary, 0);

        let indirect_first_path = inode_inner.block_to_path(12).unwrap();
        assert_eq!(indirect_first_path.depth, 2);
        assert_eq!(indirect_first_path.offsets[0], 12);
        assert_eq!(indirect_first_path.offsets[1], 0);

        let indirect_path = inode_inner.block_to_path(12 + indirect_index).unwrap();
        assert_eq!(indirect_path.depth, 2);
        assert_eq!(indirect_path.offsets[0], 12);
        assert_eq!(indirect_path.offsets[1], indirect_index);

        let indirect_last_iblock = 12 + ptrs - 1;
        let indirect_last_path = inode_inner.block_to_path(indirect_last_iblock).unwrap();
        assert_eq!(indirect_last_path.depth, 2);
        assert_eq!(indirect_last_path.offsets[0], 12);
        assert_eq!(indirect_last_path.offsets[1], ptrs - 1);
        assert_eq!(indirect_last_path.boundary, 0);

        let first_double_iblock = 12 + ptrs;
        let first_double_path = inode_inner.block_to_path(first_double_iblock).unwrap();
        assert_eq!(first_double_path.depth, 3);
        assert_eq!(first_double_path.offsets[0], 13);
        assert_eq!(first_double_path.offsets[1], 0);
        assert_eq!(first_double_path.offsets[2], 0);

        let double_iblock = 12 + ptrs + (3 << ptrs_bits) + 4;
        let double_path = inode_inner.block_to_path(double_iblock).unwrap();
        assert_eq!(double_path.depth, 3);
        assert_eq!(double_path.offsets[0], 13);
        assert_eq!(double_path.offsets[1], 3);
        assert_eq!(double_path.offsets[2], 4);

        let first_triple_iblock = 12 + ptrs + double_blocks;
        let first_triple_path = inode_inner.block_to_path(first_triple_iblock).unwrap();
        assert_eq!(first_triple_path.depth, 4);
        assert_eq!(first_triple_path.offsets[0], 14);
        assert_eq!(first_triple_path.offsets[1], 0);
        assert_eq!(first_triple_path.offsets[2], 0);
        assert_eq!(first_triple_path.offsets[3], 0);

        let triple_iblock =
            12 + ptrs + double_blocks + (2 << (ptrs_bits * 2)) + (3 << ptrs_bits) + 4;
        let triple_path = inode_inner.block_to_path(triple_iblock).unwrap();
        assert_eq!(triple_path.depth, 4);
        assert_eq!(triple_path.offsets[0], 14);
        assert_eq!(triple_path.offsets[1], 2);
        assert_eq!(triple_path.offsets[2], 3);
        assert_eq!(triple_path.offsets[3], 4);

        // Verify block lookup resolves direct/indirect/double/triple chains.
        assert_eq!(inode_inner.get_block(0).unwrap(), Some(Bid::new(11)));
        assert_eq!(inode_inner.get_block(1).unwrap(), None);
        assert_eq!(
            inode_inner.get_block(12 + indirect_index).unwrap(),
            Some(Bid::new(mapped_bid as u64))
        );
        assert_eq!(
            inode_inner.get_block(double_iblock).unwrap(),
            Some(Bid::new(double_data_bid as u64))
        );
        assert_eq!(
            inode_inner.get_block(triple_iblock).unwrap(),
            Some(Bid::new(triple_data_bid as u64))
        );
    }

    #[ktest]
    fn block_mapping_error() {
        let (disk, _sb, _descs) = prepare_disk(2, 256);
        let ext2 = Ext2::open(disk.clone() as Arc<dyn BlockDevice>).unwrap();

        let ptrs = (ext2.block_size() / size_of::<u32>()) as u64;
        let direct = 12u64;
        let indirect = ptrs;
        let double_blocks = 1u64 << (ptrs.trailing_zeros() * 2);
        let triple_blocks = 1u64 << (ptrs.trailing_zeros() * 3);
        let max_iblock = direct + indirect + double_blocks + triple_blocks - 1;
        let too_big = (max_iblock + 1) as u32;

        let inode_inner = make_inode_inner(Arc::downgrade(&ext2), [0; 15]);

        // Linux semantics: max valid iblock is accepted, max + 1 is rejected.
        inode_inner.block_to_path(max_iblock as u32).unwrap();

        let too_big_err = inode_inner.block_to_path(too_big).unwrap_err();
        assert_eq!(too_big_err.error(), Errno::EINVAL);

        // Detached inode cannot upgrade fs weak ref, so mapping returns EIO.
        let detached_inode_inner = make_inode_inner(Weak::new(), [0; 15]);
        let detached_path_err = detached_inode_inner.block_to_path(0).unwrap_err();
        assert_eq!(detached_path_err.error(), Errno::EIO);

        let detached_err = detached_inode_inner.get_block(12).unwrap_err();
        assert_eq!(detached_err.error(), Errno::EIO);

        let get_too_big_err = inode_inner.get_block(too_big).unwrap_err();
        assert_eq!(get_too_big_err.error(), Errno::EINVAL);

        // Inject a deterministic read failure on the indirect block read path.
        let (io_disk_base, _io_sb, _io_descs) = prepare_disk(2, 256);
        let io_fail_offset = Bid::new(40).to_offset();
        let io_disk = Arc::new(ErrorBioDisk::with_read_error_at(
            io_disk_base,
            BioStatus::IoError,
            io_fail_offset,
        ));
        let io_ext2 = Ext2::open(io_disk as Arc<dyn BlockDevice>).unwrap();

        let mut ptrs_for_io = [0u32; 15];
        ptrs_for_io[12] = 40;
        let io_inode_inner = make_inode_inner(Arc::downgrade(&io_ext2), ptrs_for_io);
        let io_err = io_inode_inner.get_block(12).unwrap_err();
        assert_eq!(io_err.error(), Errno::EIO);

        // Any zero pointer on the branch is treated as a hole (None).
        let mut ptrs_for_indirect_hole = [0u32; 15];
        ptrs_for_indirect_hole[12] = 40;
        let indirect_hole_inode = make_inode_inner(Arc::downgrade(&ext2), ptrs_for_indirect_hole);
        assert_eq!(indirect_hole_inode.get_block(12 + 7).unwrap(), None);

        let mut ptrs_for_double_hole = [0u32; 15];
        ptrs_for_double_hole[13] = 41;
        write_indirect_ptr(disk.as_ref(), 41, 3, 0);
        let double_hole_inode = make_inode_inner(Arc::downgrade(&ext2), ptrs_for_double_hole);
        let double_hole_iblock = 12 + (ptrs as u32) + (3 << ptrs.trailing_zeros()) + 4;
        assert_eq!(
            double_hole_inode.get_block(double_hole_iblock).unwrap(),
            None
        );

        let mut ptrs_for_triple_hole = [0u32; 15];
        ptrs_for_triple_hole[14] = 43;
        write_indirect_ptr(disk.as_ref(), 43, 2, 44);
        write_indirect_ptr(disk.as_ref(), 44, 3, 0);
        let triple_hole_inode = make_inode_inner(Arc::downgrade(&ext2), ptrs_for_triple_hole);
        let triple_hole_iblock = 12
            + (ptrs as u32)
            + (double_blocks as u32)
            + (2 << (ptrs.trailing_zeros() * 2))
            + (3 << ptrs.trailing_zeros())
            + 4;
        assert_eq!(
            triple_hole_inode.get_block(triple_hole_iblock).unwrap(),
            None
        );
    }
}
