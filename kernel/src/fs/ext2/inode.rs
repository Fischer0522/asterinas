// SPDX-License-Identifier: MPL-2.0

use core::mem::size_of;

use ostd::const_assert;

use super::{
    fs::{Ext2, ROOT_INO},
    prelude::*,
    utils::now,
};
use crate::fs::ext2::dir::{DirEntry, DirEntryIter};

#[derive(Clone, Copy, Debug)]
pub struct FilePerm(u16);

impl FilePerm {
    pub fn from_bits_truncate(bits: u16) -> Self {
        Self(bits)
    }

    pub(super) fn bits(self) -> u16 {
        self.0
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

    pub(super) fn ino(&self) -> u32 {
        self.ino
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

    pub fn write_at(&self, _offset: usize, _data: &[u8]) -> Result<usize> {
        return_errno_with_message!(Errno::ENOSYS, "write not yet implemented");
    }

    /// Initializes a newly allocated directory inode with `.` and `..` entries.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:617 (ext2_make_empty)
    pub(super) fn make_empty(&mut self, parent_ino: u32) -> Result<()> {
        if self.desc.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let total_inodes = fs.super_block().total_inodes();
        if parent_ino == 0 || parent_ino > total_inodes {
            return_errno_with_message!(Errno::EINVAL, "parent inode number out of range");
        }

        let self_ino = self
            .weak_self
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "inode already dropped"))?
            .ino();

        let chunk_size = fs.block_size();
        let sectors_per_block = (chunk_size / SECTOR_SIZE) as u32;

        // SPEC: allocate exactly one data block for the first directory chunk.
        let allocated = fs.alloc_blocks(1)?;
        if allocated.end != allocated.start.saturating_add(1) {
            return_errno_with_message!(Errno::EIO, "unexpected multi-block allocation");
        }
        let new_bid = allocated.start;

        // Preserve old state for rollback.
        let old_ptr0 = self.desc.block_ptrs[0];
        let old_size = self.desc.size;
        let old_blocks = self.desc.blocks;

        if old_ptr0 != 0 {
            let _ = fs.free_blocks(new_bid, 1);
            return_errno_with_message!(Errno::EIO, "dir block pointer already occupied");
        }
        self.desc.block_ptrs[0] = new_bid;

        let mut buf = vec![0u8; chunk_size];
        // SPEC: zero-filled chunk and canonical `.`/`..` layout.
        Self::write_dir_entry_bytes(
            &mut buf,
            0,
            self_ino,
            DirEntry::dir_rec_len(1),
            b".",
            DirEntryFileType::Dir as u8,
        )?;
        let dot_len = DirEntry::dir_rec_len(1) as usize;
        let dotdot_len = (chunk_size.saturating_sub(dot_len)) as u16;
        Self::write_dir_entry_bytes(
            &mut buf,
            dot_len,
            parent_ino,
            dotdot_len,
            b"..",
            DirEntryFileType::Dir as u8,
        )?;

        if fs
            .block_device()
            .write_bytes(Bid::new(new_bid as u64).to_offset(), &buf)
            .is_err()
        {
            self.desc.block_ptrs[0] = old_ptr0;
            let _ = fs.free_blocks(new_bid, 1);
            return_errno_with_message!(Errno::EIO, "failed to write initial dir block");
        }

        self.desc.size = chunk_size as u64;
        self.desc.blocks = self
            .desc
            .blocks
            .checked_add(sectors_per_block)
            .ok_or_else(|| Error::with_message(Errno::EIO, "inode block count overflow"))?;

        if let Err(err) = self.persist_inode_and_sync(&fs) {
            // SPEC: cleanup allocation and restore pre-state if persistence failed.
            self.desc.block_ptrs[0] = old_ptr0;
            self.desc.size = old_size;
            self.desc.blocks = old_blocks;
            let _ = fs.free_blocks(new_bid, 1);
            return Err(err);
        }

        Ok(())
    }

    /// Checks whether this directory contains only `.` and `..` as live entries.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:659 (ext2_empty_dir)
    pub(super) fn empty_dir(&self) -> bool {
        if self.desc.type_ != InodeType::Dir {
            return false;
        }

        let fs = match self.fs.upgrade() {
            Some(fs) => fs,
            None => return false,
        };

        let self_ino = match self.weak_self.upgrade() {
            Some(inode) => inode.ino(),
            None => return false,
        };

        let block_size = fs.block_size();
        let size = self.desc.size as usize;
        let max_inumber = fs.super_block().total_inodes();
        let data_blocks = size.div_ceil(block_size);

        for block_idx in 0..data_blocks {
            let bid = match self.get_block(block_idx as u32) {
                Ok(Some(bid)) => bid,
                _ => return false,
            };

            let mut buf = vec![0u8; block_size];
            if fs
                .block_device()
                .read_bytes(bid.to_offset(), &mut buf)
                .is_err()
            {
                return false;
            }

            let block_offset = block_idx.saturating_mul(block_size);
            let limit = size.saturating_sub(block_offset).min(block_size);
            if limit == 0 {
                continue;
            }

            let mut iter = match DirEntryIter::new(&buf, limit, max_inumber) {
                Ok(iter) => iter,
                Err(_) => return false,
            };

            loop {
                let entry = match iter.next_entry() {
                    Ok(Some(entry)) => entry,
                    Ok(None) => break,
                    Err(_) => return false,
                };

                if entry.inode == 0 {
                    continue;
                }

                let name = entry.name.as_bytes();
                if name == b"." {
                    if entry.inode != self_ino {
                        return false;
                    }
                    continue;
                }
                if name == b".." {
                    continue;
                }
                return false;
            }
        }

        true
    }

    /// Creates a subdirectory under this directory inode.
    ///
    /// Linux: /root/linux/fs/ext2/namei.c:228 (ext2_mkdir)
    pub(super) fn mkdir(&mut self, name: &str, perm: FilePerm) -> Result<Arc<Inode>> {
        if self.desc.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty()
            || name_bytes.len() > u8::MAX as usize
            || name_bytes == b"."
            || name_bytes == b".."
        {
            return_errno!(Errno::EINVAL);
        }

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let parent_ino = self
            .weak_self
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "inode already dropped"))?
            .ino();

        // SPEC: reserve parent link for new subdir's `..`.
        self.desc.links_count = self.desc.links_count.saturating_add(1);

        let child = match fs.create_inode(parent_ino, InodeType::Dir, perm) {
            Ok(child) => child,
            Err(err) => {
                // SPEC: rollback parent link reservation on failure.
                self.desc.links_count = self.desc.links_count.saturating_sub(1);
                return Err(err);
            }
        };
        let child_ino = child.ino();

        {
            let mut child_inner = child.inner.write();
            if let Err(err) = child_inner.make_empty(parent_ino) {
                let _ = child_inner.release_dir_data_blocks_for_cleanup(&fs);
                let _ = fs.free_inode(child_ino);
                self.desc.links_count = self.desc.links_count.saturating_sub(1);
                return Err(err);
            }
        }

        if let Err(err) = self.add_entry(name, child_ino, DirEntryFileType::Dir) {
            {
                let mut child_inner = child.inner.write();
                let _ = child_inner.release_dir_data_blocks_for_cleanup(&fs);
            }
            let _ = fs.free_inode(child_ino);
            self.desc.links_count = self.desc.links_count.saturating_sub(1);
            return Err(err);
        }

        // SPEC: persist parent link count update.
        if let Err(err) = self.persist_inode_and_sync(&fs) {
            let _ = self.delete_entry(name);
            {
                let mut child_inner = child.inner.write();
                let _ = child_inner.release_dir_data_blocks_for_cleanup(&fs);
            }
            let _ = fs.free_inode(child_ino);
            self.desc.links_count = self.desc.links_count.saturating_sub(1);
            return Err(err);
        }

        Ok(child)
    }

    /// Removes an existing empty subdirectory.
    ///
    /// Linux: /root/linux/fs/ext2/namei.c:302 (ext2_rmdir)
    pub(super) fn rmdir(&mut self, name: &str) -> Result<()> {
        if self.desc.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty()
            || name_bytes.len() > u8::MAX as usize
            || name_bytes == b"."
            || name_bytes == b".."
        {
            return_errno!(Errno::EINVAL);
        }

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let child_ino = self.find_entry(name)?;
        let child = fs.read_inode(child_ino)?;

        {
            let mut child_inner = child.inner.write();
            if child_inner.desc.type_ != InodeType::Dir {
                return_errno!(Errno::ENOTDIR);
            }
            if !child_inner.empty_dir() {
                return_errno!(Errno::ENOTEMPTY);
            }

            self.delete_entry(name)?;

            child_inner.release_dir_data_blocks_for_cleanup(&fs)?;
            child_inner.desc.size = 0;
            child_inner.desc.links_count = child_inner.desc.links_count.saturating_sub(2);
            child_inner.persist_inode_and_sync(&fs)?;
        }

        self.desc.links_count = self.desc.links_count.saturating_sub(1);
        self.persist_inode_and_sync(&fs)?;

        fs.free_inode(child_ino)
    }

    /// Finds a directory entry by name and returns its inode number.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:342 (ext2_find_entry)
    pub(super) fn find_entry(&self, name: &str) -> Result<u32> {
        if self.desc.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
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
                .ok_or_else(|| Error::with_message(Errno::EIO, "dir block not mapped"))?;
            let mut buf = vec![0u8; block_size];
            if fs
                .block_device()
                .read_bytes(bid.to_offset(), &mut buf)
                .is_err()
            {
                return_errno_with_message!(Errno::EIO, "failed to read dir block");
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

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
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
                .ok_or_else(|| Error::with_message(Errno::EIO, "dir block not mapped"))?;
            let mut buf = vec![0u8; block_size];
            if fs
                .block_device()
                .read_bytes(bid.to_offset(), &mut buf)
                .is_err()
            {
                return_errno_with_message!(Errno::EIO, "failed to read dir block");
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
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let sb = fs.super_block();
        let ptrs = (sb.block_size() / size_of::<u32>()) as u32;
        if ptrs == 0 {
            return_errno_with_message!(Errno::EINVAL, "block number out of range");
        }

        let ptrs_bits = ptrs.trailing_zeros();
        let direct_blocks = 12u32;
        let indirect_blocks = ptrs;
        let double_blocks = 1u32
            .checked_shl(ptrs_bits.saturating_mul(2))
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "block path shift overflow"))?;

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
            return_errno_with_message!(Errno::EINVAL, "block number exceeds maximum");
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

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        for level in 1..path.depth {
            let mut buf = vec![0u8; BLOCK_SIZE];
            if fs
                .block_device()
                .read_bytes(Bid::new(bid as u64).to_offset(), &mut buf)
                .is_err()
            {
                return_errno_with_message!(Errno::EIO, "failed to read indirect block");
            }

            let mut reader = VmReader::from(buf.as_slice());
            let offset_bytes = (path.offsets[level] as usize).saturating_mul(size_of::<u32>());

            let next = reader.skip(offset_bytes).read_val::<u32>().map_err(|_| {
                Error::with_message(Errno::EIO, "failed to read indirect block pointer")
            })?;
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

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let max_inumber = fs.super_block().total_inodes();
        if ino == 0 || ino > max_inumber {
            return_errno!(Errno::EINVAL);
        }

        let chunk_size = fs.block_size();
        let reclen = DirEntry::dir_rec_len(name_bytes.len()) as usize;
        if reclen > chunk_size {
            return_errno_with_message!(Errno::ENOSPC, "dir entry too large for block");
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
                    return_errno_with_message!(Errno::EIO, "unexpected multi-block allocation");
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
                .ok_or_else(|| Error::with_message(Errno::EIO, "dir block not mapped"))?;

            let mut buf = vec![0u8; chunk_size];
            if fs
                .block_device()
                .read_bytes(bid.to_offset(), &mut buf)
                .is_err()
            {
                return_errno_with_message!(Errno::EIO, "failed to read dir block");
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
            return_errno_with_message!(Errno::ENOSPC, "no space for new dir entry");
        };

        let (new_offset, new_rec_len) = if split_used_entry {
            if used_rec_len < DirEntry::dir_rec_len(1) as usize || used_rec_len >= slot_rec_len {
                return_errno_with_message!(Errno::EIO, "corrupted dir entry split");
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
            return_errno_with_message!(Errno::EIO, "failed to write dir block");
        }

        if from_new_block {
            let sectors_per_block = (chunk_size / SECTOR_SIZE) as u32;
            self.desc.size = self
                .desc
                .size
                .checked_add(chunk_size as u64)
                .ok_or_else(|| Error::with_message(Errno::EIO, "inode size overflow"))?;
            self.desc.blocks = self
                .desc
                .blocks
                .checked_add(sectors_per_block)
                .ok_or_else(|| Error::with_message(Errno::EIO, "inode block count overflow"))?;
        }

        self.update_dir_timestamps_and_flags()?;
        self.persist_inode_and_sync(&fs)?;
        Ok(())
    }

    /// Rewrites an existing entry's inode/type in-place.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:450 (ext2_set_link)
    pub(super) fn set_link(
        &mut self,
        name: &str,
        new_ino: u32,
        file_type: DirEntryFileType,
        update_times: bool,
    ) -> Result<()> {
        if self.desc.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > u8::MAX as usize || name_bytes == b"." {
            return_errno!(Errno::EINVAL);
        }

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let max_inumber = fs.super_block().total_inodes();
        if new_ino < ROOT_INO || new_ino > max_inumber {
            return_errno!(Errno::EINVAL);
        }

        let chunk_size = fs.block_size();
        let size = self.desc.size as usize;
        let Some(mut target) =
            self.find_entry_slot(name_bytes, &fs, max_inumber, chunk_size, size)?
        else {
            return_errno!(Errno::ENOENT);
        };

        Self::write_inode_number(&mut target.block_buf, target.entry_offset, new_ino)?;
        if target.entry_offset.saturating_add(size_of::<RawDirEntry>()) > target.block_buf.len() {
            return_errno_with_message!(Errno::EIO, "dir entry header out of bounds");
        }
        target.block_buf[target.entry_offset + 7] = file_type as u8;

        if fs
            .block_device()
            .write_bytes(target.block_bid.to_offset(), &target.block_buf)
            .is_err()
        {
            return_errno_with_message!(Errno::EIO, "failed to write dir block");
        }

        if update_times {
            self.update_dir_timestamps_and_flags()?;
        } else {
            self.desc.flags.remove(FileFlags::INDEX_DIR);
        }
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

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let max_inumber = fs.super_block().total_inodes();
        let chunk_size = fs.block_size();
        let size = self.desc.size as usize;

        // Linux split: ext2_find_entry() locates, ext2_delete_entry() mutates one folio/chunk.
        let Some(mut target) =
            self.find_entry_slot(name_bytes, &fs, max_inumber, chunk_size, size)?
        else {
            return_errno_with_message!(Errno::EIO, "dir entry not found for delete");
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
            return_errno_with_message!(Errno::EIO, "failed to write dir block");
        }

        self.update_dir_timestamps_and_flags()?;
        self.persist_inode_and_sync(&fs)?;
        Ok(())
    }

    fn link_new_data_block(&mut self, iblock: u32, new_bid: u32) -> Result<()> {
        // TODO:
        // Current mutation path supports direct block growth only.
        if iblock >= 12 {
            return_errno_with_message!(Errno::ENOSPC, "no direct block slots available");
        }

        let slot = &mut self.desc.block_ptrs[iblock as usize];
        if *slot != 0 {
            return_errno_with_message!(Errno::EIO, "data block slot already occupied");
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
                .ok_or_else(|| Error::with_message(Errno::EIO, "dir block not mapped"))?;

            let mut block_buf = vec![0u8; chunk_size];
            if fs
                .block_device()
                .read_bytes(block_bid.to_offset(), &mut block_buf)
                .is_err()
            {
                return_errno_with_message!(Errno::EIO, "failed to read dir block");
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
            return_errno_with_message!(Errno::EIO, "invalid dir entry rec_len for delete");
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
                return_errno_with_message!(Errno::EIO, "dir entry header out of bounds");
            }

            let rec_len = u16::from_le_bytes([block_buf[de_offset + 4], block_buf[de_offset + 5]]);
            if rec_len == 0 {
                return_errno_with_message!(Errno::EIO, "zero rec_len in dir entry chain");
            }

            let next = de_offset.saturating_add(rec_len as usize);
            if next > limit {
                return_errno_with_message!(Errno::EIO, "dir entry chain exceeds block limit");
            }

            prev_offset = Some(de_offset);
            de_offset = next;
        }

        // If traversal does not land exactly on the target entry, layout is corrupt.
        if de_offset != entry_offset {
            return_errno_with_message!(Errno::EIO, "dir entry chain offset mismatch");
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
            return_errno_with_message!(Errno::EIO, "dir entry name too long");
        }

        let rec_len_usize = rec_len as usize;
        if rec_len_usize < DirEntry::dir_rec_len(name.len()) as usize {
            return_errno_with_message!(Errno::EIO, "dir entry rec_len too small");
        }
        if rec_len_usize & 3 != 0 {
            return_errno_with_message!(Errno::EIO, "dir entry rec_len not aligned");
        }
        if offset.saturating_add(rec_len_usize) > buf.len() {
            return_errno_with_message!(Errno::EIO, "dir entry exceeds buffer");
        }

        let name_start = offset + header_len;
        let name_end = name_start.saturating_add(name.len());
        if name_end > offset.saturating_add(rec_len_usize) {
            return_errno_with_message!(Errno::EIO, "dir entry name exceeds rec_len");
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
            return_errno_with_message!(Errno::EIO, "invalid rec_len value");
        }
        if offset.saturating_add(size_of::<RawDirEntry>()) > buf.len() {
            return_errno_with_message!(Errno::EIO, "dir entry header out of bounds");
        }
        if offset.saturating_add(rec_len as usize) > buf.len() {
            return_errno_with_message!(Errno::EIO, "rec_len exceeds buffer");
        }
        buf[offset + 4..offset + 6].copy_from_slice(&rec_len.to_le_bytes());
        Ok(())
    }

    fn write_inode_number(buf: &mut [u8], offset: usize, inode: u32) -> Result<()> {
        if offset.saturating_add(size_of::<RawDirEntry>()) > buf.len() {
            return_errno_with_message!(Errno::EIO, "dir entry header out of bounds");
        }
        buf[offset..offset + 4].copy_from_slice(&inode.to_le_bytes());
        Ok(())
    }

    fn release_dir_data_blocks_for_cleanup(&mut self, fs: &Ext2) -> Result<()> {
        // DIFF from Linux:
        // Linux mkdir-failure/rmdir cleanup reaches block release through
        // discard_new_inode()/iput() -> ext2_evict_inode() -> ext2_truncate_blocks().
        // Asterinas currently has no unified inode evict+truncate path, so we
        // explicitly release directory data blocks here on rollback/removal paths.
        // TODO: Move this logic into a shared truncate/evict pipeline, and make
        // free_inode trigger it instead of per-call-site cleanup.
        for bid in self.desc.block_ptrs.iter_mut().take(12) {
            if *bid == 0 {
                continue;
            }
            fs.free_blocks(*bid, 1)?;
            *bid = 0;
        }
        self.desc.size = 0;
        self.desc.blocks = 0;
        Ok(())
    }

    fn update_dir_timestamps_and_flags(&mut self) -> Result<()> {
        let current = now();
        self.desc.ctime = current;
        self.desc.mtime = current;
        self.desc.flags.remove(FileFlags::INDEX_DIR);
        Ok(())
    }

    fn persist_inode_and_sync(&self, fs: &Ext2) -> Result<()> {
        let inode = self
            .weak_self
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "inode already dropped"))?;
        let raw = RawInode::from(&*self.desc);
        fs.write_inode_desc(inode.ino, &raw)?;
        fs.sync_metadata()?;
        Ok(())
    }
}

/// Acquires read locks on two inodes in ascending ino order to prevent deadlock.
/// Returns guards in `(a, b)` order regardless of which ino is smaller.
fn read_lock_two_inodes<'a>(
    a: &'a Inode,
    b: &'a Inode,
) -> (
    RwMutexReadGuard<'a, InodeInner>,
    RwMutexReadGuard<'a, InodeInner>,
) {
    if a.ino <= b.ino {
        let ga = a.inner.read();
        let gb = b.inner.read();
        (ga, gb)
    } else {
        let gb = b.inner.read();
        let ga = a.inner.read();
        (ga, gb)
    }
}

/// Acquires write locks on two inodes in ascending ino order to prevent deadlock.
/// Returns guards in `(a, b)` order regardless of which ino is smaller.
fn write_lock_two_inodes<'a>(
    a: &'a Inode,
    b: &'a Inode,
) -> (
    RwMutexWriteGuard<'a, InodeInner>,
    RwMutexWriteGuard<'a, InodeInner>,
) {
    if a.ino <= b.ino {
        let ga = a.inner.write();
        let gb = b.inner.write();
        (ga, gb)
    } else {
        let gb = b.inner.write();
        let ga = a.inner.write();
        (ga, gb)
    }
}

/// Acquires write locks on an arbitrary number of inodes in ascending ino order.
/// Returns guards in the same order as the input slice.
fn write_lock_multiple_inodes<'a>(inodes: &[&'a Inode]) -> Vec<RwMutexWriteGuard<'a, InodeInner>> {
    use alloc::rc::Rc;
    use core::cell::RefCell;

    // Build (original_index, ino, inode_ref) and sort by ino.
    let mut indexed: Vec<(usize, u32, &'a Inode)> = inodes
        .iter()
        .enumerate()
        .map(|(i, inode)| (i, inode.ino, *inode))
        .collect();
    indexed.sort_by_key(|&(_, ino, _)| ino);

    // Acquire locks in sorted (ascending ino) order, wrapping in Rc so we can
    // later move them into the output vec in original order.
    let mut slots: Vec<Option<Rc<RefCell<Option<RwMutexWriteGuard<'a, InodeInner>>>>>> =
        vec![None; inodes.len()];
    for &(orig_idx, _, inode) in &indexed {
        let guard = inode.inner.write();
        slots[orig_idx] = Some(Rc::new(RefCell::new(Some(guard))));
    }

    // Extract guards in original input order.
    slots
        .into_iter()
        .map(|slot| {
            slot.expect("all slots filled")
                .borrow_mut()
                .take()
                .expect("guard not yet taken")
        })
        .collect()
}

impl Inode {
    /// Converts an `InodeType` to the corresponding `DirEntryFileType`.
    fn inode_type_to_dir_file_type(type_: InodeType) -> DirEntryFileType {
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

    /// Creates a child inode and directory entry under this directory.
    ///
    /// Linux: /root/linux/fs/ext2/namei.c:102 (ext2_create)
    /// Linux: /root/linux/fs/ext2/namei.c:228 (ext2_mkdir)
    pub(super) fn create(
        &self,
        name: &str,
        type_: InodeType,
        perm: FilePerm,
    ) -> Result<Arc<Inode>> {
        // SPEC: self must be a directory.
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty()
            || name_bytes.len() > u8::MAX as usize
            || name_bytes == b"."
            || name_bytes == b".."
        {
            ostd::early_println!("ext2: create: invalid name: {:?}", name_bytes);
            return_errno!(Errno::EINVAL);
        }

        // SPEC: Phase 6.3 supports File and Dir only.
        if type_ != InodeType::File && type_ != InodeType::Dir {
            ostd::early_println!("ext2: create: unsupported type: {:?}", type_);
            return_errno!(Errno::EINVAL);
        }

        // Acquire write lock on self.inner for the entire mutation.
        let mut inner = self.inner.write();

        // SPEC: check for duplicate name before allocation.
        if inner.find_entry(name).is_ok() {
            return_errno!(Errno::EEXIST);
        }

        if type_ == InodeType::Dir {
            // TODO: different from Linux, should we keep this?
            // Delegate to existing mkdir which handles the full directory
            // creation state machine (parent link reservation, make_empty,
            // add_entry, rollback).
            // Linux: ext2_mkdir
            let ret = inner.mkdir(name, perm)?;
            return Ok(ret);
        } else {
            // Linux: ext2_create → ext2_new_inode + ext2_add_nondir
            let fs = self
                .fs
                .upgrade()
                .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
            let child = fs.create_inode(self.ino, type_, perm)?;
            let child_ino = child.ino();
            let dir_ft = Self::inode_type_to_dir_file_type(type_);

            if let Err(err) = inner.add_entry(name, child_ino, dir_ft) {
                // SPEC: rollback — ext2_add_nondir failure path:
                // decrement link count and discard inode.
                let _ = fs.free_inode(child_ino);
                return Err(err);
            }

            Ok(child)
        }
    }

    /// Adds a hard link in this directory to an existing non-directory inode.
    ///
    /// Linux: /root/linux/fs/ext2/namei.c:204 (ext2_link)
    pub(super) fn link(&self, old: &Arc<Inode>, name: &str) -> Result<()> {
        // SPEC: self must be a directory.
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        // SPEC: hard links to directories are not allowed.
        if old.type_ == InodeType::Dir {
            return_errno!(Errno::EPERM);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty()
            || name_bytes.len() > u8::MAX as usize
            || name_bytes == b"."
            || name_bytes == b".."
        {
            return_errno!(Errno::EINVAL);
        }

        // SPEC: cross-filesystem link check.
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let old_fs = old
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        if !Arc::ptr_eq(&fs, &old_fs) {
            return_errno!(Errno::EINVAL);
        }

        // Lock ordering: acquire write locks by ascending inode number
        // to prevent deadlock when self and old are different inodes.
        let dir_ft = Self::inode_type_to_dir_file_type(old.type_);

        let (mut self_inner, mut old_inner) = write_lock_two_inodes(self, old);

        // SPEC: check duplicate before modifying link count.
        if self_inner.find_entry(name).is_ok() {
            return_errno!(Errno::EEXIST);
        }

        // Linux: inode_set_ctime_current + inode_inc_link_count before add_link.
        old_inner.desc.ctime = now();
        old_inner.desc.links_count = old_inner.desc.links_count.saturating_add(1);

        if let Err(err) = self_inner.add_entry(name, old.ino, dir_ft) {
            // SPEC: rollback link count on add_entry failure.
            old_inner.desc.links_count = old_inner.desc.links_count.saturating_sub(1);
            return Err(err);
        }

        // Persist old inode metadata.
        old_inner.persist_inode_and_sync(&fs)?;
        Ok(())
    }

    /// Removes a non-directory entry from this directory.
    ///
    /// Linux: /root/linux/fs/ext2/namei.c:273 (ext2_unlink)
    pub(super) fn unlink(&self, name: &str) -> Result<()> {
        // SPEC: self must be a directory.
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty()
            || name_bytes.len() > u8::MAX as usize
            || name_bytes == b"."
            || name_bytes == b".."
        {
            return_errno!(Errno::EINVAL);
        }

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;

        // Acquire self write lock to resolve and delete entry.
        let mut self_inner = self.inner.write();
        let child_ino = self_inner.find_entry(name)?;
        let child = fs.read_inode(child_ino)?;

        // SPEC: unlink rejects directories — use rmdir instead.
        if child.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        // Delete the directory entry first.
        self_inner.delete_entry(name)?;

        // Linux: inode_set_ctime_to_ts(inode, inode_get_ctime(dir))
        // then inode_dec_link_count.
        let mut child_inner = child.inner.write();
        child_inner.desc.ctime = self_inner.desc.ctime;
        child_inner.desc.links_count = child_inner.desc.links_count.saturating_sub(1);

        // SPEC: if link count reaches 0, mark deletion time and free inode.
        // DIFF from Linux: Linux defers reclamation to inode eviction/orphan.
        // Asterinas reclaims immediately since orphan pipeline is not yet integrated.
        if child_inner.desc.links_count == 0 {
            child_inner.desc.dtime = now();
            child_inner.persist_inode_and_sync(&fs)?;
            drop(child_inner);
            let _ = fs.free_inode(child_ino);
        } else {
            child_inner.persist_inode_and_sync(&fs)?;
        }

        Ok(())
    }

    /// Renames or moves an entry from this directory to `target` directory.
    ///
    /// Linux: /root/linux/fs/ext2/namei.c:318 (ext2_rename)
    pub(super) fn rename(&self, old_name: &str, target: &Arc<Inode>, new_name: &str) -> Result<()> {
        // SPEC: both self and target must be directories.
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }
        if target.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let old_bytes = old_name.as_bytes();
        let new_bytes = new_name.as_bytes();
        if old_bytes.is_empty()
            || old_bytes.len() > u8::MAX as usize
            || old_bytes == b"."
            || old_bytes == b".."
        {
            return_errno!(Errno::EISDIR);
        }
        if new_bytes.is_empty()
            || new_bytes.len() > u8::MAX as usize
            || new_bytes == b"."
            || new_bytes == b".."
        {
            return_errno!(Errno::EISDIR);
        }

        // SPEC: cross-filesystem rename check.
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let target_fs = target
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        if !Arc::ptr_eq(&fs, &target_fs) {
            return_errno!(Errno::EINVAL);
        }

        // Rename to itself is a no-op.
        if self.ino == target.ino && old_name == new_name {
            return Ok(());
        }

        // Acquire directory write locks by ascending inode number to avoid deadlock.
        let same_dir = self.ino == target.ino;

        if same_dir {
            self.rename_same_dir(old_name, new_name, &fs)
        } else {
            let (mut self_inner, mut target_inner) = write_lock_two_inodes(self, target);
            Self::rename_inner(
                &mut self_inner,
                &mut target_inner,
                self.ino,
                target.ino,
                old_name,
                new_name,
                &fs,
            )
        }
    }

    /// Rename within the same directory (single lock).
    fn rename_same_dir(&self, old_name: &str, new_name: &str, fs: &Arc<Ext2>) -> Result<()> {
        let mut inner = self.inner.write();

        let old_ino = inner.find_entry(old_name)?;
        let old_inode = fs.read_inode(old_ino)?;
        let old_is_dir = old_inode.type_ == InodeType::Dir;
        let moved_ft = Self::inode_type_to_dir_file_type(old_inode.type_);

        // Check if new_name already exists.
        let existing_ino = inner.find_entry(new_name).ok();

        if let Some(existing_ino) = existing_ino {
            let existing = fs.read_inode(existing_ino)?;
            let existing_is_dir = existing.type_ == InodeType::Dir;

            // SPEC: type compatibility check.
            if old_is_dir && !existing_is_dir {
                return_errno!(Errno::ENOTDIR);
            }
            if !old_is_dir && existing_is_dir {
                return_errno!(Errno::EISDIR);
            }

            // SPEC: replacing a directory requires it to be empty.
            if existing_is_dir {
                let existing_inner = existing.inner.write();
                if !existing_inner.empty_dir() {
                    return_errno!(Errno::ENOTEMPTY);
                }
                drop(existing_inner);
            }

            // Replace destination entry: set_link(new_name, old_ino, ..., true).
            inner.set_link(new_name, old_ino, moved_ft, true)?;

            // Update replaced inode: set ctime, decrement links.
            let mut existing_inner = existing.inner.write();
            existing_inner.desc.ctime = now();
            if old_is_dir {
                // Directory replacement: drop extra link for `..`.
                existing_inner.desc.links_count = existing_inner.desc.links_count.saturating_sub(1);
            }
            existing_inner.desc.links_count = existing_inner.desc.links_count.saturating_sub(1);

            if existing_inner.desc.links_count == 0 {
                existing_inner.desc.dtime = now();
                existing_inner.persist_inode_and_sync(fs)?;
                drop(existing_inner);
                let _ = fs.free_inode(existing_ino);
            } else {
                existing_inner.persist_inode_and_sync(fs)?;
            }
        } else {
            // No existing entry: add new entry.
            inner.add_entry(new_name, old_ino, moved_ft)?;
        }

        // Linux: inode_set_ctime_current(old_inode) + mark_inode_dirty.
        {
            let mut old_inner = old_inode.inner.write();
            old_inner.desc.ctime = now();
            old_inner.persist_inode_and_sync(fs)?;
        }

        // Delete old entry.
        inner.delete_entry(old_name)?;

        // Linux: when old_is_dir, always inode_dec_link_count(old_dir).
        // For same-dir without replacement, add_entry above implicitly
        // paired with inode_inc_link_count(new_dir) — since same dir,
        // the net effect is zero. But we must still track both sides.
        if old_is_dir {
            if existing_ino.is_none() {
                // Linux: inode_inc_link_count(new_dir) was done in the else branch
                // of the replacement check. For same dir, this is self.
                inner.desc.links_count = inner.desc.links_count.saturating_add(1);
            }
            // Linux line 397: inode_dec_link_count(old_dir) — always when old_is_dir.
            inner.desc.links_count = inner.desc.links_count.saturating_sub(1);
            inner.persist_inode_and_sync(fs)?;
        }

        Ok(())
    }

    /// Core rename logic when locks are already held.
    fn rename_inner(
        self_inner: &mut InodeInner,
        target_inner: &mut InodeInner,
        self_ino: u32,
        target_ino: u32,
        old_name: &str,
        new_name: &str,
        fs: &Arc<Ext2>,
    ) -> Result<()> {
        let old_ino = self_inner.find_entry(old_name)?;
        let old_inode = fs.read_inode(old_ino)?;
        let old_is_dir = old_inode.type_ == InodeType::Dir;
        let moved_ft = Self::inode_type_to_dir_file_type(old_inode.type_);

        // If moving a directory across parents, verify `..` is accessible.
        // Linux: ext2_dotdot check.
        if old_is_dir {
            let old_inner = old_inode.inner.write();
            // Verify `..` entry exists and points to self_ino.
            let dotdot_ino = old_inner.find_entry("..")?;
            if dotdot_ino != self_ino {
                drop(old_inner);
                return_errno_with_message!(Errno::EIO, "failed to update dotdot entry");
            }
            drop(old_inner);
        }

        // Check if new_name already exists in target.
        let existing_ino = target_inner.find_entry(new_name).ok();

        if let Some(existing_ino) = existing_ino {
            let existing = fs.read_inode(existing_ino)?;
            let existing_is_dir = existing.type_ == InodeType::Dir;

            // SPEC: type compatibility.
            if old_is_dir && !existing_is_dir {
                return_errno!(Errno::ENOTDIR);
            }
            if !old_is_dir && existing_is_dir {
                return_errno!(Errno::EISDIR);
            }

            if existing_is_dir {
                let existing_inner = existing.inner.write();
                if !existing_inner.empty_dir() {
                    return_errno!(Errno::ENOTEMPTY);
                }
                drop(existing_inner);
            }

            // Replace destination: ext2_set_link(new_dir, ..., old_inode, true).
            target_inner.set_link(new_name, old_ino, moved_ft, true)?;

            // Update replaced inode.
            let mut existing_inner = existing.inner.write();
            existing_inner.desc.ctime = now();
            if old_is_dir {
                existing_inner.desc.links_count = existing_inner.desc.links_count.saturating_sub(1);
            }
            existing_inner.desc.links_count = existing_inner.desc.links_count.saturating_sub(1);

            if existing_inner.desc.links_count == 0 {
                existing_inner.desc.dtime = now();
                existing_inner.persist_inode_and_sync(fs)?;
                drop(existing_inner);
                let _ = fs.free_inode(existing_ino);
            } else {
                existing_inner.persist_inode_and_sync(fs)?;
            }
        } else {
            // No existing entry: ext2_add_link.
            target_inner.add_entry(new_name, old_ino, moved_ft)?;
            if old_is_dir {
                // Linux: inode_inc_link_count(new_dir) for new subdir.
                target_inner.desc.links_count = target_inner.desc.links_count.saturating_add(1);
            }
        }

        // Linux: inode_set_ctime_current(old_inode) + mark_inode_dirty.
        {
            let mut old_inner = old_inode.inner.write();
            old_inner.desc.ctime = now();
            old_inner.persist_inode_and_sync(fs)?;
        }

        // Delete old entry from source directory.
        self_inner.delete_entry(old_name)?;

        // If moving a directory across parents, update `..` to point to target.
        if old_is_dir {
            // ext2_set_link(old_inode, dir_de, ..., new_dir, false)
            let mut old_inner = old_inode.inner.write();
            old_inner.set_link("..", target_ino, DirEntryFileType::Dir, false)?;
            drop(old_inner);

            // Linux: inode_dec_link_count(old_dir) — old parent loses a subdir.
            self_inner.desc.links_count = self_inner.desc.links_count.saturating_sub(1);
            self_inner.persist_inode_and_sync(fs)?;

            // If destination didn't already have the entry (no replacement),
            // target link count was already incremented above.
            // If replacement happened, the replaced dir's link decrement
            // already accounts for it.
            target_inner.persist_inode_and_sync(fs)?;
        }

        Ok(())
    }

    /// Rewrites an existing directory entry to point to a new inode.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:450 (ext2_set_link)
    ///
    /// This is an `Inode`-level wrapper that acquires the write lock
    /// and delegates to `InodeInner::set_link`.
    pub(super) fn set_link(
        &self,
        name: &str,
        new_ino: u32,
        file_type: DirEntryFileType,
        update_times: bool,
    ) -> Result<()> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }
        let mut inner = self.inner.write();
        inner.set_link(name, new_ino, file_type, update_times)
    }
}

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
    atime: Duration,
    ctime: Duration,
    mtime: Duration,
    dtime: Duration,
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
            return_errno_with_message!(Errno::ESTALE, "inode has been deleted");
        }

        // TODO: Different from Linux
        let type_ = InodeType::from_raw_mode(mode)?;

        let perm = FilePerm::from_bits_truncate(mode);

        let uid = (raw.uid as u32) | ((raw.uid_high as u32) << 16);
        let gid = (raw.gid as u32) | ((raw.gid_high as u32) << 16);

        let atime = Duration::from_secs(raw.atime as u64);
        let ctime = Duration::from_secs(raw.ctime as u64);
        let mtime = Duration::from_secs(raw.mtime as u64);

        let blocks = raw.blocks;

        let mut size = raw.size_lo as u64;
        if type_ == InodeType::File {
            size |= (raw.size_high as u64) << 32;
        }
        if size > i64::MAX as u64 {
            return_errno_with_message!(Errno::EUCLEAN, "corrupted inode on disk");
        }

        let file_acl = raw.file_acl;

        let block_ptrs = raw.block;

        let flags = FileFlags::from_bits(raw.flags)
            .ok_or_else(|| Error::with_message(Errno::EIO, "invalid inode flags"))?;

        Ok(InodeDesc {
            type_,
            perm,
            uid,
            gid,
            size,
            atime,
            ctime,
            mtime,
            dtime: Duration::from_secs(raw.dtime as u64),
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

        let atime = desc.atime;
        let ctime = desc.ctime;
        let mtime = desc.mtime;
        let dtime = desc.dtime;

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

    use super::*;
    use crate::{
        fs::ext2::{
            fs::ROOT_INO,
            testkit::{
                self, CollectDirentVisitor, ErrorBioDisk, Ext2FixtureBuilder,
                RawInodeBuilder, StopAfterVisitor, encode_dir_entry, write_indirect_ptr,
            },
        },
        prelude::*,
        time::clocks,
    };

    fn make_raw_inode(mode: u16) -> RawInode {
        RawInodeBuilder::new(mode).build()
    }

    fn make_inode_inner(fs: Weak<Ext2>, block_ptrs: [u32; 15]) -> InodeInner {
        let mut raw = make_raw_inode(0o100644);
        raw.block = block_ptrs;
        let desc = InodeDesc::try_from(&raw).unwrap();
        InodeInner::new(Dirty::new(desc), Weak::new(), fs)
    }

    fn make_dir_inode_inner(
        fs: Weak<Ext2>,
        size: usize,
        blocks: u32,
        block_ptrs: [u32; 15],
    ) -> InodeInner {
        let mut raw = make_raw_inode(0o040755);
        raw.size_lo = size as u32;
        raw.blocks = blocks;
        raw.block = block_ptrs;
        let desc = InodeDesc::try_from(&raw).unwrap();
        InodeInner::new(Dirty::new(desc), Weak::new(), fs)
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
        Inode::new(
            ino,
            InodeType::Dir,
            Dirty::new(desc),
            0,
            Arc::downgrade(ext2),
        )
    }

    #[ktest]
    fn namei_create_phase06_spec() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();

        // Linux ext2_create intent: allocate inode then publish dir entry.
        let created = root
            .create(
                "alpha",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        assert_eq!(
            root.inner.read().find_entry("alpha").unwrap(),
            created.ino()
        );
        assert_eq!(f.ext2.read_inode_desc(created.ino()).unwrap().links_count, 1);

        // Linux ext2_mkdir intent: child links=2 and parent link count +1.
        let created_dir = root
            .create("sub", InodeType::Dir, FilePerm::from_bits_truncate(0o755))
            .unwrap();
        assert_eq!(
            root.inner.read().find_entry("sub").unwrap(),
            created_dir.ino()
        );
        assert_eq!(
            f.ext2.read_inode_desc(created_dir.ino()).unwrap().links_count,
            2
        );
        assert_eq!(f.ext2.read_inode_desc(ROOT_INO).unwrap().links_count, 3);

        assert_eq!(
            root.create(".", InodeType::File, FilePerm::from_bits_truncate(0o644))
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
        assert_eq!(
            root.create(
                "alpha",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644)
            )
            .unwrap_err()
            .error(),
            Errno::EEXIST
        );
        assert_eq!(
            root.create(
                "sock",
                InodeType::Socket,
                FilePerm::from_bits_truncate(0o644)
            )
            .unwrap_err()
            .error(),
            Errno::EINVAL
        );
    }

    #[ktest]
    fn namei_link_unlink_phase06_spec() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();

        let old = root
            .create("old", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();
        let old_ino = old.ino();
        let old_links_before = f.ext2.read_inode_desc(old_ino).unwrap().links_count;

        // Linux ext2_link intent: increase nlink before publishing name.
        root.link(&old, "alias").unwrap();
        assert_eq!(root.inner.read().find_entry("alias").unwrap(), old_ino);
        assert_eq!(
            f.ext2.read_inode_desc(old_ino).unwrap().links_count,
            old_links_before + 1
        );

        let dir = root
            .create("dir", InodeType::Dir, FilePerm::from_bits_truncate(0o755))
            .unwrap();
        assert_eq!(
            root.link(&dir, "dir_hard").unwrap_err().error(),
            Errno::EPERM
        );

        // Linux ext2_unlink intent: remove name then decrement target nlink.
        root.unlink("alias").unwrap();
        assert_eq!(
            root.inner.read().find_entry("alias").unwrap_err().error(),
            Errno::ENOENT
        );
        assert_eq!(
            f.ext2.read_inode_desc(old_ino).unwrap().links_count,
            old_links_before
        );

        assert_eq!(root.unlink("dir").unwrap_err().error(), Errno::EISDIR);
        assert_eq!(root.unlink(".").unwrap_err().error(), Errno::EINVAL);

        root.unlink("old").unwrap();
        let inode_bitmap = f.read_inode_bitmap(0).unwrap();
        assert!(!testkit::bit_is_set_lsb0(
            &inode_bitmap,
            (old_ino - 1) as usize
        ));
    }

    #[ktest]
    fn namei_set_link_and_rename_phase06_spec() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();

        let src = root
            .create("src", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();
        let target = root
            .create(
                "target",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();

        let (ctime_before, mtime_before) = {
            let guard = root.inner.read();
            (guard.desc.ctime, guard.desc.mtime)
        };

        // Linux ext2_set_link with update_times=false keeps ctime/mtime unchanged.
        root.set_link("src", target.ino(), DirEntryFileType::File, false)
            .unwrap();
        assert_eq!(root.inner.read().find_entry("src").unwrap(), target.ino());
        let (ctime_after, mtime_after) = {
            let guard = root.inner.read();
            (guard.desc.ctime, guard.desc.mtime)
        };
        assert_eq!(ctime_before, ctime_after);
        assert_eq!(mtime_before, mtime_after);

        assert_eq!(
            root.set_link("missing", target.ino(), DirEntryFileType::File, false)
                .unwrap_err()
                .error(),
            Errno::ENOENT
        );
        assert_eq!(
            root.set_link("src", ROOT_INO - 1, DirEntryFileType::File, false)
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );

        // Rename to itself is a no-op success.
        root.rename("target", &root, "target").unwrap();

        let old = root
            .create("old", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();
        let new = root
            .create("new", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();
        let old_ino = old.ino();
        let replaced_ino = new.ino();

        // Linux ext2_rename replacement path: dst is overwritten and old name removed.
        root.rename("old", &root, "new").unwrap();
        {
            let guard = root.inner.read();
            assert_eq!(guard.find_entry("new").unwrap(), old_ino);
            assert_eq!(guard.find_entry("old").unwrap_err().error(), Errno::ENOENT);
        }

        let inode_bitmap = f.read_inode_bitmap(0).unwrap();
        assert!(!testkit::bit_is_set_lsb0(
            &inode_bitmap,
            (replaced_ino - 1) as usize
        ));

        let _ = src;
    }

    #[ktest]
    fn namei_cross_fs_link_and_rename_rejected() {
        clocks::init_for_ktest();

        let f_a = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root_a = f_a.ext2.read_inode(ROOT_INO).unwrap();
        let inode_a = root_a
            .create("from", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();

        let f_b = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root_b = f_b.ext2.read_inode(ROOT_INO).unwrap();

        assert_eq!(
            root_b.link(&inode_a, "x").unwrap_err().error(),
            Errno::EINVAL
        );
        assert_eq!(
            root_a.rename("from", &root_b, "to").unwrap_err().error(),
            Errno::EINVAL
        );
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
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let (disk, ext2) = (&f.disk, &f.ext2);
        let block_size = ext2.block_size();

        let mut block = vec![0u8; block_size];
        encode_dir_entry(&mut block, 0, 2, 12, b".", 2);
        encode_dir_entry(&mut block, 12, 11, 12, b"foo", 1);
        encode_dir_entry(&mut block, 24, 14, 12, b"unk", 0);
        encode_dir_entry(&mut block, 36, 0, 12, b"hid", 1);
        encode_dir_entry(&mut block, 48, 13, 16, b"subdir", 2);

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
        assert_eq!(
            visitor.entries[1],
            ("foo".to_string(), 11, InodeType::File, 12)
        );
        assert_eq!(
            visitor.entries[2],
            ("unk".to_string(), 14, InodeType::Unknown, 24)
        );
        assert_eq!(
            visitor.entries[3],
            ("subdir".to_string(), 13, InodeType::Dir, 48)
        );

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
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let (disk, ext2) = (&f.disk, &f.ext2);
        let block_size = ext2.block_size();

        let mut file_ptrs = [0u32; 15];
        file_ptrs[0] = 80;
        let file_inode = make_inode_inner(Arc::downgrade(&ext2), file_ptrs);
        assert_eq!(
            file_inode.find_entry("foo").unwrap_err().error(),
            Errno::ENOTDIR
        );
        let mut vec_visitor = Vec::<String>::new();
        assert_eq!(
            file_inode
                .readdir_at(0, &mut vec_visitor)
                .unwrap_err()
                .error(),
            Errno::ENOTDIR
        );

        let hole_inode = make_dir_inode_inner(Arc::downgrade(&ext2), 12, 8, [0u32; 15]);
        assert_eq!(
            hole_inode.find_entry("foo").unwrap_err().error(),
            Errno::EIO
        );
        let mut vec_visitor = Vec::<String>::new();
        assert_eq!(
            hole_inode
                .readdir_at(0, &mut vec_visitor)
                .unwrap_err()
                .error(),
            Errno::EIO
        );

        let mut one_block = vec![0u8; block_size];
        encode_dir_entry(&mut one_block, 0, 2, 12, b".", 2);
        encode_dir_entry(&mut one_block, 12, 0, (block_size - 12) as u16, b"", 0);
        let data_bid = 81u32;
        disk.segment()
            .write_bytes(Bid::new(data_bid as u64).to_offset(), &one_block)
            .unwrap();

        let mut ptrs = [0u32; 15];
        ptrs[0] = data_bid;
        let limited_blocks_inode =
            make_dir_inode_inner(Arc::downgrade(&ext2), block_size * 2, 0, ptrs);
        assert_eq!(
            limited_blocks_inode
                .find_entry("missing")
                .unwrap_err()
                .error(),
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
        assert_eq!(
            bad_inode
                .readdir_at(0, &mut vec_visitor)
                .unwrap_err()
                .error(),
            Errno::EIO
        );
    }

    #[ktest]
    fn dir_add_delete_entry_ok() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let (disk, ext2) = (&f.disk, &f.ext2);
        let block_size = ext2.block_size();

        let mut block = vec![0u8; block_size];
        encode_dir_entry(&mut block, 0, 2, 12, b".", 2);
        encode_dir_entry(&mut block, 12, 13, (block_size - 12) as u16, b"bar", 1);

        let data_bid = 80u32;
        disk.segment()
            .write_bytes(Bid::new(data_bid as u64).to_offset(), &block)
            .unwrap();

        let mut block_ptrs = [0u32; 15];
        block_ptrs[0] = data_bid;
        let inode = make_live_dir_inode(&ext2, 2, block_size, 8, FileFlags::INDEX_DIR, block_ptrs);

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

        let dot =
            DirEntry::parse_at(&post, 0, block_size, ext2.super_block().total_inodes()).unwrap();
        assert_eq!(dot.inode, 2);
        assert_eq!(dot.rec_len, 12);

        let bar =
            DirEntry::parse_at(&post, 12, block_size, ext2.super_block().total_inodes()).unwrap();
        assert_eq!(bar.inode, 13);
        assert_eq!(bar.name.as_bytes(), b"bar");
        assert_eq!(bar.rec_len, (block_size - 12) as u16);

        let inner = inode.inner.read();
        assert_eq!(inner.find_entry("foo").unwrap_err().error(), Errno::ENOENT);
    }

    #[ktest]
    fn dir_add_entry_grow_by_new_block_ok() {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(32, 32)
            .build()
            .unwrap();
        let block_size = f.sb.block_size();

        // Pick a data block inside group 0 and outside metadata area:
        // [block_bitmap, inode_bitmap, inode_table...].
        let first = f.sb.group_first_block_no(0);
        let last = f.sb.group_last_block_no(0);
        let data_bid = first
            .saturating_add(2)
            .saturating_add(f.sb.itb_per_group())
            .saturating_add(1);
        assert!(data_bid <= last);
        // Let allocator choose one free data block; do not pre-occupy `data_bid`.
        testkit::write_block_bitmap(f.disk.as_ref(), &f.sb, &f.descs[0], &[]);

        let mut first_block = vec![0u8; block_size];
        encode_dir_entry(&mut first_block, 0, 2, 12, b".", 2);
        encode_dir_entry(&mut first_block, 12, 2, 12, b"..", 2);
        f.disk.segment()
            .write_bytes(Bid::new(data_bid as u64).to_offset(), &first_block)
            .unwrap();

        let mut block_ptrs = [0u32; 15];
        block_ptrs[0] = data_bid;
        let inode = make_live_dir_inode(&f.ext2, 2, 24, 8, FileFlags::INDEX_DIR, block_ptrs);

        let new_bid = {
            let mut inner = inode.inner.write();
            inner.add_entry("foo", 11, DirEntryFileType::File).unwrap();
            assert_eq!(inner.desc.size, (24 + block_size) as u64);
            assert_eq!(inner.desc.blocks, 16);
            assert_ne!(inner.desc.block_ptrs[1], 0);
            inner.desc.block_ptrs[1]
        };

        let mut new_block = vec![0u8; block_size];
        f.disk.segment()
            .read_bytes(Bid::new(new_bid as u64).to_offset(), &mut new_block)
            .unwrap();
        let entry =
            DirEntry::parse_at(&new_block, 0, block_size, f.ext2.super_block().total_inodes())
                .unwrap();
        assert_eq!(entry.inode, 11);
        assert_eq!(entry.name.as_bytes(), b"foo");
    }

    #[ktest]
    fn dir_mutation_error_cases() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let ext2 = &f.ext2;

        let mut file_inode = make_inode_inner(Arc::downgrade(&ext2), [0u32; 15]);
        assert_eq!(
            file_inode
                .add_entry("foo", 2, DirEntryFileType::File)
                .unwrap_err()
                .error(),
            Errno::ENOTDIR
        );
        assert_eq!(
            file_inode.delete_entry("foo").unwrap_err().error(),
            Errno::ENOTDIR
        );

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
        assert_eq!(
            dir_inode.delete_entry("").unwrap_err().error(),
            Errno::EINVAL
        );
    }

    #[ktest]
    fn dir_make_empty_and_empty_dir_ok() {
        clocks::init_for_ktest();
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let block_size = f.ext2.block_size();

        let first = f.sb.group_first_block_no(0);
        let last = f.sb.group_last_block_no(0);
        let data_bid = first
            .saturating_add(2)
            .saturating_add(f.sb.itb_per_group())
            .saturating_add(1);
        assert!(data_bid <= last);
        testkit::write_block_bitmap(f.disk.as_ref(), &f.sb, &f.descs[0], &[data_bid]);

        let mut raw = make_raw_inode(0o040755);
        raw.links_count = 2;
        let desc = InodeDesc::try_from(&raw).unwrap();
        let inode = Inode::new(
            12,
            InodeType::Dir,
            Dirty::new(desc),
            0,
            Arc::downgrade(&f.ext2),
        );

        let allocated_bid = {
            let mut inner = inode.inner.write();
            inner.make_empty(ROOT_INO).unwrap();
            assert!(inner.empty_dir());
            assert_eq!(inner.desc.size as usize, block_size);
            assert_eq!(inner.desc.blocks, (block_size / SECTOR_SIZE) as u32);
            inner.desc.block_ptrs[0]
        };
        assert_ne!(allocated_bid, 0);

        let mut buf = vec![0u8; block_size];
        f.disk
            .segment()
            .read_bytes(Bid::new(allocated_bid as u64).to_offset(), &mut buf)
            .unwrap();

        let dot =
            DirEntry::parse_at(&buf, 0, block_size, f.ext2.super_block().total_inodes()).unwrap();
        assert_eq!(dot.inode, 12);
        assert_eq!(dot.name.as_bytes(), b".");

        let dotdot = DirEntry::parse_at(
            &buf,
            DirEntry::dir_rec_len(1) as usize,
            block_size,
            f.ext2.super_block().total_inodes(),
        )
        .unwrap();
        assert_eq!(dotdot.inode, ROOT_INO);
        assert_eq!(dotdot.name.as_bytes(), b"..");
    }

    #[ktest]
    fn empty_dir_false_on_non_dot_entries() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let (disk, ext2) = (&f.disk, &f.ext2);
        let block_size = ext2.block_size();

        let data_bid = 80u32;
        let mut block = vec![0u8; block_size];
        encode_dir_entry(&mut block, 0, 2, 12, b".", 2);
        encode_dir_entry(&mut block, 12, 2, 12, b"..", 2);
        encode_dir_entry(&mut block, 24, 13, (block_size - 24) as u16, b"foo", 1);
        disk.segment()
            .write_bytes(Bid::new(data_bid as u64).to_offset(), &block)
            .unwrap();

        let mut ptrs = [0u32; 15];
        ptrs[0] = data_bid;
        let inode = make_live_dir_inode(&ext2, 2, block_size, 8, FileFlags::empty(), ptrs);

        assert!(!inode.inner.read().empty_dir());
    }

    struct RmdirTestEnv {
        f: testkit::Ext2Fixture,
        child_ino: u32,
    }

    /// Sets up a parent directory (ROOT_INO) with a "sub" child directory.
    /// `child_extra_entries` is written into the child block after "." and "..".
    fn prepare_rmdir_env(
        child_extra_entries: &[(u32, &[u8], u8)],
    ) -> RmdirTestEnv {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let block_size = f.sb.block_size();

        let first = f.sb.group_first_block_no(0);
        let last = f.sb.group_last_block_no(0);
        let parent_bid = first
            .saturating_add(2)
            .saturating_add(f.sb.itb_per_group())
            .saturating_add(1);
        let child_bid = parent_bid.saturating_add(1);
        assert!(child_bid <= last);

        let child_ino = f.sb.first_ino().saturating_add(1);
        assert!(child_ino <= f.sb.total_inodes());

        testkit::write_block_bitmap(f.disk.as_ref(), &f.sb, &f.descs[0], &[parent_bid, child_bid]);
        testkit::write_inode_bitmap(f.disk.as_ref(), &f.sb, &f.descs[0], &[ROOT_INO, child_ino]);

        // Parent block: "." + ".." + "sub" -> child_ino.
        let mut parent_block = vec![0u8; block_size];
        encode_dir_entry(&mut parent_block, 0, ROOT_INO, 12, b".", 2);
        encode_dir_entry(&mut parent_block, 12, ROOT_INO, 12, b"..", 2);
        encode_dir_entry(
            &mut parent_block,
            24,
            child_ino,
            (block_size - 24) as u16,
            b"sub",
            2,
        );
        f.disk
            .segment()
            .write_bytes(Bid::new(parent_bid as u64).to_offset(), &parent_block)
            .unwrap();

        // Child block: "." + ".." + optional extra entries.
        let mut child_block = vec![0u8; block_size];
        encode_dir_entry(&mut child_block, 0, child_ino, 12, b".", 2);
        if child_extra_entries.is_empty() {
            encode_dir_entry(&mut child_block, 12, ROOT_INO, (block_size - 12) as u16, b"..", 2);
        } else {
            encode_dir_entry(&mut child_block, 12, ROOT_INO, 12, b"..", 2);
            let mut offset = 24;
            for (i, &(ino, name, ft)) in child_extra_entries.iter().enumerate() {
                let is_last = i == child_extra_entries.len() - 1;
                let rec_len = if is_last {
                    (block_size - offset) as u16
                } else {
                    DirEntry::dir_rec_len(name.len())
                };
                encode_dir_entry(&mut child_block, offset, ino, rec_len, name, ft);
                offset += rec_len as usize;
            }
        }
        f.disk
            .segment()
            .write_bytes(Bid::new(child_bid as u64).to_offset(), &child_block)
            .unwrap();

        // Parent raw inode: links=3 (self + "." + child "..").
        let mut parent_raw = make_raw_inode(0o040755);
        parent_raw.size_lo = block_size as u32;
        parent_raw.blocks = (block_size / SECTOR_SIZE) as u32;
        parent_raw.links_count = 3;
        parent_raw.block[0] = parent_bid;
        f.ext2.write_inode_desc(ROOT_INO, &parent_raw).unwrap();

        // Child raw inode: links=2 ("." + parent "sub").
        let mut child_raw = make_raw_inode(0o040755);
        child_raw.size_lo = block_size as u32;
        child_raw.blocks = (block_size / SECTOR_SIZE) as u32;
        child_raw.links_count = 2;
        child_raw.block[0] = child_bid;
        f.ext2.write_inode_desc(child_ino, &child_raw).unwrap();

        RmdirTestEnv { f, child_ino }
    }

    #[ktest]
    fn dir_rmdir_ok() {
        clocks::init_for_ktest();

        let env = prepare_rmdir_env(&[]);
        let f = &env.f;
        let child_ino = env.child_ino;

        let parent = f.ext2.read_inode(ROOT_INO).unwrap();
        {
            let mut parent_inner = parent.inner.write();
            parent_inner.rmdir("sub").unwrap();
            assert_eq!(parent_inner.desc.links_count, 2);
            assert_eq!(
                parent_inner.find_entry("sub").unwrap_err().error(),
                Errno::ENOENT
            );
        }

        let parent_desc = f.ext2.read_inode_desc(ROOT_INO).unwrap();
        assert_eq!(parent_desc.links_count, 2);
        let child_desc = f.ext2.read_inode_desc(child_ino).unwrap();
        assert_eq!(child_desc.size, 0);
        assert_eq!(child_desc.links_count, 0);

        let inode_bitmap = f.read_inode_bitmap(0).unwrap();
        assert!(!testkit::bit_is_set_lsb0(
            &inode_bitmap,
            (child_ino - 1) as usize
        ));
    }

    #[ktest]
    fn dir_rmdir_enotempty_keeps_parent_entry() {
        clocks::init_for_ktest();

        let env = prepare_rmdir_env(&[(ROOT_INO, b"foo", 1)]);
        let f = &env.f;
        let child_ino = env.child_ino;

        let parent = f.ext2.read_inode(ROOT_INO).unwrap();
        let err = parent.inner.write().rmdir("sub").unwrap_err();
        assert_eq!(err.error(), Errno::ENOTEMPTY);

        let parent_inner = parent.inner.read();
        assert_eq!(parent_inner.find_entry("sub").unwrap(), child_ino);
        assert_eq!(parent_inner.desc.links_count, 3);

        let inode_bitmap = f.read_inode_bitmap(0).unwrap();
        assert!(testkit::bit_is_set_lsb0(
            &inode_bitmap,
            (child_ino - 1) as usize
        ));
    }

    #[ktest]
    fn block_mapping_ok() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let (disk, ext2) = (&f.disk, &f.ext2);

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
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let (disk, ext2) = (&f.disk, &f.ext2);

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
        let io_base = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let io_fail_offset = Bid::new(40).to_offset();
        let io_disk = Arc::new(ErrorBioDisk::with_read_error_at(
            io_base.disk.clone(),
            BioStatus::IoError,
            io_fail_offset,
        ));
        let io_f = Ext2FixtureBuilder::new(2, 256)
            .with_device(io_disk)
            .build()
            .unwrap();
        let io_ext2 = &io_f.ext2;

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
