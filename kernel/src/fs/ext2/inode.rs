// SPDX-License-Identifier: MPL-2.0

use core::mem::size_of;

use ostd::{const_assert, mm::io_util::HasVmReaderWriter};

use super::{
    fs::{Ext2, ROOT_INO},
    prelude::*,
    utils::now,
};
use crate::{
    fs::{
        ext2::dir::{DirEntry, DirEntryIter},
        utils::{Extension, InodeMode, Metadata},
    },
    process::{Gid, Uid},
};

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
    extension: Extension,
}

impl Inode {
    pub(super) fn new(
        ino: u32,
        type_: InodeType,
        desc: Dirty<InodeDesc>,
        block_group_idx: usize,
        fs: Weak<Ext2>,
    ) -> Arc<Self> {
        // Use `new_cyclic` so `InodeInner` can build a `PageCache` backend that
        // points back to this inode via `Weak<dyn PageCacheBackend>`.
        Arc::new_cyclic(|weak_self: &Weak<Self>| Self {
            ino,
            type_,
            inner: RwMutex::new(InodeInner::new(desc, weak_self.clone(), fs.clone())),
            block_group_idx,
            fs,
            extension: Extension::new(),
        })
    }

    pub(super) fn ino(&self) -> u32 {
        self.ino
    }

    pub(super) fn fs_arc(&self) -> Result<Arc<Ext2>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))
    }

    pub(super) fn file_size(&self) -> usize {
        self.inner.read().desc.size as usize
    }

    pub(super) fn resize(&self, new_size: usize) -> Result<()> {
        let fs = self.fs_arc()?;
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        let old_size = {
            let inner = self.inner.read();
            if inner.desc.type_ != InodeType::File
                && inner.desc.type_ != InodeType::Dir
                && inner.desc.type_ != InodeType::SymLink
            {
                return_errno!(Errno::EINVAL);
            }

            if inner.desc.type_ == InodeType::SymLink
                && inner.desc.blocks == 0
                && inner.desc.size <= 60
            {
                return_errno!(Errno::EINVAL);
            }

            if inner
                .desc
                .flags
                .intersects(FileFlags::APPEND_ONLY | FileFlags::IMMUTABLE)
            {
                return_errno!(Errno::EPERM);
            }

            let old_size = inner.desc.size as usize;
            if new_size == old_size {
                return Ok(());
            }
            old_size
        };

        if new_size < old_size && new_size % block_size != 0 {
            // Linux-compatible shrink semantics: zero the truncated tail in the
            // last partial block before dropping cache pages / blocks.
            let zero_to = new_size.align_up(block_size);
            let inner = self.inner.read();
            inner.page_cache.fill_zeros(new_size..zero_to)?;
        }

        let mut inner = self.inner.write();
        let old_size = inner.desc.size as usize;
        if new_size == old_size {
            return Ok(());
        }

        if new_size < old_size {
            let old_size_aligned = old_size.align_up(block_size);
            let new_size_aligned = new_size.align_up(block_size);
            if new_size_aligned < old_size_aligned {
                inner
                    .page_cache
                    .discard_range(new_size_aligned..old_size_aligned);
            }
            inner.page_cache.resize(new_size_aligned)?;
            inner.desc.size = new_size as u64;
            inner.truncate_blocks(new_size)?;
        } else {
            inner.page_cache.resize(new_size.align_up(block_size))?;
            inner.desc.size = new_size as u64;
        }

        let current = now();
        inner.desc.mtime = current;
        inner.desc.ctime = current;
        inner.persist_inode_and_sync(&fs)
    }

    pub(super) fn metadata(&self) -> Metadata {
        let inner = self.inner.read();
        let (dev, blk_size) = match self.fs.upgrade() {
            Some(fs) => (fs.block_device().id().as_encoded_u64(), fs.block_size()),
            None => (0, BLOCK_SIZE),
        };
        Metadata {
            dev,
            ino: self.ino as u64,
            size: inner.desc.size as usize,
            blk_size,
            blocks: inner.desc.blocks as usize,
            atime: inner.desc.atime,
            mtime: inner.desc.mtime,
            ctime: inner.desc.ctime,
            type_: self.type_,
            mode: InodeMode::from_bits_truncate(inner.desc.perm.bits() as _),
            nlinks: inner.desc.links_count as usize,
            uid: Uid::new(inner.desc.uid),
            gid: Gid::new(inner.desc.gid),
            rdev: 0,
        }
    }

    pub(super) fn inode_type(&self) -> InodeType {
        self.type_
    }

    pub(super) fn mode(&self) -> InodeMode {
        InodeMode::from_bits_truncate(self.inner.read().desc.perm.bits() as _)
    }

    pub(super) fn set_mode(&self, mode: InodeMode) -> Result<()> {
        let fs = self.fs_arc()?;
        let mut inner = self.inner.write();
        inner.desc.perm = FilePerm::from_bits_truncate(mode.bits() as u16);
        inner.desc.ctime = now();
        inner.persist_inode_and_sync(&fs)
    }

    pub(super) fn uid(&self) -> u32 {
        self.inner.read().desc.uid
    }

    pub(super) fn set_uid(&self, uid: u32) -> Result<()> {
        let fs = self.fs_arc()?;
        let mut inner = self.inner.write();
        inner.desc.uid = uid;
        inner.desc.ctime = now();
        inner.persist_inode_and_sync(&fs)
    }

    pub(super) fn gid(&self) -> u32 {
        self.inner.read().desc.gid
    }

    pub(super) fn set_gid(&self, gid: u32) -> Result<()> {
        let fs = self.fs_arc()?;
        let mut inner = self.inner.write();
        inner.desc.gid = gid;
        inner.desc.ctime = now();
        inner.persist_inode_and_sync(&fs)
    }

    pub(super) fn atime(&self) -> Duration {
        self.inner.read().desc.atime
    }

    pub(super) fn set_atime(&self, time: Duration) {
        self.inner.write().desc.atime = time;
    }

    pub(super) fn mtime(&self) -> Duration {
        self.inner.read().desc.mtime
    }

    pub(super) fn set_mtime(&self, time: Duration) {
        self.inner.write().desc.mtime = time;
    }

    pub(super) fn ctime(&self) -> Duration {
        self.inner.read().desc.ctime
    }

    pub(super) fn set_ctime(&self, time: Duration) {
        self.inner.write().desc.ctime = time;
    }

    pub(super) fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        if writer.avail() == 0 {
            return Ok(0);
        }

        let read_len = {
            let inner = self.inner.read();
            let file_size = inner.desc.size as usize;
            if offset >= file_size {
                return Ok(0);
            }
            let read_len = writer.avail().min(file_size.saturating_sub(offset));
            writer.limit(read_len);
            inner.page_cache.pages().read(offset, writer)?;
            read_len
        };

        self.set_atime(now());
        Ok(read_len)
    }

    pub(super) fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        let write_len = reader.remain();
        if write_len == 0 {
            return Ok(0);
        }

        let fs = self.fs_arc()?;
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        let end = offset
            .checked_add(write_len)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "write range overflow"))?;

        {
            let mut inner = self.inner.write();
            let old_size = inner.desc.size as usize;
            let start_block = offset / block_size;
            let end_block = end.div_ceil(block_size);

            // Phase 1: ensure all target blocks exist and grow page-cache/file size
            // first, so data write (phase 2) only touches mapped pages.
            let phase1_result = (|| -> Result<()> {
                for iblock in start_block..end_block {
                    let iblock = u32::try_from(iblock).map_err(|_| {
                        Error::with_message(Errno::EINVAL, "logical block number overflow")
                    })?;
                    if inner.get_or_alloc_block(iblock, true)?.is_none() {
                        return_errno_with_message!(
                            Errno::EIO,
                            "missing block mapping after allocation"
                        );
                    }
                }

                if end > old_size {
                    inner.page_cache.resize(end.align_up(block_size))?;
                    inner.desc.size = end as u64;
                }

                Ok(())
            })();

            if let Err(err) = phase1_result {
                Self::write_failed_cleanup(&mut inner, old_size, end, block_size);
                return Err(err);
            }
        }

        {
            let inner = self.inner.read();
            // Phase 2: copy user data through VMO-backed page cache.
            inner.page_cache.pages().write(offset, reader)?;
        }

        let mut inner = self.inner.write();
        let current = now();
        inner.desc.mtime = current;
        inner.desc.ctime = current;
        inner.persist_inode_and_sync(&fs)?;
        Ok(write_len)
    }

    /// Direct-I/O read path.
    ///
    /// Linux: /root/linux/fs/ext2/file.c:168 (ext2_dio_read_iter)
    pub(super) fn read_direct_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        let fs = self.fs_arc()?;
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }
        if !offset.is_multiple_of(block_size) || !writer.avail().is_multiple_of(block_size) {
            return_errno_with_message!(Errno::EINVAL, "not block-aligned");
        }

        let read_len = {
            let inner = self.inner.read();
            let file_size = inner.desc.size as usize;
            if offset >= file_size {
                0
            } else {
                let read_len = writer.avail().min(file_size.saturating_sub(offset));
                let end = offset
                    .checked_add(read_len)
                    .ok_or_else(|| Error::with_message(Errno::EINVAL, "read range overflow"))?;
                inner.page_cache.discard_range(offset..end);
                inner.read_at(offset, writer)?
            }
        };

        self.set_atime(now());
        Ok(read_len)
    }

    /// Direct-I/O write path with pre-allocation and rollback.
    ///
    /// Linux: /root/linux/fs/ext2/file.c:214 (ext2_dio_write_iter)
    /// Linux: /root/linux/fs/ext2/file.c:183 (ext2_dio_write_end_io)
    /// Linux: /root/linux/fs/ext2/inode.c:59 (ext2_write_failed)
    pub(super) fn write_direct_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        let fs = self.fs_arc()?;
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }
        if !offset.is_multiple_of(block_size) || !reader.remain().is_multiple_of(block_size) {
            return_errno_with_message!(Errno::EINVAL, "not block-aligned");
        }

        let write_len = reader.remain();
        if write_len == 0 {
            return Ok(0);
        }
        let end = offset
            .checked_add(write_len)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "write range overflow"))?;
        let old_size;

        {
            let mut inner = self.inner.write();
            old_size = inner.desc.size as usize;
            let start_block = offset / block_size;
            let end_block = end.div_ceil(block_size);

            let phase1_result = (|| -> Result<()> {
                for iblock in start_block..end_block {
                    let iblock = u32::try_from(iblock).map_err(|_| {
                        Error::with_message(Errno::EINVAL, "logical block number overflow")
                    })?;
                    if inner.get_or_alloc_block(iblock, true)?.is_none() {
                        return_errno_with_message!(
                            Errno::EIO,
                            "missing block mapping after allocation"
                        );
                    }
                }

                if end > old_size {
                    inner.page_cache.resize(end.align_up(block_size))?;
                    inner.desc.size = end as u64;
                }

                let discard_start = offset.min(old_size);
                let discard_end = end.min(old_size);
                if discard_start < discard_end {
                    inner.page_cache.discard_range(discard_start..discard_end);
                }

                Ok(())
            })();

            if let Err(err) = phase1_result {
                Self::write_failed_cleanup(&mut inner, old_size, end, block_size);
                return Err(err);
            }
        }

        {
            let inner = self.inner.read();
            if let Err(err) = inner.write_at(offset, reader) {
                drop(inner);
                let mut inner = self.inner.write();
                Self::write_failed_cleanup(&mut inner, old_size, end, block_size);
                return Err(err);
            }
        }

        let mut inner = self.inner.write();
        let current = now();
        inner.desc.mtime = current;
        inner.desc.ctime = current;
        inner.persist_inode_and_sync(&fs)?;
        Ok(write_len)
    }

    fn write_failed_cleanup(
        inner: &mut InodeInner,
        old_size: usize,
        end: usize,
        block_size: usize,
    ) {
        if end <= old_size {
            return;
        }

        let old_size_aligned = old_size.align_up(block_size);
        let end_aligned = end.align_up(block_size);
        // Mirrors Linux ext2 write failure rollback: drop speculative cache range
        // and truncate newly allocated blocks back to the old size.
        inner
            .page_cache
            .discard_range(old_size_aligned..end_aligned);

        if let Err(err) = inner.page_cache.resize(old_size_aligned) {
            error!(
                "ext2: write_at cleanup page cache resize failed: old_size_aligned={}, err={:?}",
                old_size_aligned, err
            );
        }
        if let Err(err) = inner.truncate_blocks(old_size) {
            error!(
                "ext2: write_at cleanup truncate_blocks failed: old_size={}, err={:?}",
                old_size, err
            );
        }
        inner.desc.size = old_size as u64;
    }

    pub(super) fn lookup(&self, name: &str) -> Result<Arc<Inode>> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let ino = self.inner.read().find_entry(name)?;
        self.fs_arc()?.read_inode(ino)
    }

    /// Adds a new directory entry using upread/upgrade phases.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:476 (ext2_add_link)
    pub(super) fn add_entry(
        &self,
        name: &str,
        ino: u32,
        file_type: DirEntryFileType,
    ) -> Result<()> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > u8::MAX as usize {
            return_errno!(Errno::EINVAL);
        }

        let fs = self.fs_arc()?;
        let max_inumber = fs.super_block().total_inodes();
        if ino == 0 || ino > max_inumber {
            return_errno!(Errno::EINVAL);
        }

        // SPEC: keep upread while doing all PageCache I/O.
        let mut inner = self.inner.upread();
        let slot = match inner.scan_dir_for_slot(name, &fs)? {
            DirScanResult::Slot(slot) => slot,
            DirScanResult::NeedGrowth => {
                // SPEC: upgrade only for metadata/block allocation mutation.
                let mut write_inner = inner.upgrade();
                let grown = write_inner.grow_dir_block(&fs)?;
                inner = write_inner.downgrade();
                grown
            }
        };

        inner.write_dir_entry_to_cache(&slot, name, ino, file_type as u8)?;

        // SPEC: upgrade after cache write to commit inode metadata.
        let mut write_inner = inner.upgrade();
        write_inner.commit_dir_metadata(&fs)
    }

    pub(super) fn readdir_at(
        &self,
        offset: usize,
        visitor: &mut dyn DirentVisitor,
    ) -> Result<usize> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        self.inner.read().readdir_at(offset, visitor)
    }

    /// Deletes a directory entry by name using upread/upgrade phases.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:560 (ext2_delete_entry)
    pub(super) fn delete_entry(&self, name: &str) -> Result<()> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > u8::MAX as usize {
            return_errno!(Errno::EINVAL);
        }

        let fs = self.fs_arc()?;
        let inner = self.inner.upread();
        let target = inner.find_entry_target(name).map_err(|err| {
            if err.error() == Errno::ENOENT {
                Error::with_message(Errno::EIO, "dir entry not found for delete")
            } else {
                err
            }
        })?;
        inner.delete_entry_in_cache(&target)?;

        let mut write_inner = inner.upgrade();
        write_inner.commit_dir_metadata(&fs)
    }

    /// Initializes a directory with `.` and `..` using write->upread->write phases.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:617 (ext2_make_empty)
    pub(super) fn make_empty(&self, parent_ino: u32) -> Result<()> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let fs = self.fs_arc()?;
        let total_inodes = fs.super_block().total_inodes();
        if parent_ino == 0 || parent_ino > total_inodes {
            return_errno_with_message!(Errno::EINVAL, "parent inode number out of range");
        }

        let block_size = fs.block_size();
        let mut write_inner = self.inner.write();
        if write_inner.desc.block_ptrs[0] != 0 {
            return_errno_with_message!(Errno::EIO, "dir block pointer already occupied");
        }

        let old_ptr0 = write_inner.desc.block_ptrs[0];
        let old_size = write_inner.desc.size;
        let old_blocks = write_inner.desc.blocks;

        // SPEC: allocate first data block under write lock (&mut self required).
        let new_bid = write_inner
            .get_or_alloc_block(0, true)?
            .ok_or_else(|| {
                Error::with_message(Errno::ENOSPC, "failed to allocate first dir block")
            })?
            .to_raw() as u32;
        write_inner.desc.size = block_size as u64;

        if let Err(err) = write_inner.page_cache.resize(block_size) {
            write_inner.page_cache.discard_range(0..block_size);
            write_inner.desc.block_ptrs[0] = old_ptr0;
            write_inner.desc.size = old_size;
            write_inner.desc.blocks = old_blocks;
            let _ = fs.free_blocks(new_bid, 1);
            return Err(err);
        }

        // SPEC: downgrade for PageCache write path.
        let upread_inner = write_inner.downgrade();
        let mut buf = vec![0u8; block_size];
        InodeInner::write_dir_entry_bytes(
            &mut buf,
            0,
            self.ino,
            DirEntry::dir_rec_len(1),
            b".",
            DirEntryFileType::Dir as u8,
        )?;
        let dot_len = DirEntry::dir_rec_len(1) as usize;
        InodeInner::write_dir_entry_bytes(
            &mut buf,
            dot_len,
            parent_ino,
            (block_size.saturating_sub(dot_len)) as u16,
            b"..",
            DirEntryFileType::Dir as u8,
        )?;

        if let Err(err) = upread_inner.page_cache.pages().write_bytes(0, &buf) {
            let mut write_inner = upread_inner.upgrade();
            write_inner.page_cache.discard_range(0..block_size);
            write_inner.desc.block_ptrs[0] = old_ptr0;
            write_inner.desc.size = old_size;
            write_inner.desc.blocks = old_blocks;
            let _ = fs.free_blocks(new_bid, 1);
            return Err(err.into());
        }

        let write_inner = upread_inner.upgrade();
        if let Err(err) = write_inner.persist_inode_and_sync(&fs) {
            let mut write_inner = write_inner;
            write_inner.page_cache.discard_range(0..block_size);
            write_inner.desc.block_ptrs[0] = old_ptr0;
            write_inner.desc.size = old_size;
            write_inner.desc.blocks = old_blocks;
            let _ = fs.free_blocks(new_bid, 1);
            return Err(err);
        }

        Ok(())
    }

    pub(super) fn empty_dir(&self) -> bool {
        self.inner.read().empty_dir()
    }

    pub(super) fn rmdir(&self, name: &str) -> Result<()> {
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

        let fs = self.fs_arc()?;
        let parent_upread = self.inner.upread();
        let child_ino = parent_upread.find_entry(name)?;
        let child = fs.read_inode(child_ino)?;

        {
            let child_read = child.inner.read();
            if child_read.desc.type_ != InodeType::Dir {
                return_errno!(Errno::ENOTDIR);
            }
            if !child_read.empty_dir() {
                return_errno!(Errno::ENOTEMPTY);
            }
        }

        let target = parent_upread.find_entry_target(name)?;
        parent_upread.delete_entry_in_cache(&target)?;
        let mut parent_write = parent_upread.upgrade();
        parent_write.commit_dir_metadata(&fs)?;

        {
            let mut child_write = child.inner.write();
            child_write.release_dir_data_blocks_for_cleanup(&fs)?;
            child_write.desc.links_count = child_write.desc.links_count.saturating_sub(2);
            child_write.desc.dtime = now();
            child_write.persist_inode_and_sync(&fs)?;
        }

        parent_write.desc.links_count = parent_write.desc.links_count.saturating_sub(1);
        parent_write.persist_inode_and_sync(&fs)?;
        fs.free_inode(child_ino)
    }

    /// Creates a subdirectory under this directory using phased locking.
    ///
    /// Linux: /root/linux/fs/ext2/namei.c:228 (ext2_mkdir)
    pub(super) fn mkdir(&self, name: &str, perm: FilePerm) -> Result<Arc<Inode>> {
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

        let fs = self.fs_arc()?;

        // SPEC: hold upread for directory-data phases and upgrade for metadata phases.
        let mut parent_guard = self.inner.upread();
        let slot = match parent_guard.scan_dir_for_slot(name, &fs)? {
            DirScanResult::Slot(slot) => slot,
            DirScanResult::NeedGrowth => {
                let mut parent_write = parent_guard.upgrade();
                let grown = parent_write.grow_dir_block(&fs)?;
                parent_guard = parent_write.downgrade();
                grown
            }
        };

        let mut parent_write = parent_guard.upgrade();
        parent_write.desc.links_count = parent_write.desc.links_count.saturating_add(1);
        parent_guard = parent_write.downgrade();

        let child = match fs.create_inode(self.ino, InodeType::Dir, perm) {
            Ok(child) => child,
            Err(err) => {
                let mut parent_write = parent_guard.upgrade();
                parent_write.desc.links_count = parent_write.desc.links_count.saturating_sub(1);
                return Err(err);
            }
        };
        let child_ino = child.ino();

        if let Err(err) = child.make_empty(self.ino) {
            let _ = fs.free_inode(child_ino);
            let mut parent_write = parent_guard.upgrade();
            parent_write.desc.links_count = parent_write.desc.links_count.saturating_sub(1);
            return Err(err);
        }

        if let Err(err) = parent_guard.write_dir_entry_to_cache(
            &slot,
            name,
            child_ino,
            DirEntryFileType::Dir as u8,
        ) {
            {
                let mut child_inner = child.inner.write();
                let _ = child_inner.release_dir_data_blocks_for_cleanup(&fs);
            }
            let _ = fs.free_inode(child_ino);
            let mut parent_write = parent_guard.upgrade();
            parent_write.desc.links_count = parent_write.desc.links_count.saturating_sub(1);
            return Err(err);
        }

        let mut parent_write = parent_guard.upgrade();
        if let Err(err) = parent_write.commit_dir_metadata(&fs) {
            let _ = self.delete_entry(name);
            {
                let mut child_inner = child.inner.write();
                let _ = child_inner.release_dir_data_blocks_for_cleanup(&fs);
            }
            let _ = fs.free_inode(child_ino);
            parent_write.desc.links_count = parent_write.desc.links_count.saturating_sub(1);
            return Err(err);
        }

        Ok(child)
    }

    pub(super) fn sync_all(&self) -> Result<()> {
        let fs = self.fs_arc()?;
        self.inner.read().persist_inode_and_sync(&fs)?;
        fs.block_device().sync()?;
        Ok(())
    }

    pub(super) fn sync_data(&self) -> Result<()> {
        self.fs_arc()?.block_device().sync()?;
        Ok(())
    }

    pub(super) fn extension(&self) -> &Extension {
        &self.extension
    }

    pub(super) fn page_cache_vmo(&self) -> Arc<Vmo> {
        self.inner.read().page_cache.pages().clone()
    }
}

impl PageCacheBackend for Inode {
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter> {
        let inner = self.inner.read();
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let iblock = u32::try_from(idx)
            .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;

        match inner.get_block(iblock)? {
            Some(bid) => {
                let bio_segment = BioSegment::new_from_segment(
                    Segment::from(frame.clone()).into(),
                    BioDirection::FromDevice,
                );
                Ok(fs.block_device().read_blocks_async(bid, bio_segment)?)
            }
            None => {
                // Sparse hole: return a zero-filled page without issuing BIO.
                frame.writer().fill_zeros(BLOCK_SIZE);
                Ok(BioWaiter::new())
            }
        }
    }

    fn write_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter> {
        let inner = self.inner.read();
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let iblock = u32::try_from(idx)
            .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;

        let bid = inner.get_block(iblock)?.ok_or_else(|| {
            error!("write_page_async: no block mapping for idx {}", idx);
            Error::with_message(Errno::EIO, "missing block mapping for writeback")
        })?;

        let bio_segment = BioSegment::new_from_segment(
            Segment::from(frame.clone()).into(),
            BioDirection::ToDevice,
        );
        Ok(fs.block_device().write_blocks_async(bid, bio_segment)?)
    }

    fn npages(&self) -> usize {
        let inner = self.inner.read();
        (inner.desc.size as usize).align_up(BLOCK_SIZE) / BLOCK_SIZE
    }
}

#[derive(Debug)]
pub struct InodeInner {
    desc: Dirty<InodeDesc>,
    is_freed: bool,
    weak_self: Weak<Inode>,
    fs: Weak<Ext2>,
    page_cache: PageCache,
}

/// Scan result for directory slot search.
#[derive(Debug)]
enum DirScanResult {
    /// Found a usable slot in an existing block.
    Slot(DirSlotInfo),
    /// No slot found; directory must grow by one block.
    NeedGrowth,
}

/// Information about a candidate directory entry slot.
#[derive(Clone, Copy, Debug)]
struct DirSlotInfo {
    /// Byte offset within the directory (block_idx * block_size + offset_in_block).
    dir_offset: usize,
    /// Current rec_len of the candidate slot.
    slot_rec_len: usize,
    /// Minimal occupied length of the existing entry head (0 if slot is free).
    used_rec_len: usize,
}

/// Located directory entry for delete/set_link.
#[derive(Clone, Copy, Debug)]
struct DirEntryTarget {
    /// Byte offset of the target entry within the directory.
    dir_offset: usize,
    /// rec_len of the target entry.
    entry_rec_len: usize,
}

impl InodeInner {
    pub fn new(desc: Dirty<InodeDesc>, weak_self: Weak<Inode>, fs: Weak<Ext2>) -> Self {
        let num_page_bytes = (desc.size as usize).align_up(BLOCK_SIZE);
        let backend: Weak<dyn PageCacheBackend> = weak_self.clone();
        // Keep page-cache capacity aligned with inode size so `npages`/VMO window
        // and on-disk data extent stay consistent from mount time.
        let page_cache = if num_page_bytes == 0 {
            PageCache::new(backend)
        } else {
            PageCache::with_capacity(num_page_bytes, backend)
        }
        .expect("ext2 inode page cache allocation failed");

        Self {
            desc,
            is_freed: false,
            weak_self,
            fs,
            page_cache,
        }
    }

    /// Reads file data directly from data blocks into `writer`.
    ///
    /// Linux: /root/linux/fs/ext2/file.c:168 (ext2_dio_read_iter)
    pub fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        if self.desc.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        let file_size = self.desc.size as usize;
        if offset >= file_size || writer.avail() == 0 {
            return Ok(0);
        }

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        let read_len = writer.avail().min(file_size.saturating_sub(offset));
        let mut current_offset = offset;
        let end = offset
            .checked_add(read_len)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "read range overflow"))?;

        while current_offset < end {
            let iblock = u32::try_from(current_offset / block_size)
                .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;
            let offset_in_block = current_offset % block_size;
            let bytes_this_block = (block_size - offset_in_block).min(end - current_offset);

            match self.get_block(iblock)? {
                Some(bid) => {
                    let bio_segment = BioSegment::alloc(1, BioDirection::FromDevice);
                    let status = fs
                        .block_device()
                        .read_blocks(bid, bio_segment.clone())
                        .map_err(|_| {
                            Error::with_message(Errno::EIO, "failed to read data block")
                        })?;
                    if status != BioStatus::Complete {
                        return_errno_with_message!(Errno::EIO, "failed to read data block");
                    }

                    if offset_in_block == 0 && bytes_this_block == block_size {
                        let mut segment_reader = bio_segment.reader().map_err(|_| {
                            Error::with_message(Errno::EIO, "failed to access bio read segment")
                        })?;
                        segment_reader.read_fallible(writer)?;
                    } else {
                        let mut block_buf = vec![0u8; block_size];
                        {
                            let mut segment_reader = bio_segment.reader().map_err(|_| {
                                Error::with_message(Errno::EIO, "failed to access bio read segment")
                            })?;
                            let mut block_writer =
                                VmWriter::from(block_buf.as_mut_slice()).to_fallible();
                            segment_reader.read_fallible(&mut block_writer)?;
                        }

                        let copy_end =
                            offset_in_block
                                .checked_add(bytes_this_block)
                                .ok_or_else(|| {
                                    Error::with_message(Errno::EINVAL, "read block slice overflow")
                                })?;
                        let mut block_reader =
                            VmReader::from(&block_buf[offset_in_block..copy_end]).to_fallible();
                        writer.write_fallible(&mut block_reader)?;
                    }
                }
                None => {
                    // Sparse hole: return zero-filled bytes without issuing BIO.
                    writer.fill_zeros(bytes_this_block)?;
                }
            }

            current_offset = current_offset.saturating_add(bytes_this_block);
        }

        Ok(read_len)
    }

    /// Writes file data directly to already-allocated data blocks.
    ///
    /// Linux: /root/linux/fs/ext2/file.c:214 (ext2_dio_write_iter)
    pub fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize> {
        if self.desc.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }
        if reader.remain() == 0 {
            return Ok(0);
        }

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        let write_len = reader.remain();
        let end = offset
            .checked_add(write_len)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "write range overflow"))?;
        let mut current_offset = offset;

        while current_offset < end {
            let iblock = u32::try_from(current_offset / block_size)
                .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;
            let offset_in_block = current_offset % block_size;
            let bytes_this_block = (block_size - offset_in_block).min(end - current_offset);
            let bid = self.get_block(iblock)?.ok_or_else(|| {
                Error::with_message(Errno::EIO, "missing block mapping for direct write")
            })?;

            let mut block_buf = vec![0u8; block_size];
            if offset_in_block != 0 || bytes_this_block < block_size {
                let read_segment = BioSegment::alloc(1, BioDirection::FromDevice);
                let read_status = fs
                    .block_device()
                    .read_blocks(bid, read_segment.clone())
                    .map_err(|_| {
                        Error::with_message(Errno::EIO, "failed to read block for partial write")
                    })?;
                if read_status != BioStatus::Complete {
                    return_errno_with_message!(
                        Errno::EIO,
                        "failed to read block for partial write"
                    );
                }

                let mut segment_reader = read_segment.reader().map_err(|_| {
                    Error::with_message(Errno::EIO, "failed to access bio read segment")
                })?;
                let mut block_writer = VmWriter::from(block_buf.as_mut_slice()).to_fallible();
                segment_reader.read_fallible(&mut block_writer)?;
            }

            let copy_end = offset_in_block
                .checked_add(bytes_this_block)
                .ok_or_else(|| Error::with_message(Errno::EINVAL, "write block slice overflow"))?;
            let mut slice_writer =
                VmWriter::from(&mut block_buf[offset_in_block..copy_end]).to_fallible();
            slice_writer.write_fallible(reader)?;

            let write_segment = BioSegment::alloc(1, BioDirection::ToDevice);
            {
                let mut segment_writer = write_segment.writer().map_err(|_| {
                    Error::with_message(Errno::EIO, "failed to access bio write segment")
                })?;
                let mut block_reader = VmReader::from(block_buf.as_slice()).to_fallible();
                segment_writer.write_fallible(&mut block_reader)?;
            }

            let write_status = fs
                .block_device()
                .write_blocks(bid, write_segment)
                .map_err(|_| Error::with_message(Errno::EIO, "failed to write data block"))?;
            if write_status != BioStatus::Complete {
                return_errno_with_message!(Errno::EIO, "failed to write data block");
            }

            current_offset = current_offset.saturating_add(bytes_this_block);
        }

        Ok(write_len)
    }

    /// Resizes this inode to `new_size` bytes.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1275 (ext2_setsize)
    pub fn resize(&mut self, new_size: usize) -> Result<()> {
        // SPEC: ext2_setsize only supports regular file, directory, and symlink.
        if self.desc.type_ != InodeType::File
            && self.desc.type_ != InodeType::Dir
            && self.desc.type_ != InodeType::SymLink
        {
            return_errno!(Errno::EINVAL);
        }

        // TODO: refactor this into a new func
        // FIXME: Linux ext2_inode_is_fast_symlink (fs/ext2/inode.c:48-55) uses:
        //   S_ISLNK && (i_blocks - ea_blocks == 0),
        // where ea_blocks depends on i_file_acl:
        //   ea_blocks = i_file_acl != 0 ? (block_size >> 9) : 0.
        // ACL/EA accounting is not implemented in this path yet, so this remains
        // an approximation until ACL support is added.
        // SPEC: reject fast symlink (inline data in i_block[]).
        if self.desc.type_ == InodeType::SymLink && self.desc.blocks == 0 && self.desc.size <= 60 {
            return_errno!(Errno::EINVAL);
        }

        // SPEC: Linux IS_APPEND/IS_IMMUTABLE gate.
        if self
            .desc
            .flags
            .intersects(FileFlags::APPEND_ONLY | FileFlags::IMMUTABLE)
        {
            return_errno!(Errno::EPERM);
        }

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        let old_size = self.desc.size as usize;
        if new_size == old_size {
            return Ok(());
        }

        // SPEC: ext2_setsize calls block_truncate_page before size update.
        if new_size % block_size != 0 {
            let tail_iblock = u32::try_from(new_size / block_size)
                .map_err(|_| Error::with_message(Errno::EINVAL, "resize block index overflow"))?;
            let zero_from = new_size % block_size;
            if let Some(tail_bid) = self.get_block(tail_iblock)? {
                let mut block_buf = vec![0u8; block_size];
                if fs
                    .block_device()
                    .read_bytes(tail_bid.to_offset(), &mut block_buf)
                    .is_err()
                {
                    return_errno_with_message!(
                        Errno::EIO,
                        "failed to read tail block during resize"
                    );
                }
                block_buf[zero_from..].fill(0);
                if fs
                    .block_device()
                    .write_bytes(tail_bid.to_offset(), &block_buf)
                    .is_err()
                {
                    return_errno_with_message!(
                        Errno::EIO,
                        "failed to write tail block during resize"
                    );
                }
            }
        }

        // SPEC: Linux truncate_setsize updates i_size before block release.
        // Linux ext2_setsize then always calls __ext2_truncate_blocks, even on
        // extension, so keep the same control-flow here.
        self.desc.size = new_size as u64;
        self.truncate_blocks(new_size)?;
        let current = now();
        self.desc.mtime = current;
        self.desc.ctime = current;
        self.persist_inode_and_sync(&fs)?;
        Ok(())
    }

    /// Frees blocks beyond `new_size`.
    ///
    /// This function implements the core truncation logic for ext2 files. It handles
    /// both direct blocks and indirect blocks (single, double, and triple indirect).
    /// The algorithm follows Linux's __ext2_truncate_blocks closely, including the
    /// all_zeroes optimization for sparse files.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1172 (__ext2_truncate_blocks)
    fn truncate_blocks(&mut self, new_size: usize) -> Result<()> {
        // === Initialization ===
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }
        let sectors_per_block = (block_size / SECTOR_SIZE) as u32;
        if sectors_per_block == 0 {
            return_errno_with_message!(Errno::EIO, "invalid sector accounting for block size");
        }

        // SPEC: first logical block to free = ceil(new_size / block_size).
        let iblock = u32::try_from(new_size.div_ceil(block_size))
            .map_err(|_| Error::with_message(Errno::EINVAL, "truncate size exceeds ext2 limits"))?;

        // Convert logical block number to access path (depth + offsets).
        let path = self.block_to_path(iblock)?;
        if path.depth == 0 {
            return Ok(());
        }

        let ptrs_per_block = block_size / size_of::<u32>();
        if ptrs_per_block == 0 {
            return_errno_with_message!(Errno::EIO, "invalid indirect pointer fanout");
        }

        // === Case 1: Direct blocks only ===
        if path.depth == 1 {
            // Linux: ext2_free_data(i_data + offsets[0], i_data + EXT2_NDIR_BLOCKS).
            // Free direct blocks from offsets[0] to block_ptrs[11].
            let start = (path.offsets[0] as usize).min(12);
            for idx in start..12 {
                let ptr = self.desc.block_ptrs[idx];
                if ptr == 0 {
                    continue;
                }
                fs.free_blocks(ptr, 1)?;
                self.desc.block_ptrs[idx] = 0;
                self.desc.blocks = self.desc.blocks.saturating_sub(sectors_per_block);
            }
        } else {
            // === Case 2: Indirect blocks ===

            // --- Step 1: Adjust depth for boundary case ---
            // SPEC: ext2_find_shared-style partial branch handling.
            // If truncation point is at the start of an indirect block (offset = 0),
            // we can handle it at a higher level without reading that indirect block.
            let mut k = path.depth;
            while k > 1 && path.offsets[k - 1] == 0 {
                k -= 1;
            }
            let shared_path = BlockPath {
                depth: k,
                offsets: path.offsets,
                boundary: path.boundary,
            };

            // --- Step 2: Read indirect block chain ---
            // branch.chain[i] contains the i-th level indirect block.
            // branch.partial_level indicates how deep we successfully read.
            let branch = self.get_branch(&shared_path, &fs)?;
            // `partial` is an index into `branch.chain[]`, pointing to the
            // deepest level from which we start detaching and freeing blocks.
            // If the full chain was read (partial_level == k), the last valid
            // index is k-1; otherwise partial_level already is the index where
            // traversal stopped on a zero pointer.
            // The all_zeroes loop below may shrink `partial` upward.
            let mut partial = if branch.partial_level == k {
                k.saturating_sub(1)
            } else {
                branch.partial_level
            };

            // --- Step 3: all_zeroes optimization ---
            // SPEC: preserve Linux all_zeroes optimization by walking up to the
            // highest indirect block that can be fully detached.
            // If the left side (to be kept) of an indirect block is all zeros,
            // we can free the entire indirect block and handle it at a higher level.
            while partial > 0 {
                let buf = branch
                    .chain
                    .get(partial)
                    .and_then(|entry| entry.bh.as_ref())
                    .ok_or_else(|| {
                        Error::with_message(
                            Errno::EIO,
                            "missing indirect block buffer for all-zeroes check",
                        )
                    })?;

                // Check if entries [0..offsets[partial]) are all zero.
                let keep_entries = path.offsets[partial] as usize;
                let keep_bytes = keep_entries.saturating_mul(size_of::<u32>());
                if keep_bytes > buf.len() {
                    return_errno_with_message!(Errno::EIO, "all-zeroes check offset out of bounds");
                }

                let mut all_zero = true;
                let mut byte = 0usize;
                while byte < keep_bytes {
                    let val = u32::from_le_bytes([
                        buf[byte],
                        buf[byte + 1],
                        buf[byte + 2],
                        buf[byte + 3],
                    ]);
                    if val != 0 {
                        all_zero = false;
                        break;
                    }
                    byte = byte.saturating_add(size_of::<u32>());
                }
                if !all_zero {
                    break;
                }
                // Left side is all zeros, move up one level.
                partial -= 1;
            }

            // --- Step 4: Detach subtree root ---
            // Disconnect the pointer at offsets[partial] and get the subtree root block number.
            let detached_nr;
            if partial == 0 {
                // Detach from inode.block_ptrs directly.
                let slot = path.offsets[0] as usize;
                if slot >= self.desc.block_ptrs.len() {
                    return_errno_with_message!(Errno::EIO, "inode block pointer slot out of range");
                }
                detached_nr = self.desc.block_ptrs[slot];
                self.desc.block_ptrs[slot] = 0;
            } else {
                // Detach from parent indirect block.
                // chain[partial] contains the current level's buffer.
                // chain[partial-1].key is the block number to write back.
                let mut parent_buf = branch
                    .chain
                    .get(partial)
                    .and_then(|entry| entry.bh.clone())
                    .ok_or_else(|| {
                        Error::with_message(Errno::EIO, "missing parent indirect block buffer")
                    })?;
                let parent_bid = branch
                    .chain
                    .get(partial - 1)
                    .map(|entry| entry.key)
                    .unwrap_or(0);
                if parent_bid == 0 {
                    return_errno_with_message!(Errno::EIO, "invalid parent indirect block number");
                }

                // Read the pointer at offsets[partial].
                let ptr_offset = (path.offsets[partial] as usize).saturating_mul(size_of::<u32>());
                let ptr_end = ptr_offset.saturating_add(size_of::<u32>());
                if ptr_end > parent_buf.len() {
                    return_errno_with_message!(Errno::EIO, "shared branch pointer out of bounds");
                }

                detached_nr = u32::from_le_bytes([
                    parent_buf[ptr_offset],
                    parent_buf[ptr_offset + 1],
                    parent_buf[ptr_offset + 2],
                    parent_buf[ptr_offset + 3],
                ]);

                // Zero out the pointer and write back.
                parent_buf[ptr_offset..ptr_end].copy_from_slice(&0u32.to_le_bytes());
                if fs
                    .block_device()
                    .write_bytes(Bid::new(parent_bid as u64).to_offset(), &parent_buf)
                    .is_err()
                {
                    return_errno_with_message!(Errno::EIO, "failed to detach shared branch");
                }
            }

            // Recursively free the detached subtree.
            if detached_nr != 0 {
                // SPEC: free detached subtree root.
                let subtree_depth = (path.depth - 1).saturating_sub(partial) as u32;
                self.free_branches(&fs, detached_nr, subtree_depth);
            }

            // --- Step 5: Clear right side of partially shared indirect blocks ---
            // SPEC: clear right side of each partially shared indirect block.
            // For each level from partial down to 1, free all pointers to the right
            // of offsets[level].
            for level in (1..=partial).rev() {
                let parent_bid = branch
                    .chain
                    .get(level - 1)
                    .map(|entry| entry.key)
                    .unwrap_or(0);
                if parent_bid == 0 {
                    return_errno_with_message!(Errno::EIO, "invalid indirect block number on tail");
                }

                // Re-read the current indirect block state to avoid stale-buffer
                // overwrite after the detach step above.
                let mut buf = vec![0u8; block_size];
                if fs
                    .block_device()
                    .read_bytes(Bid::new(parent_bid as u64).to_offset(), &mut buf)
                    .is_err()
                {
                    return_errno_with_message!(
                        Errno::EIO,
                        "failed to read indirect block for truncation tail"
                    );
                }

                // Free all pointers from offsets[level]+1 to the end.
                let start_idx = (path.offsets[level] as usize).saturating_add(1);
                let child_depth = (path.depth - 1).saturating_sub(level) as u32;
                for idx in start_idx..ptrs_per_block {
                    let ptr_offset = idx.saturating_mul(size_of::<u32>());
                    let ptr_end = ptr_offset.saturating_add(size_of::<u32>());
                    if ptr_end > buf.len() {
                        break;
                    }
                    let nr = u32::from_le_bytes([
                        buf[ptr_offset],
                        buf[ptr_offset + 1],
                        buf[ptr_offset + 2],
                        buf[ptr_offset + 3],
                    ]);
                    if nr == 0 {
                        continue;
                    }
                    buf[ptr_offset..ptr_end].copy_from_slice(&0u32.to_le_bytes());
                    self.free_branches(&fs, nr, child_depth);
                }

                // Write back the modified indirect block.
                if fs
                    .block_device()
                    .write_bytes(Bid::new(parent_bid as u64).to_offset(), &buf)
                    .is_err()
                {
                    return_errno_with_message!(
                        Errno::EIO,
                        "failed to persist partial indirect truncation"
                    );
                }
            }
        }

        // === Step 6: Free complete indirect block trees ===
        // Linux: do_indirects switch/fallthrough by offsets[0].
        // If truncation point is in direct blocks, free all indirect trees.
        // If in single indirect, free double and triple indirect trees, etc.
        if path.offsets[0] < 12 {
            // Truncation in direct blocks: free single, double, triple indirect.
            let nr = self.desc.block_ptrs[12];
            if nr != 0 {
                self.desc.block_ptrs[12] = 0;
                self.free_branches(&fs, nr, 1);
            }
        }
        if path.offsets[0] <= 12 {
            // Truncation in direct or single indirect: free double, triple indirect.
            let nr = self.desc.block_ptrs[13];
            if nr != 0 {
                self.desc.block_ptrs[13] = 0;
                self.free_branches(&fs, nr, 2);
            }
        }
        if path.offsets[0] <= 13 {
            // Truncation in direct, single, or double indirect: free triple indirect.
            let nr = self.desc.block_ptrs[14];
            if nr != 0 {
                self.desc.block_ptrs[14] = 0;
                self.free_branches(&fs, nr, 3);
            }
        }

        Ok(())
    }

    /// Recursively frees an indirect branch.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1136 (ext2_free_branches)
    /// Linux: /root/linux/fs/ext2/inode.c:1096 (ext2_free_data)
    fn free_branches(&mut self, fs: &Ext2, block_nr: u32, depth: u32) {
        if block_nr == 0 {
            return;
        }

        let block_size = fs.block_size();
        let sectors_per_block = (block_size / SECTOR_SIZE) as u32;
        if sectors_per_block == 0 {
            error!(
                "ext2: free_branches: invalid sector accounting for block size {}",
                block_size
            );
            return;
        }

        if depth == 0 {
            if let Err(err) = fs.free_blocks(block_nr, 1) {
                // SPEC: best-effort free path logs errors and proceeds.
                error!(
                    "ext2: free_branches: failed to free data block {}: {:?}",
                    block_nr, err
                );
                return;
            }
            self.desc.blocks = self.desc.blocks.saturating_sub(sectors_per_block);
            return;
        }

        let ptrs_per_block = block_size / size_of::<u32>();
        if ptrs_per_block == 0 {
            error!(
                "ext2: free_branches: invalid indirect fanout for block size {}",
                block_size
            );
            return;
        }

        let mut buf = vec![0u8; block_size];
        if fs
            .block_device()
            .read_bytes(Bid::new(block_nr as u64).to_offset(), &mut buf)
            .is_err()
        {
            // Linux ext2_free_branches logs read failure and skips that branch.
            error!(
                "ext2: free_branches: failed to read indirect block {} (depth {})",
                block_nr, depth
            );
            return;
        }

        for idx in 0..ptrs_per_block {
            let ptr_offset = idx.saturating_mul(size_of::<u32>());
            let ptr_end = ptr_offset.saturating_add(size_of::<u32>());
            if ptr_end > buf.len() {
                break;
            }

            let nr = u32::from_le_bytes([
                buf[ptr_offset],
                buf[ptr_offset + 1],
                buf[ptr_offset + 2],
                buf[ptr_offset + 3],
            ]);
            if nr == 0 {
                continue;
            }
            self.free_branches(fs, nr, depth.saturating_sub(1));
        }

        if let Err(err) = fs.free_blocks(block_nr, 1) {
            error!(
                "ext2: free_branches: failed to free indirect block {}: {:?}",
                block_nr, err
            );
            return;
        }
        self.desc.blocks = self.desc.blocks.saturating_sub(sectors_per_block);
    }

    /// Initializes a newly allocated directory inode with `.` and `..` entries.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:617 (ext2_make_empty)
    // pub(super) fn make_empty(&mut self, parent_ino: u32) -> Result<()> {
    //     if self.desc.type_ != InodeType::Dir {
    //         return_errno!(Errno::ENOTDIR);
    //     }

    //     let fs = self
    //         .fs
    //         .upgrade()
    //         .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
    //     let total_inodes = fs.super_block().total_inodes();
    //     if parent_ino == 0 || parent_ino > total_inodes {
    //         return_errno_with_message!(Errno::EINVAL, "parent inode number out of range");
    //     }

    //     let self_ino = self
    //         .weak_self
    //         .upgrade()
    //         .ok_or_else(|| Error::with_message(Errno::EIO, "inode already dropped"))?
    //         .ino();

    //     let chunk_size = fs.block_size();
    //     let sectors_per_block = (chunk_size / SECTOR_SIZE) as u32;

    //     // SPEC: allocate exactly one data block for the first directory chunk.
    //     let allocated = fs.alloc_blocks(1)?;
    //     if allocated.end != allocated.start.saturating_add(1) {
    //         return_errno_with_message!(Errno::EIO, "unexpected multi-block allocation");
    //     }
    //     let new_bid = allocated.start;

    //     // Preserve old state for rollback.
    //     let old_ptr0 = self.desc.block_ptrs[0];
    //     let old_size = self.desc.size;
    //     let old_blocks = self.desc.blocks;

    //     if old_ptr0 != 0 {
    //         let _ = fs.free_blocks(new_bid, 1);
    //         return_errno_with_message!(Errno::EIO, "dir block pointer already occupied");
    //     }
    //     self.desc.block_ptrs[0] = new_bid;

    //     let mut buf = vec![0u8; chunk_size];
    //     // SPEC: zero-filled chunk and canonical `.`/`..` layout.
    //     Self::write_dir_entry_bytes(
    //         &mut buf,
    //         0,
    //         self_ino,
    //         DirEntry::dir_rec_len(1),
    //         b".",
    //         DirEntryFileType::Dir as u8,
    //     )?;
    //     let dot_len = DirEntry::dir_rec_len(1) as usize;
    //     let dotdot_len = (chunk_size.saturating_sub(dot_len)) as u16;
    //     Self::write_dir_entry_bytes(
    //         &mut buf,
    //         dot_len,
    //         parent_ino,
    //         dotdot_len,
    //         b"..",
    //         DirEntryFileType::Dir as u8,
    //     )?;

    //     if fs
    //         .block_device()
    //         .write_bytes(Bid::new(new_bid as u64).to_offset(), &buf)
    //         .is_err()
    //     {
    //         self.desc.block_ptrs[0] = old_ptr0;
    //         let _ = fs.free_blocks(new_bid, 1);
    //         return_errno_with_message!(Errno::EIO, "failed to write initial dir block");
    //     }

    //     self.desc.size = chunk_size as u64;
    //     self.desc.blocks = self
    //         .desc
    //         .blocks
    //         .checked_add(sectors_per_block)
    //         .ok_or_else(|| Error::with_message(Errno::EIO, "inode block count overflow"))?;

    //     if let Err(err) = self.persist_inode_and_sync(&fs) {
    //         // SPEC: cleanup allocation and restore pre-state if persistence failed.
    //         self.desc.block_ptrs[0] = old_ptr0;
    //         self.desc.size = old_size;
    //         self.desc.blocks = old_blocks;
    //         let _ = fs.free_blocks(new_bid, 1);
    //         return Err(err);
    //     }

    //     Ok(())
    // }

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
            let mut buf = vec![0u8; block_size];
            let block_offset = block_idx.saturating_mul(block_size);
            if self
                .page_cache
                .pages()
                .read_bytes(block_offset, &mut buf)
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
    // pub(super) fn mkdir(&mut self, name: &str, perm: FilePerm) -> Result<Arc<Inode>> {
    //     if self.desc.type_ != InodeType::Dir {
    //         return_errno!(Errno::ENOTDIR);
    //     }

    //     let name_bytes = name.as_bytes();
    //     if name_bytes.is_empty()
    //         || name_bytes.len() > u8::MAX as usize
    //         || name_bytes == b"."
    //         || name_bytes == b".."
    //     {
    //         return_errno!(Errno::EINVAL);
    //     }

    //     let fs = self
    //         .fs
    //         .upgrade()
    //         .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
    //     let parent_ino = self
    //         .weak_self
    //         .upgrade()
    //         .ok_or_else(|| Error::with_message(Errno::EIO, "inode already dropped"))?
    //         .ino();

    //     // SPEC: reserve parent link for new subdir's `..`.
    //     self.desc.links_count = self.desc.links_count.saturating_add(1);

    //     let child = match fs.create_inode(parent_ino, InodeType::Dir, perm) {
    //         Ok(child) => child,
    //         Err(err) => {
    //             // SPEC: rollback parent link reservation on failure.
    //             self.desc.links_count = self.desc.links_count.saturating_sub(1);
    //             return Err(err);
    //         }
    //     };
    //     let child_ino = child.ino();

    //     {
    //         let mut child_inner = child.inner.write();
    //         if let Err(err) = child_inner.make_empty(parent_ino) {
    //             let _ = child_inner.release_dir_data_blocks_for_cleanup(&fs);
    //             let _ = fs.free_inode(child_ino);
    //             self.desc.links_count = self.desc.links_count.saturating_sub(1);
    //             return Err(err);
    //         }
    //     }

    //     if let Err(err) = self.add_entry(name, child_ino, DirEntryFileType::Dir) {
    //         {
    //             let mut child_inner = child.inner.write();
    //             let _ = child_inner.release_dir_data_blocks_for_cleanup(&fs);
    //         }
    //         let _ = fs.free_inode(child_ino);
    //         self.desc.links_count = self.desc.links_count.saturating_sub(1);
    //         return Err(err);
    //     }

    //     // SPEC: persist parent link count update.
    //     if let Err(err) = self.persist_inode_and_sync(&fs) {
    //         let _ = self.delete_entry(name);
    //         {
    //             let mut child_inner = child.inner.write();
    //             let _ = child_inner.release_dir_data_blocks_for_cleanup(&fs);
    //         }
    //         let _ = fs.free_inode(child_ino);
    //         self.desc.links_count = self.desc.links_count.saturating_sub(1);
    //         return Err(err);
    //     }

    //     Ok(child)
    // }

    /// Removes an existing empty subdirectory.
    ///
    /// Linux: /root/linux/fs/ext2/namei.c:302 (ext2_rmdir)
    // pub(super) fn rmdir(&mut self, name: &str) -> Result<()> {
    //     if self.desc.type_ != InodeType::Dir {
    //         return_errno!(Errno::ENOTDIR);
    //     }

    //     let name_bytes = name.as_bytes();
    //     if name_bytes.is_empty()
    //         || name_bytes.len() > u8::MAX as usize
    //         || name_bytes == b"."
    //         || name_bytes == b".."
    //     {
    //         return_errno!(Errno::EINVAL);
    //     }

    //     let fs = self
    //         .fs
    //         .upgrade()
    //         .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
    //     let child_ino = self.find_entry(name)?;
    //     let child = fs.read_inode(child_ino)?;

    //     {
    //         let mut child_inner = child.inner.write();
    //         if child_inner.desc.type_ != InodeType::Dir {
    //             return_errno!(Errno::ENOTDIR);
    //         }
    //         if !child_inner.empty_dir() {
    //             return_errno!(Errno::ENOTEMPTY);
    //         }

    //         self.delete_entry(name)?;

    //         child_inner.release_dir_data_blocks_for_cleanup(&fs)?;
    //         child_inner.desc.size = 0;
    //         child_inner.desc.links_count = child_inner.desc.links_count.saturating_sub(2);
    //         child_inner.persist_inode_and_sync(&fs)?;
    //     }

    //     self.desc.links_count = self.desc.links_count.saturating_sub(1);
    //     self.persist_inode_and_sync(&fs)?;

    //     fs.free_inode(child_ino)
    // }

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
        let mut block_idx = 0usize;

        while (block_idx as u64).saturating_mul(block_size as u64) < size {
            let block_offset = block_idx.saturating_mul(block_size);
            let remain = (size as usize).saturating_sub(block_offset);
            let limit = remain.min(block_size);
            if limit == 0 {
                break;
            }

            let mut buf = vec![0u8; block_size];
            if self
                .page_cache
                .pages()
                .read_bytes(block_offset, &mut buf)
                .is_err()
            {
                return_errno_with_message!(Errno::EIO, "failed to read dir block via page cache");
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

            let mut buf = vec![0u8; block_size];
            if self
                .page_cache
                .pages()
                .read_bytes(block_offset, &mut buf)
                .is_err()
            {
                return_errno_with_message!(Errno::EIO, "failed to read dir block via page cache");
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

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let branch = self.get_branch(&path, &fs)?;
        if branch.partial_level < path.depth {
            return Ok(None);
        }

        let bid = branch
            .chain
            .get(path.depth.saturating_sub(1))
            .ok_or_else(|| Error::with_message(Errno::EIO, "incomplete branch result"))?
            .key;
        if bid == 0 {
            return Ok(None);
        }
        Ok(Some(Bid::new(bid as u64)))
    }

    /// Traverses the existing block pointer chain for a block path.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:234 (ext2_get_branch)
    fn get_branch(&self, path: &BlockPath, fs: &Ext2) -> Result<BranchResult> {
        if path.depth == 0 || path.depth > path.offsets.len() {
            return_errno_with_message!(Errno::EIO, "invalid block path depth");
        }

        let top_offset = path.offsets[0] as usize;
        let top_key =
            *self.desc.block_ptrs.get(top_offset).ok_or_else(|| {
                Error::with_message(Errno::EIO, "invalid top-level block pointer")
            })?;

        let mut chain = Vec::with_capacity(path.depth);
        chain.push(IndirectEntry {
            key: top_key,
            bh: None,
        });
        if top_key == 0 {
            // SPEC: zero pointer means the chain is broken at level 0.
            return Ok(BranchResult {
                partial_level: 0,
                chain,
            });
        }

        let block_size = fs.block_size();
        if block_size < size_of::<u32>() {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        // DIFF from Linux ext2_get_branch: no verify_chain/-EAGAIN retry loop.
        // Asterinas callers hold InodeInner locks, so chain pointers are stable.
        for level in 1..path.depth {
            let parent_key = chain[level - 1].key;
            let mut buf = vec![0u8; block_size];
            if fs
                .block_device()
                .read_bytes(Bid::new(parent_key as u64).to_offset(), &mut buf)
                .is_err()
            {
                // SPEC: indirect block read failure must return EIO.
                return_errno_with_message!(Errno::EIO, "failed to read indirect block");
            }

            let ptr_offset = (path.offsets[level] as usize).saturating_mul(size_of::<u32>());
            let ptr_end = ptr_offset.saturating_add(size_of::<u32>());
            if ptr_end > buf.len() {
                return_errno_with_message!(Errno::EIO, "indirect pointer offset out of bounds");
            }

            let next_key = u32::from_le_bytes([
                buf[ptr_offset],
                buf[ptr_offset + 1],
                buf[ptr_offset + 2],
                buf[ptr_offset + 3],
            ]);
            chain.push(IndirectEntry {
                key: next_key,
                bh: Some(buf),
            });
            if next_key == 0 {
                // SPEC: include the zero-key entry and report break level.
                return Ok(BranchResult {
                    partial_level: level,
                    chain,
                });
            }
        }

        Ok(BranchResult {
            partial_level: path.depth,
            chain,
        })
    }

    /// Counts metadata/data blocks needed to complete a broken chain.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:361 (ext2_blks_to_allocate)
    ///
    /// DIFF from Linux: `data_blks` is always 1. Linux uses `boundary` and
    /// `maxblocks` to batch-allocate multiple contiguous data blocks in a
    /// single call, avoiding repeated `get_block` round-trips. This
    /// implementation allocates one data block at a time; multi-block
    /// contiguous allocation can be added later using `BlockPath::boundary`.
    fn blks_to_allocate(&self, branch: &BranchResult, path: &BlockPath) -> (u32, u32) {
        let indirect_blks = path
            .depth
            .saturating_sub(1)
            .saturating_sub(branch.partial_level) as u32;
        (indirect_blks, 1)
    }

    /// Allocates a missing branch and splices it into the inode block tree.
    ///
    /// The function proceeds in two phases for crash safety:
    ///
    /// Phase 1 — Build the new chain (alloc_branch):
    ///   Allocate `indirect_blks` metadata blocks + `data_blks` data blocks,
    ///   zero-fill each new indirect block, write the next-level pointer into it,
    ///   and flush to disk. After this phase the new chain is fully formed on disk
    ///   but unreachable — nothing in the existing tree points to it yet.
    ///
    /// Phase 2 — Splice into the tree (splice_branch):
    ///   Write `new_blocks[0]` (the chain head) into the break point:
    ///   - If the break is at level 0, write directly into `inode.i_block[]`.
    ///   - Otherwise, patch the cached indirect block at the break point and
    ///     flush it back to disk.
    ///   This single pointer write atomically makes the entire new chain visible.
    ///
    /// Crash safety: if a crash occurs during phase 1, the new blocks are orphaned
    /// (reclaimable by fsck) but the tree remains consistent. Only after phase 2
    /// completes does the new chain become reachable.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:479 (ext2_alloc_branch)
    /// Linux: /root/linux/fs/ext2/inode.c:561 (ext2_splice_branch)
    fn alloc_and_splice_branch(
        &mut self,
        fs: &Ext2,
        indirect_blks: u32,
        data_blks: u32,
        path: &BlockPath,
        branch: &BranchResult,
    ) -> Result<Bid> {
        if data_blks == 0 {
            return_errno_with_message!(Errno::EIO, "invalid zero data allocation");
        }
        if branch.partial_level >= path.depth {
            return_errno_with_message!(Errno::EIO, "branch is already complete");
        }

        let total = indirect_blks
            .checked_add(data_blks)
            .ok_or_else(|| Error::with_message(Errno::EIO, "block allocation count overflow"))?;
        if total == 0 {
            return_errno_with_message!(Errno::EIO, "invalid zero total allocation");
        }

        let block_size = fs.block_size();
        if block_size < size_of::<u32>() {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        let sectors_per_block = (block_size / SECTOR_SIZE) as u32;
        if sectors_per_block == 0 {
            return_errno_with_message!(Errno::EIO, "invalid sector accounting for block size");
        }
        let added_sectors = total
            .checked_mul(sectors_per_block)
            .ok_or_else(|| Error::with_message(Errno::EIO, "inode block accounting overflow"))?;
        let new_block_count = self
            .desc
            .blocks
            .checked_add(added_sectors)
            .ok_or_else(|| Error::with_message(Errno::EIO, "inode block count overflow"))?;

        let mut new_blocks = Vec::with_capacity(total as usize);
        // Rollback helper: frees all blocks allocated so far.
        let free_all = |blocks: &Vec<u32>| {
            for bid in blocks {
                let _ = fs.free_blocks(*bid, 1);
            }
        };

        while (new_blocks.len() as u32) < total {
            let remain = total - new_blocks.len() as u32;
            let allocated = match fs.alloc_blocks(remain) {
                Ok(allocated) => allocated,
                Err(err) => {
                    free_all(&new_blocks);
                    return Err(err);
                }
            };

            let alloc_len = allocated.end.saturating_sub(allocated.start);
            if alloc_len == 0 || alloc_len > remain {
                free_all(&new_blocks);
                return_errno_with_message!(Errno::EIO, "invalid block allocation result");
            }

            new_blocks.extend(allocated);
        }

        // SPEC: data block is at index `indirect_blks` in allocation order.
        let data_block = match new_blocks.get(indirect_blks as usize) {
            Some(bid) => *bid,
            None => {
                free_all(&new_blocks);
                return_errno_with_message!(Errno::EIO, "allocated chain missing data block");
            }
        };

        // Phase 1: Build the new chain — zero-fill each indirect block, write
        // the next-level pointer, and flush to disk. The chain is fully formed
        // but still unreachable from the existing tree.
        for i in 0..(indirect_blks as usize) {
            let level = branch.partial_level + 1 + i;
            if level >= path.depth {
                free_all(&new_blocks);
                return_errno_with_message!(Errno::EIO, "invalid branch depth during allocation");
            }

            let ptr_offset = (path.offsets[level] as usize).saturating_mul(size_of::<u32>());
            let ptr_end = ptr_offset.saturating_add(size_of::<u32>());
            if ptr_end > block_size {
                free_all(&new_blocks);
                return_errno_with_message!(Errno::EIO, "indirect pointer offset out of bounds");
            }

            let next_block = match new_blocks.get(i + 1) {
                Some(bid) => *bid,
                None => {
                    free_all(&new_blocks);
                    return_errno_with_message!(Errno::EIO, "allocated chain metadata mismatch");
                }
            };

            let mut buf = vec![0u8; block_size];
            buf[ptr_offset..ptr_end].copy_from_slice(&next_block.to_le_bytes());
            if fs
                .block_device()
                .write_bytes(Bid::new(new_blocks[i] as u64).to_offset(), &buf)
                .is_err()
            {
                free_all(&new_blocks);
                return_errno_with_message!(Errno::EIO, "failed to write new indirect block");
            }
        }

        // Phase 2: Splice — write the chain head into the break point, making
        // the entire new chain reachable in one pointer write.
        let splice_ptr = new_blocks[0];
        if branch.partial_level == 0 {
            let slot = path.offsets[0] as usize;
            if slot >= self.desc.block_ptrs.len() {
                free_all(&new_blocks);
                return_errno_with_message!(Errno::EIO, "invalid inode block pointer slot");
            }
            if self.desc.block_ptrs[slot] != 0 {
                free_all(&new_blocks);
                return_errno_with_message!(Errno::EIO, "block pointer changed during allocation");
            }

            // SPEC: splice directly into inode i_block[].
            self.desc.block_ptrs[slot] = splice_ptr;
        } else {
            let parent_entry_level = branch.partial_level;
            let mut parent_buf = match branch
                .chain
                .get(parent_entry_level)
                .and_then(|entry| entry.bh.clone())
            {
                Some(buf) => buf,
                None => {
                    free_all(&new_blocks);
                    return_errno_with_message!(
                        Errno::EIO,
                        "missing parent indirect block for splice"
                    );
                }
            };

            let parent_bid = branch
                .chain
                .get(parent_entry_level.saturating_sub(1))
                .map(|entry| entry.key)
                .unwrap_or(0);
            if parent_bid == 0 {
                free_all(&new_blocks);
                return_errno_with_message!(Errno::EIO, "invalid parent indirect block number");
            }

            let ptr_offset =
                (path.offsets[parent_entry_level] as usize).saturating_mul(size_of::<u32>());
            let ptr_end = ptr_offset.saturating_add(size_of::<u32>());
            if ptr_end > parent_buf.len() {
                free_all(&new_blocks);
                return_errno_with_message!(Errno::EIO, "splice offset out of bounds");
            }
            parent_buf[ptr_offset..ptr_end].copy_from_slice(&splice_ptr.to_le_bytes());

            if fs
                .block_device()
                .write_bytes(Bid::new(parent_bid as u64).to_offset(), &parent_buf)
                .is_err()
            {
                free_all(&new_blocks);
                return_errno_with_message!(Errno::EIO, "failed to splice branch into parent");
            }
        }

        // SPEC: ext2_splice_branch-style inode accounting and ctime update.
        self.desc.blocks = new_block_count;
        self.desc.ctime = now();

        Ok(Bid::new(data_block as u64))
    }

    /// Resolves a logical block to physical, allocating a missing branch if requested.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:624 (ext2_get_blocks, create path)
    pub(super) fn get_or_alloc_block(&mut self, iblock: u32, create: bool) -> Result<Option<Bid>> {
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let path = self.block_to_path(iblock)?;
        if path.depth == 0 {
            return_errno_with_message!(Errno::EIO, "invalid block path depth");
        }

        let branch = self.get_branch(&path, &fs)?;
        if branch.partial_level == path.depth {
            let mapped = branch
                .chain
                .get(path.depth.saturating_sub(1))
                .ok_or_else(|| Error::with_message(Errno::EIO, "incomplete branch result"))?
                .key;
            return Ok(Some(Bid::new(mapped as u64)));
        }
        if !create {
            return Ok(None);
        }

        let (indirect_blks, data_blks) = self.blks_to_allocate(&branch, &path);
        let bid = self.alloc_and_splice_branch(&fs, indirect_blks, data_blks, &path, &branch)?;
        Ok(Some(bid))
    }

    /// Phase 1: scan directory blocks for reusable slot or duplicate.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:476 (ext2_add_link scan loop)
    fn scan_dir_for_slot(&self, name: &str, fs: &Ext2) -> Result<DirScanResult> {
        if self.desc.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > u8::MAX as usize {
            return_errno!(Errno::EINVAL);
        }

        let block_size = fs.block_size();
        let reclen = DirEntry::dir_rec_len(name_bytes.len()) as usize;
        if reclen > block_size {
            return_errno_with_message!(Errno::ENOSPC, "dir entry too large for block");
        }

        let size = self.desc.size as usize;
        let max_inumber = fs.super_block().total_inodes();
        let data_blocks = size.div_ceil(block_size);

        for block_idx in 0..data_blocks {
            let block_offset = block_idx.saturating_mul(block_size);
            let limit = size.saturating_sub(block_offset).min(block_size);
            if limit == 0 {
                continue;
            }

            let mut buf = vec![0u8; block_size];
            // SPEC: directory scan reads through inode page cache.
            self.page_cache.pages().read_bytes(block_offset, &mut buf)?;

            let entries = Self::collect_dir_entries_with_offsets(&buf, limit, max_inumber)?;
            for (entry_offset, entry) in entries {
                if entry.inode != 0
                    && entry.name_len as usize == name_bytes.len()
                    && entry.name.as_bytes() == name_bytes
                {
                    // SPEC: duplicate names fail with EEXIST.
                    return_errno!(Errno::EEXIST);
                }

                let rec_len = entry.rec_len as usize;
                let used_len = if entry.inode == 0 {
                    0
                } else {
                    DirEntry::dir_rec_len(entry.name_len as usize) as usize
                };

                // SPEC: free entry can be reused, occupied entry can be split.
                if (entry.inode == 0 && rec_len >= reclen)
                    || (entry.inode != 0 && rec_len >= used_len.saturating_add(reclen))
                {
                    return Ok(DirScanResult::Slot(DirSlotInfo {
                        dir_offset: block_offset + entry_offset,
                        slot_rec_len: rec_len,
                        used_rec_len: used_len,
                    }));
                }
            }
        }

        Ok(DirScanResult::NeedGrowth)
    }

    /// Phase 2: grow directory by one data block.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:476 (ext2_add_link growth path)
    fn grow_dir_block(&mut self, fs: &Ext2) -> Result<DirSlotInfo> {
        let block_size = fs.block_size();
        let old_size = self.desc.size as usize;
        let old_blocks = self.desc.blocks;
        let data_blocks = old_size.div_ceil(block_size);
        let growth_iblock = u32::try_from(data_blocks)
            .map_err(|_| Error::with_message(Errno::EINVAL, "directory block index overflow"))?;

        // SPEC: allocation under write lock; no PageCache I/O.
        self.get_or_alloc_block(growth_iblock, true)?
            .ok_or_else(|| Error::with_message(Errno::ENOSPC, "failed to grow directory block"))?;

        let new_size = old_size.saturating_add(block_size);
        self.desc.size = new_size as u64;
        if let Err(err) = self.page_cache.resize(new_size) {
            // SPEC: rollback allocated growth on resize failure.
            self.page_cache.discard_range(old_size..new_size);
            self.desc.size = old_size as u64;
            self.desc.blocks = old_blocks;
            self.truncate_blocks(old_size)?;
            return Err(err);
        }

        Ok(DirSlotInfo {
            dir_offset: old_size,
            slot_rec_len: block_size,
            used_rec_len: 0,
        })
    }

    /// Phase 3: write a new entry into a selected slot via PageCache.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:476 (ext2_add_link commit)
    fn write_dir_entry_to_cache(
        &self,
        slot: &DirSlotInfo,
        name: &str,
        ino: u32,
        ft: u8,
    ) -> Result<()> {
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let max_inumber = fs.super_block().total_inodes();
        if ino == 0 || ino > max_inumber {
            return_errno!(Errno::EINVAL);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > u8::MAX as usize {
            return_errno!(Errno::EINVAL);
        }

        let entry_reclen = DirEntry::dir_rec_len(name_bytes.len()) as usize;
        if entry_reclen > slot.slot_rec_len {
            return_errno_with_message!(Errno::ENOSPC, "slot too small for dir entry");
        }

        let mut offset = slot.dir_offset;
        let mut rec_len = slot.slot_rec_len;
        if slot.used_rec_len != 0 {
            if slot.used_rec_len >= slot.slot_rec_len {
                return_errno_with_message!(Errno::EIO, "corrupted dir entry split");
            }
            // SPEC: when splitting, commit predecessor rec_len before writing new entry.
            let split_len = (slot.used_rec_len as u16).to_le_bytes();
            self.page_cache
                .pages()
                .write_bytes(slot.dir_offset.saturating_add(4), &split_len)?;
            offset = slot.dir_offset.saturating_add(slot.used_rec_len);
            rec_len = slot.slot_rec_len.saturating_sub(slot.used_rec_len);
        }

        let mut entry_buf = vec![0u8; rec_len];
        Self::write_dir_entry_bytes(&mut entry_buf, 0, ino, rec_len as u16, name_bytes, ft)?;
        self.page_cache.pages().write_bytes(offset, &entry_buf)?;
        Ok(())
    }

    /// Phase 4: update directory ctime/mtime and persist inode.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:84 (ext2_commit_chunk)
    fn commit_dir_metadata(&mut self, fs: &Ext2) -> Result<()> {
        self.update_dir_timestamps_and_flags()?;
        self.persist_inode_and_sync(fs)
    }

    /// Locate a target entry by name for delete/set_link operations.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:342 (ext2_find_entry)
    fn find_entry_target(&self, name: &str) -> Result<DirEntryTarget> {
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let max_inumber = fs.super_block().total_inodes();
        let block_size = fs.block_size();
        let size = self.desc.size as usize;
        let name_bytes = name.as_bytes();

        for block_idx in 0..size.div_ceil(block_size) {
            let block_offset = block_idx.saturating_mul(block_size);
            let limit = size.saturating_sub(block_offset).min(block_size);
            if limit == 0 {
                continue;
            }

            let mut buf = vec![0u8; block_size];
            self.page_cache.pages().read_bytes(block_offset, &mut buf)?;

            let entries = Self::collect_dir_entries_with_offsets(&buf, limit, max_inumber)?;
            for (entry_offset, entry) in entries {
                if entry.inode == 0 {
                    continue;
                }
                if entry.name_len as usize != name_bytes.len() {
                    continue;
                }
                if entry.name.as_bytes() == name_bytes {
                    return Ok(DirEntryTarget {
                        dir_offset: block_offset + entry_offset,
                        entry_rec_len: entry.rec_len as usize,
                    });
                }
            }
        }

        return_errno!(Errno::ENOENT)
    }

    /// Delete a located entry by zeroing inode and merging rec_len.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:560 (ext2_delete_entry)
    fn delete_entry_in_cache(&self, target: &DirEntryTarget) -> Result<()> {
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let block_size = fs.block_size();
        let block_base = (target.dir_offset / block_size).saturating_mul(block_size);
        let entry_offset = target.dir_offset.saturating_sub(block_base);
        let limit = (self.desc.size as usize)
            .saturating_sub(block_base)
            .min(block_size);

        let mut block_buf = vec![0u8; block_size];
        self.page_cache
            .pages()
            .read_bytes(block_base, &mut block_buf)?;
        Self::delete_entry_in_block(
            &mut block_buf,
            limit,
            block_size,
            entry_offset,
            target.entry_rec_len,
        )?;
        self.page_cache
            .pages()
            .write_bytes(block_base, &block_buf)?;
        Ok(())
    }

    /// Rewrite a located entry's inode/type via PageCache.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:450 (ext2_set_link)
    fn set_link_in_cache(&self, target: &DirEntryTarget, new_ino: u32, ft: u8) -> Result<()> {
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
        let block_size = fs.block_size();
        let block_base = (target.dir_offset / block_size).saturating_mul(block_size);
        let entry_offset = target.dir_offset.saturating_sub(block_base);

        let mut block_buf = vec![0u8; block_size];
        self.page_cache
            .pages()
            .read_bytes(block_base, &mut block_buf)?;
        Self::write_inode_number(&mut block_buf, entry_offset, new_ino)?;
        if entry_offset.saturating_add(size_of::<RawDirEntry>()) > block_buf.len() {
            return_errno_with_message!(Errno::EIO, "dir entry header out of bounds");
        }
        block_buf[entry_offset + 7] = ft;
        self.page_cache
            .pages()
            .write_bytes(block_base, &block_buf)?;
        Ok(())
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
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;

        // SPEC: ext2_add_link-style scan then growth if needed.
        let slot = match self.scan_dir_for_slot(name, &fs)? {
            DirScanResult::Slot(slot) => slot,
            DirScanResult::NeedGrowth => self.grow_dir_block(&fs)?,
        };
        self.write_dir_entry_to_cache(&slot, name, ino, file_type as u8)?;
        self.commit_dir_metadata(&fs)
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

        let target = self.find_entry_target(name)?;
        self.set_link_in_cache(&target, new_ino, file_type as u8)?;

        if update_times {
            self.commit_dir_metadata(&fs)
        } else {
            self.desc.flags.remove(FileFlags::INDEX_DIR);
            self.persist_inode_and_sync(&fs)
        }
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
        let target = self.find_entry_target(name).map_err(|err| {
            if err.error() == Errno::ENOENT {
                Error::with_message(Errno::EIO, "dir entry not found for delete")
            } else {
                err
            }
        })?;
        self.delete_entry_in_cache(&target)?;
        self.commit_dir_metadata(&fs)
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

    fn release_dir_data_blocks_for_cleanup(&mut self, _fs: &Ext2) -> Result<()> {
        // DIFF from Linux:
        // Linux mkdir-failure/rmdir cleanup reaches block release through
        // discard_new_inode()/iput() -> ext2_evict_inode() -> ext2_truncate_blocks().
        // Asterinas currently has no unified inode evict+truncate path, so we
        // explicitly trigger truncate-based release on rollback/removal paths.
        // TODO: Move this logic into a shared truncate/evict pipeline, and make
        // free_inode trigger it instead of per-call-site cleanup.
        // SPEC: delegate to full indirect-tree truncation path.
        self.truncate_blocks(0)?;
        // SPEC: cleanup path must leave directory size at zero.
        self.desc.size = 0;
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
            return_errno!(Errno::EINVAL);
        }

        // SPEC: Phase 6.3 supports File and Dir only.
        if type_ != InodeType::File && type_ != InodeType::Dir {
            return_errno!(Errno::EINVAL);
        }

        if type_ == InodeType::Dir {
            return self.mkdir(name, perm);
        } else {
            // Linux: ext2_create → ext2_new_inode + ext2_add_nondir
            let fs = self
                .fs
                .upgrade()
                .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))?;
            let child = fs.create_inode(self.ino, type_, perm)?;
            let child_ino = child.ino();
            let dir_ft = Self::inode_type_to_dir_file_type(type_);

            if let Err(err) = self.add_entry(name, child_ino, dir_ft) {
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
    pub(super) fn link(&self, old: &Inode, name: &str) -> Result<()> {
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

        let dir_ft = Self::inode_type_to_dir_file_type(old.type_);

        // Linux: inode_set_ctime_current + inode_inc_link_count before add_link.
        {
            let mut old_inner = old.inner.write();
            old_inner.desc.ctime = now();
            old_inner.desc.links_count = old_inner.desc.links_count.saturating_add(1);
        }

        if let Err(err) = self.add_entry(name, old.ino, dir_ft) {
            // SPEC: rollback link count on add_entry failure.
            let mut old_inner = old.inner.write();
            old_inner.desc.links_count = old_inner.desc.links_count.saturating_sub(1);
            return Err(err);
        }

        old.inner.write().persist_inode_and_sync(&fs)?;
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

        let child_ino = self.inner.read().find_entry(name)?;
        let child = fs.read_inode(child_ino)?;

        // SPEC: unlink rejects directories — use rmdir instead.
        if child.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        // Delete the directory entry first.
        self.delete_entry(name)?;

        // Linux: inode_set_ctime_to_ts(inode, inode_get_ctime(dir))
        // then inode_dec_link_count.
        let mut child_inner = child.inner.write();
        child_inner.desc.ctime = now();
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
    pub(super) fn rename(&self, old_name: &str, target: &Inode, new_name: &str) -> Result<()> {
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

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > u8::MAX as usize || name_bytes == b"." {
            return_errno!(Errno::EINVAL);
        }

        let fs = self.fs_arc()?;
        let max_inumber = fs.super_block().total_inodes();
        if new_ino < ROOT_INO || new_ino > max_inumber {
            return_errno!(Errno::EINVAL);
        }

        // SPEC: run PageCache locate+update under upread, then metadata under write.
        let inner = self.inner.upread();
        let target = inner.find_entry_target(name)?;
        inner.set_link_in_cache(&target, new_ino, file_type as u8)?;

        let mut write_inner = inner.upgrade();
        if update_times {
            write_inner.commit_dir_metadata(&fs)
        } else {
            write_inner.desc.flags.remove(FileFlags::INDEX_DIR);
            write_inner.persist_inode_and_sync(&fs)
        }
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
///
/// Produced by `block_to_path` from a logical block number.
/// Linux analogue: output of `ext2_block_to_path` in `fs/ext2/inode.c`.
#[derive(Clone, Copy, Debug)]
pub(super) struct BlockPath {
    /// Number of levels in the pointer chain (1 = direct, 2 = single indirect,
    /// 3 = double indirect, 4 = triple indirect). A depth of 0 is invalid.
    pub depth: usize,
    /// Index at each level of the block pointer tree. Only `offsets[0..depth]`
    /// are meaningful. `offsets[0]` indexes into `inode.i_block[]` (0..11 for
    /// direct, 12/13/14 for indirect entries); subsequent elements index into
    /// the corresponding indirect block.
    pub offsets: [u32; 4],
    /// Number of consecutive block slots remaining after the current offset
    /// within the lowest-level indirect block (or the direct region for depth 1).
    /// Used for multi-block contiguous allocation optimization.
    pub boundary: u32,
}

/// A single level in the block-pointer chain.
///
/// Linux analogue: `Indirect` entry in `/root/linux/fs/ext2/inode.c`.
#[derive(Clone, Debug)]
struct IndirectEntry {
    /// Physical block number read from this level's slot; 0 means hole.
    key: u32,
    /// Cached indirect block that contains this level's slot.
    /// `None` for level 0, where the slot lives in inode `i_block[]`.
    bh: Option<Vec<u8>>,
}

/// Result of traversing a block-pointer chain.
#[derive(Debug)]
struct BranchResult {
    /// Level where traversal stopped on a zero pointer, or `path.depth` if complete.
    partial_level: usize,
    /// Entries traversed so far.
    chain: Vec<IndirectEntry>,
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
        fs::{
            ext2::{
                fs::ROOT_INO,
                testkit::{
                    self, CollectDirentVisitor, ErrorBioDisk, Ext2FixtureBuilder, RawInodeBuilder,
                    StopAfterVisitor, encode_dir_entry, write_indirect_ptr,
                },
            },
            utils::{IdBitmap, InodeIo, StatusFlags},
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

    fn make_live_file_inode(
        ext2: &Arc<Ext2>,
        ino: u32,
        size: usize,
        blocks: u32,
        flags: FileFlags,
        block_ptrs: [u32; 15],
    ) -> Arc<Inode> {
        let mut raw = make_raw_inode(0o100644);
        raw.size_lo = size as u32;
        raw.blocks = blocks;
        raw.flags = flags.bits();
        raw.block = block_ptrs;
        let desc = InodeDesc::try_from(&raw).unwrap();
        Inode::new(
            ino,
            InodeType::File,
            Dirty::new(desc),
            0,
            Arc::downgrade(ext2),
        )
    }

    fn reload_group0_cached_bitmaps_from_disk(f: &testkit::Ext2Fixture) {
        let group = &f.block_groups()[0];

        let mut block_bitmap_buf = vec![0u8; BLOCK_SIZE];
        f.disk
            .segment()
            .read_bytes(group.block_bitmap_bid().to_offset(), &mut block_bitmap_buf)
            .unwrap();
        let block_len = {
            let bitmap = group.block_bitmap();
            bitmap.len()
        };
        let mut block_bitmap = group.block_bitmap_mut();
        **block_bitmap = IdBitmap::from_buf(block_bitmap_buf.into_boxed_slice(), block_len);

        let mut inode_bitmap_buf = vec![0u8; BLOCK_SIZE];
        f.disk
            .segment()
            .read_bytes(group.inode_bitmap_bid().to_offset(), &mut inode_bitmap_buf)
            .unwrap();
        let inode_len = {
            let bitmap = group.inode_bitmap();
            bitmap.len()
        };
        let mut inode_bitmap = group.inode_bitmap_mut();
        **inode_bitmap = IdBitmap::from_buf(inode_bitmap_buf.into_boxed_slice(), inode_len);
    }

    #[ktest]
    fn namei_create_adds_entry_and_inits_inode() {
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
        assert_eq!(
            f.ext2.read_inode_desc(created.ino()).unwrap().links_count,
            1
        );

        // Linux ext2_mkdir intent: child links=2 and parent link count +1.
        let created_dir = root
            .create("sub", InodeType::Dir, FilePerm::from_bits_truncate(0o755))
            .unwrap();
        assert_eq!(
            root.inner.read().find_entry("sub").unwrap(),
            created_dir.ino()
        );
        assert_eq!(
            f.ext2
                .read_inode_desc(created_dir.ino())
                .unwrap()
                .links_count,
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
    fn namei_link_unlink_updates_nlinks() {
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
        let inode_bitmap = f.block_groups()[0].inode_bitmap();
        assert!(!inode_bitmap.is_allocated((old_ino - 1) as u16));
    }

    #[ktest]
    fn namei_set_link_and_rename_ok() {
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

        let inode_bitmap = f.block_groups()[0].inode_bitmap();
        assert!(!inode_bitmap.is_allocated((replaced_ino - 1) as u16));

        let _ = src;
    }

    #[ktest]
    fn namei_cross_fs_link_and_rename_returns_exdev() {
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
    fn desc_try_from_valid_raw_ok() {
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
    fn desc_try_from_invalid_raw_returns_err() {
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
    fn dir_lookup_and_readdir_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();

        let foo = root
            .create("foo", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();
        let subdir = root
            .create(
                "subdir",
                InodeType::Dir,
                FilePerm::from_bits_truncate(0o755),
            )
            .unwrap();

        let inner = root.inner.read();
        assert_eq!(inner.find_entry("foo").unwrap(), foo.ino());
        assert_eq!(inner.find_entry("subdir").unwrap(), subdir.ino());
        assert_eq!(
            inner.find_entry("missing").unwrap_err().error(),
            Errno::ENOENT
        );

        let mut visitor = CollectDirentVisitor::default();
        inner.readdir_at(0, &mut visitor).unwrap();
        // Root has ".", "..", "foo", "subdir".
        assert_eq!(visitor.entries.len(), 4);
        assert_eq!(visitor.entries[0].0, ".");
        assert_eq!(visitor.entries[1].0, "..");
        assert_eq!(visitor.entries[2].0, "foo");
        assert_eq!(visitor.entries[2].2, InodeType::File);
        assert_eq!(visitor.entries[3].0, "subdir");
        assert_eq!(visitor.entries[3].2, InodeType::Dir);

        let mut stop_visitor = StopAfterVisitor::new(2);
        let stop_advanced = inner.readdir_at(0, &mut stop_visitor).unwrap();
        assert!(stop_advanced > 0 && stop_advanced < inner.desc.size as usize);

        let first_entry_end = visitor.entries[0].3 + 1;
        let mut offset_visitor = CollectDirentVisitor::default();
        inner
            .readdir_at(first_entry_end, &mut offset_visitor)
            .unwrap();
        assert_eq!(offset_visitor.entries.len(), 3);
        assert_eq!(offset_visitor.entries[0].0, "..");
    }

    #[ktest]
    fn dir_lookup_and_readdir_invalid_returns_err() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let (disk, ext2) = (&f.disk, &f.ext2);
        let block_size = ext2.block_size();

        let mut file_ptrs = [0u32; 15];
        file_ptrs[0] = 80;
        let file_inode = make_live_file_inode(ext2, 50, 0, 0, FileFlags::empty(), file_ptrs);
        let file_inner = file_inode.inner.read();
        assert_eq!(
            file_inner.find_entry("foo").unwrap_err().error(),
            Errno::ENOTDIR
        );
        let mut vec_visitor = Vec::<String>::new();
        assert_eq!(
            file_inner
                .readdir_at(0, &mut vec_visitor)
                .unwrap_err()
                .error(),
            Errno::ENOTDIR
        );
        drop(file_inner);

        let hole_inode = make_live_dir_inode(ext2, 3, 12, 8, FileFlags::empty(), [0u32; 15]);
        let hole_inner = hole_inode.inner.read();
        assert_eq!(
            hole_inner.find_entry("foo").unwrap_err().error(),
            Errno::EIO
        );
        let mut vec_visitor = Vec::<String>::new();
        assert_eq!(
            hole_inner
                .readdir_at(0, &mut vec_visitor)
                .unwrap_err()
                .error(),
            Errno::EIO
        );
        drop(hole_inner);

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
            make_live_dir_inode(ext2, 4, block_size, 0, FileFlags::empty(), ptrs);
        let limited_inner = limited_blocks_inode.inner.read();
        assert_eq!(
            limited_inner.find_entry("missing").unwrap_err().error(),
            Errno::ENOENT
        );

        let tiny_dir_inode = make_live_dir_inode(ext2, 5, 11, 8, FileFlags::empty(), ptrs);
        let tiny_inner = tiny_dir_inode.inner.read();
        let mut vec_visitor = Vec::<String>::new();
        assert_eq!(tiny_inner.readdir_at(0, &mut vec_visitor).unwrap(), 0);
        drop(tiny_inner);

        let mut vec_visitor = Vec::<String>::new();
        assert_eq!(
            limited_inner
                .readdir_at(block_size - 11, &mut vec_visitor)
                .unwrap(),
            0
        );
        drop(limited_inner);

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
        let bad_inode = make_live_dir_inode(ext2, 6, 12, 8, FileFlags::empty(), bad_ptrs);
        let bad_inner = bad_inode.inner.read();
        assert_eq!(bad_inner.find_entry(".").unwrap_err().error(), Errno::EIO);
        let mut vec_visitor = Vec::<String>::new();
        assert_eq!(
            bad_inner
                .readdir_at(0, &mut vec_visitor)
                .unwrap_err()
                .error(),
            Errno::EIO
        );
    }

    #[ktest]
    fn dir_add_and_delete_entry_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();

        let bar = root
            .create("bar", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();

        root.inner.read().find_entry("bar").unwrap();
        root.add_entry("foo", 11, DirEntryFileType::File).unwrap();
        let dup = root
            .add_entry("foo", 12, DirEntryFileType::File)
            .unwrap_err();
        assert_eq!(dup.error(), Errno::EEXIST);
        root.delete_entry("foo").unwrap();

        let inner = root.inner.read();
        assert_eq!(inner.find_entry(".").unwrap(), ROOT_INO);
        assert_eq!(inner.find_entry("bar").unwrap(), bar.ino());
        assert_eq!(inner.find_entry("foo").unwrap_err().error(), Errno::ENOENT);
    }

    #[ktest]
    fn dir_add_entry_grows_by_new_block_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let block_size = f.ext2.block_size();
        let sectors_per_block = (block_size / SECTOR_SIZE) as u32;

        // Fill root's first block by creating entries with short names.
        // dir_rec_len("x") = 12, so each entry consumes 12 bytes.
        // Keep creating until the block is full.
        let mut idx = 0u32;
        loop {
            let name = alloc::format!("e{idx:03}");
            let size_before = root.inner.read().desc.size;
            let result = root.create(&name, InodeType::File, FilePerm::from_bits_truncate(0o644));
            match result {
                Ok(_) => {
                    let size_after = root.inner.read().desc.size;
                    if size_after > size_before {
                        // Block growth happened — this is what we wanted to test.
                        let inner = root.inner.read();
                        assert_eq!(inner.desc.size, (size_before as usize + block_size) as u64);
                        assert_ne!(inner.desc.block_ptrs[1], 0);
                        assert!(inner.find_entry(&name).is_ok());
                        return;
                    }
                }
                Err(_) => panic!("unexpected create failure"),
            }
            idx += 1;
            assert!(idx < 1000, "block should have grown by now");
        }
    }

    #[ktest]
    fn dir_add_entry_grows_into_indirect_block_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 512)
            .with_free_blocks(256, 256)
            .with_free_inodes(1000, 1000)
            .with_group0_used_dirs(1)
            .with_root()
            .build()
            .unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let block_size = f.ext2.block_size();
        // Use near-NAME_MAX entries so each record is large and we reach
        // single-indirect growth with far fewer insertions.
        let long_name_pad = "x".repeat(250);

        // Use add_entry (no inode alloc) to pack blocks until indirect is triggered.
        let mut idx = 0u32;
        loop {
            let name = alloc::format!("e{idx:04}{long_name_pad}");
            root.add_entry(&name, ROOT_INO, DirEntryFileType::File)
                .unwrap();
            let inner = root.inner.read();
            if inner.desc.block_ptrs[12] != 0 {
                assert!(inner.desc.size > (block_size * 12) as u64);
                assert!(inner.find_entry(&name).is_ok());
                return;
            }
            drop(inner);
            idx += 1;
            assert!(
                idx < 1024,
                "indirect block should have been allocated by now"
            );
        }
    }

    #[ktest]
    fn dir_release_data_blocks_truncates_full_tree() {
        let f = Ext2FixtureBuilder::new(1, 512)
            .with_free_blocks(256, 256)
            .build()
            .unwrap();
        let block_size = f.ext2.block_size();
        let sectors_per_block = (block_size / SECTOR_SIZE) as u32;
        let inode = make_live_dir_inode(&f.ext2, 40, 0, 0, FileFlags::empty(), [0; 15]);

        let (old_block_sectors, free_before, free_after) = {
            let mut inner = inode.inner.write();
            for iblock in 0..13u32 {
                inner.get_or_alloc_block(iblock, true).unwrap().unwrap();
            }
            inner.desc.size = (13 * block_size) as u64;

            let old_block_sectors = inner.desc.blocks;
            assert!(inner.desc.block_ptrs[12] != 0);
            assert!(inner.get_block(12).unwrap().is_some());
            let free_before = f.ext2.super_block().free_blocks_count();

            inner.release_dir_data_blocks_for_cleanup(&f.ext2).unwrap();
            assert_eq!(inner.desc.size, 0);
            assert_eq!(inner.desc.blocks, 0);
            assert!(inner.desc.block_ptrs.iter().all(|ptr| *ptr == 0));
            assert_eq!(inner.get_block(0).unwrap(), None);
            assert_eq!(inner.get_block(12).unwrap(), None);

            let free_after = f.ext2.super_block().free_blocks_count();
            (old_block_sectors, free_before, free_after)
        };

        assert_eq!(
            free_after.saturating_sub(free_before),
            old_block_sectors / sectors_per_block
        );
    }

    #[ktest]
    fn dir_mutation_invalid_ops_return_err() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let ext2 = &f.ext2;

        let file_inode = make_live_file_inode(ext2, 50, 0, 0, FileFlags::empty(), [0u32; 15]);
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
        let dir_inode = make_live_dir_inode(ext2, 2, 0, 8, FileFlags::empty(), dir_ptrs);
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
    fn dir_make_empty_and_is_empty_ok() {
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
        reload_group0_cached_bitmaps_from_disk(&f);

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

        inode.make_empty(ROOT_INO).unwrap();
        let allocated_bid = {
            let inner = inode.inner.read();
            assert!(inner.empty_dir());
            assert_eq!(inner.desc.size as usize, block_size);
            assert_eq!(inner.desc.blocks, (block_size / SECTOR_SIZE) as u32);
            inner.desc.block_ptrs[0]
        };
        assert_ne!(allocated_bid, 0);

        let inner = inode.inner.read();
        assert_eq!(inner.find_entry(".").unwrap(), 12);
        assert_eq!(inner.find_entry("..").unwrap(), ROOT_INO);
    }

    #[ktest]
    fn dir_is_empty_with_extra_entries_returns_false() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();

        let sub = root
            .create("sub", InodeType::Dir, FilePerm::from_bits_truncate(0o755))
            .unwrap();
        sub.create("foo", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();

        assert!(!sub.inner.read().empty_dir());
    }

    struct RmdirTestEnv {
        f: testkit::Ext2Fixture,
        parent: Arc<Inode>,
        child_ino: u32,
    }

    /// Sets up a parent directory (ROOT_INO) with a "sub" child directory.
    /// If `add_child_file` is true, creates a file "foo" inside "sub".
    fn prepare_rmdir_env(add_child_file: bool) -> RmdirTestEnv {
        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let parent = f.ext2.read_inode(ROOT_INO).unwrap();

        let sub = parent
            .create("sub", InodeType::Dir, FilePerm::from_bits_truncate(0o755))
            .unwrap();
        let child_ino = sub.ino();

        if add_child_file {
            sub.create("foo", InodeType::File, FilePerm::from_bits_truncate(0o644))
                .unwrap();
        }

        RmdirTestEnv {
            f,
            parent,
            child_ino,
        }
    }

    // TODO: this test will pass after adding inode cache
    // #[ktest]
    // fn dir_rmdir_removes_child_and_updates_nlinks() {
    //     clocks::init_for_ktest();

    //     let env = prepare_rmdir_env(false);
    //     let f = &env.f;
    //     let child_ino = env.child_ino;
    //     let parent = &env.parent;

    //     parent.rmdir("sub").unwrap();
    //     {
    //         let parent_inner = parent.inner.read();
    //         assert_eq!(parent_inner.desc.links_count, 2);
    //         assert_eq!(
    //             parent_inner.find_entry("sub").unwrap_err().error(),
    //             Errno::ENOENT
    //         );
    //     }

    //     let parent_desc = f.ext2.read_inode_desc(ROOT_INO).unwrap();
    //     assert_eq!(parent_desc.links_count, 2);
    //     let child_desc = f.ext2.read_inode_desc(child_ino).unwrap();
    //     assert_eq!(child_desc.size, 0);
    //     assert_eq!(child_desc.links_count, 0);

    //     let inode_bitmap = f.block_groups()[0].inode_bitmap();
    //     assert!(!inode_bitmap.is_allocated((child_ino - 1) as u16));
    // }

    #[ktest]
    fn dir_rmdir_notempty_returns_enotempty() {
        clocks::init_for_ktest();

        let env = prepare_rmdir_env(true);
        let f = &env.f;
        let child_ino = env.child_ino;
        let parent = &env.parent;

        let err = parent.rmdir("sub").unwrap_err();
        assert_eq!(err.error(), Errno::ENOTEMPTY);

        let parent_inner = parent.inner.read();
        assert_eq!(parent_inner.find_entry("sub").unwrap(), child_ino);
        assert_eq!(parent_inner.desc.links_count, 3);

        let inode_bitmap = f.block_groups()[0].inode_bitmap();
        assert!(inode_bitmap.is_allocated((child_ino - 1) as u16));
    }

    #[ktest]
    fn block_mapping_direct_and_indirect_ok() {
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
    fn block_mapping_invalid_depth_returns_err() {
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

    #[ktest]
    fn block_alloc_direct_path_ok() {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let ext2 = &f.ext2;
        let sectors_per_block = (ext2.block_size() / SECTOR_SIZE) as u32;

        let mut inode_inner = make_inode_inner(Arc::downgrade(ext2), [0u32; 15]);
        assert_eq!(inode_inner.get_or_alloc_block(0, false).unwrap(), None);
        assert_eq!(inode_inner.desc.block_ptrs[0], 0);

        let free_before = ext2.super_block().free_blocks_count();
        let allocated = inode_inner.get_or_alloc_block(0, true).unwrap().unwrap();
        let free_after = ext2.super_block().free_blocks_count();

        assert_eq!(inode_inner.desc.block_ptrs[0], allocated.to_raw() as u32);
        assert_eq!(inode_inner.get_block(0).unwrap(), Some(allocated));
        assert_eq!(inode_inner.desc.blocks, sectors_per_block);
        assert_eq!(free_before.saturating_sub(free_after), 1);
    }

    #[ktest]
    fn block_alloc_indirect_path_ok() {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let ext2 = &f.ext2;
        let sectors_per_block = (ext2.block_size() / SECTOR_SIZE) as u32;

        let mut inode_inner = make_inode_inner(Arc::downgrade(ext2), [0u32; 15]);
        let free_before = ext2.super_block().free_blocks_count();
        let allocated = inode_inner.get_or_alloc_block(12, true).unwrap().unwrap();
        let free_after = ext2.super_block().free_blocks_count();

        assert_ne!(inode_inner.desc.block_ptrs[12], 0);
        assert_eq!(inode_inner.get_block(12).unwrap(), Some(allocated));
        assert_eq!(inode_inner.desc.blocks, sectors_per_block.saturating_mul(2));
        assert_eq!(free_before.saturating_sub(free_after), 2);
    }

    #[ktest]
    fn block_alloc_enospc_preserves_inode_state() {
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(0, 0)
            .with_filled_block_bitmap(true)
            .build()
            .unwrap();
        let ext2 = &f.ext2;

        let mut inode_inner = make_inode_inner(Arc::downgrade(ext2), [0u32; 15]);
        let err = inode_inner.get_or_alloc_block(0, true).unwrap_err();
        assert_eq!(err.error(), Errno::ENOSPC);
        assert_eq!(inode_inner.desc.block_ptrs, [0u32; 15]);
        assert_eq!(inode_inner.desc.blocks, 0);
    }

    #[ktest]
    fn block_alloc_fragmented_chain_ok() {
        // Corner case: total free blocks are enough, but no contiguous run can satisfy
        // the full request in one call. This forces get_or_alloc_block() to loop and
        // accumulate allocations across multiple fs.alloc_blocks() calls.
        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(3, 3)
            .build()
            .unwrap();
        let ext2 = &f.ext2;
        let sb = &f.sb;
        let desc = &f.descs[0];

        let first = sb.group_first_block_no(0);
        let last = sb.group_last_block_no(0);
        let data_base = first
            .saturating_add(2)
            .saturating_add(sb.itb_per_group())
            .saturating_add(2);
        let free0 = data_base;
        let free1 = data_base.saturating_add(2);
        let free2 = data_base.saturating_add(4);
        assert!(free2 <= last);

        // Mark every block allocated except three isolated free blocks.
        let mut allocated_blocks = Vec::new();
        for block in first..=last {
            if block == free0 || block == free1 || block == free2 {
                continue;
            }
            allocated_blocks.push(block);
        }
        testkit::write_block_bitmap(f.disk.as_ref(), sb, desc, &allocated_blocks);
        reload_group0_cached_bitmaps_from_disk(&f);

        let ptrs = (ext2.block_size() / size_of::<u32>()) as u32;
        let first_double_iblock = 12 + ptrs;
        let mut inode_inner = make_inode_inner(Arc::downgrade(ext2), [0u32; 15]);

        let allocated_data = inode_inner
            .get_or_alloc_block(first_double_iblock, true)
            .unwrap()
            .unwrap();
        assert_ne!(inode_inner.desc.block_ptrs[13], 0);
        assert_eq!(
            inode_inner.get_block(first_double_iblock).unwrap(),
            Some(allocated_data)
        );

        // All three isolated free blocks should be consumed.
        let block_size = ext2.block_size();
        assert_eq!(ext2.super_block().free_blocks_count(), 0);
        assert_eq!(f.block_groups()[0].free_blocks_count(), 0);
        assert_eq!(
            inode_inner.desc.blocks,
            ((block_size / SECTOR_SIZE) as u32) * 3
        );
    }

    #[ktest]
    fn write_and_resize_truncate_round_trip_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "phase07_io_file",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();

        let block_size = f.ext2.block_size();
        let payload = vec![0x5au8; block_size];

        let free_before_write = f.ext2.super_block().free_blocks_count();
        let mut payload_reader = VmReader::from(payload.as_slice()).to_fallible();
        assert_eq!(
            file.write_direct_at(0, &mut payload_reader).unwrap(),
            payload.len()
        );
        let free_after_write = f.ext2.super_block().free_blocks_count();
        assert_eq!(free_before_write.saturating_sub(free_after_write), 1);

        {
            let inner = file.inner.read();
            assert_eq!(inner.desc.size as usize, payload.len());
        }
        let mut readback = vec![0u8; block_size];
        let mut readback_writer = VmWriter::from(readback.as_mut_slice()).to_fallible();
        assert_eq!(
            file.read_direct_at(0, &mut readback_writer).unwrap(),
            payload.len()
        );
        assert_eq!(&readback[..payload.len()], payload.as_slice());

        let free_before_truncate = f.ext2.super_block().free_blocks_count();
        file.resize(0).unwrap();
        let free_after_truncate = f.ext2.super_block().free_blocks_count();
        assert_eq!(free_after_truncate.saturating_sub(free_before_truncate), 1);

        {
            let inner = file.inner.read();
            assert_eq!(inner.desc.size, 0);
            assert_eq!(inner.desc.blocks, 0);
            assert_eq!(inner.desc.block_ptrs[0], 0);
        }

        let on_disk = f.ext2.read_inode_desc(file.ino()).unwrap();
        assert_eq!(on_disk.size, 0);
        assert_eq!(on_disk.blocks, 0);
    }

    #[ktest]
    fn resize_guard_rejects_invalid_ops() {
        let mut raw_append = make_raw_inode(0o100644);
        raw_append.flags = FileFlags::APPEND_ONLY.bits();
        let append_desc = InodeDesc::try_from(&raw_append).unwrap();
        let mut append_inner = InodeInner::new(Dirty::new(append_desc), Weak::new(), Weak::new());
        assert_eq!(append_inner.resize(1).unwrap_err().error(), Errno::EPERM);

        let mut raw_fast_symlink = make_raw_inode(0o120777);
        raw_fast_symlink.size_lo = 10;
        raw_fast_symlink.blocks = 0;
        let symlink_desc = InodeDesc::try_from(&raw_fast_symlink).unwrap();
        let mut symlink_inner = InodeInner::new(Dirty::new(symlink_desc), Weak::new(), Weak::new());
        assert_eq!(symlink_inner.resize(4).unwrap_err().error(), Errno::EINVAL);
    }

    #[ktest]
    fn write_at_partial_block_preserves_rest() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 20, 0, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();
        let original = vec![0x11u8; block_size];
        let patch = vec![0x7cu8; 257];
        let patch_off = 123usize;

        let mut original_reader = VmReader::from(original.as_slice()).to_fallible();
        file.write_at(0, &mut original_reader).unwrap();
        let mut patch_reader = VmReader::from(patch.as_slice()).to_fallible();
        file.write_at(patch_off, &mut patch_reader).unwrap();

        let mut out = vec![0u8; block_size];
        let mut out_writer = VmWriter::from(out.as_mut_slice()).to_fallible();
        assert_eq!(file.read_at(0, &mut out_writer).unwrap(), block_size);
        assert_eq!(&out[..patch_off], &original[..patch_off]);
        assert_eq!(&out[patch_off..patch_off + patch.len()], patch.as_slice());
        assert_eq!(
            &out[patch_off + patch.len()..],
            &original[patch_off + patch.len()..]
        );
    }

    #[ktest]
    fn write_at_cross_block_boundary_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 21, 0, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();
        let crossing_off = block_size - 64;
        let crossing_data = (0..128)
            .map(|i| (i as u8).wrapping_add(1))
            .collect::<Vec<_>>();

        let zeros = vec![0u8; block_size * 2];
        let mut zeros_reader = VmReader::from(zeros.as_slice()).to_fallible();
        file.write_at(0, &mut zeros_reader).unwrap();
        let mut crossing_reader = VmReader::from(crossing_data.as_slice()).to_fallible();
        file.write_at(crossing_off, &mut crossing_reader).unwrap();

        let mut out = vec![0u8; block_size * 2];
        let mut out_writer = VmWriter::from(out.as_mut_slice()).to_fallible();
        assert_eq!(file.read_at(0, &mut out_writer).unwrap(), block_size * 2);
        assert_eq!(
            &out[crossing_off..crossing_off + 128],
            crossing_data.as_slice()
        );
        assert_eq!(file.inner.read().desc.size as usize, block_size * 2);
    }

    #[ktest]
    fn write_at_sparse_hole_extends_file_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 22, 0, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();
        let write_off = block_size * 2 + 128;
        let payload = vec![0x3au8; 256];

        let mut payload_reader = VmReader::from(payload.as_slice()).to_fallible();
        file.write_at(write_off, &mut payload_reader).unwrap();

        let inner = file.inner.read();
        assert_eq!(inner.desc.size as usize, write_off + payload.len());
        assert_eq!(inner.get_block(0).unwrap(), None);
        assert_eq!(inner.get_block(1).unwrap(), None);
        drop(inner);

        let mut out = vec![0u8; payload.len()];
        let mut out_writer = VmWriter::from(out.as_mut_slice()).to_fallible();
        assert_eq!(
            file.read_at(write_off, &mut out_writer).unwrap(),
            payload.len()
        );
        assert_eq!(out, payload);
    }

    #[ktest]
    fn write_at_enospc_rolls_back_state() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(2, 2)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 23, 0, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();
        let base_data = vec![0x44u8; block_size];

        let mut base_reader = VmReader::from(base_data.as_slice()).to_fallible();
        file.write_direct_at(0, &mut base_reader).unwrap();
        let free_before_fail = f.ext2.super_block().free_blocks_count();
        assert_eq!(free_before_fail, 1);

        let fail_payload = vec![0x66u8; block_size * 2];
        let mut fail_reader = VmReader::from(fail_payload.as_slice()).to_fallible();
        let err = file
            .write_direct_at(block_size, &mut fail_reader)
            .unwrap_err();
        assert_eq!(err.error(), Errno::ENOSPC);

        let inner = file.inner.read();
        assert_eq!(inner.desc.size as usize, block_size);
        assert!(inner.get_block(0).unwrap().is_some());
        assert_eq!(inner.get_block(1).unwrap(), None);
        assert_eq!(f.ext2.super_block().free_blocks_count(), free_before_fail);
        drop(inner);

        let mut readback = vec![0u8; block_size];
        let mut readback_writer = VmWriter::from(readback.as_mut_slice()).to_fallible();
        assert_eq!(
            file.read_direct_at(0, &mut readback_writer).unwrap(),
            block_size
        );
        assert_eq!(readback, base_data);
    }

    #[ktest]
    fn write_at_directory_returns_eisdir() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let mut reader = VmReader::from(b"x".as_slice()).to_fallible();
        let err = root.write_at(0, &mut reader).unwrap_err();
        assert_eq!(err.error(), Errno::EISDIR);
    }

    #[ktest]
    fn read_at_sparse_hole_returns_zeros_and_clamps_eof() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 29, 0, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();
        let write_off = block_size * 2 + 128;
        let payload = (0..256u16).map(|v| v as u8).collect::<Vec<_>>();

        let mut payload_reader = VmReader::from(payload.as_slice()).to_fallible();
        file.write_at(write_off, &mut payload_reader).unwrap();

        let mut buf = vec![0xa5u8; block_size + 256];
        let mut writer = VmWriter::from(buf.as_mut_slice()).to_fallible();
        let bytes_read = file.read_at(block_size, &mut writer).unwrap();
        assert_eq!(bytes_read, buf.len());
        assert!(buf[..block_size].iter().all(|b| *b == 0));
        assert!(buf[block_size..block_size + 128].iter().all(|b| *b == 0));
        assert_eq!(&buf[block_size + 128..], &payload[..128]);

        let mut eof_buf = [0x5au8; 16];
        let mut eof_writer = VmWriter::from(eof_buf.as_mut_slice()).to_fallible();
        let eof_read = file
            .read_at(write_off + payload.len(), &mut eof_writer)
            .unwrap();
        assert_eq!(eof_read, 0);
        assert_eq!(eof_buf, [0x5au8; 16]);

        let mut empty = [];
        let mut empty_writer = VmWriter::from(empty.as_mut_slice()).to_fallible();
        assert_eq!(file.read_at(0, &mut empty_writer).unwrap(), 0);
    }

    #[ktest]
    fn read_at_directory_returns_eisdir() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let mut buf = [0u8; 1];
        let mut writer = VmWriter::from(buf.as_mut_slice()).to_fallible();
        let err = root.read_at(0, &mut writer).unwrap_err();
        assert_eq!(err.error(), Errno::EISDIR);
    }

    #[ktest]
    fn read_at_io_error_returns_eio() {
        clocks::init_for_ktest();

        let base = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let fail_bid = 40u32;
        let fail_offset = Bid::new(fail_bid as u64).to_offset();
        let io_disk = Arc::new(ErrorBioDisk::with_read_error_at(
            base.disk.clone(),
            BioStatus::IoError,
            fail_offset,
        ));
        let io_f = Ext2FixtureBuilder::new(2, 256)
            .with_device(io_disk)
            .build()
            .unwrap();

        let block_size = io_f.ext2.block_size();
        let sectors_per_block = (block_size / SECTOR_SIZE) as u32;
        let mut ptrs = [0u32; 15];
        ptrs[0] = fail_bid;
        let file = make_live_file_inode(
            &io_f.ext2,
            30,
            64,
            sectors_per_block,
            FileFlags::empty(),
            ptrs,
        );

        let mut buf = [0u8; 32];
        let mut writer = VmWriter::from(buf.as_mut_slice()).to_fallible();
        let err = file.read_at(0, &mut writer).unwrap_err();
        assert_eq!(err.error(), Errno::EIO);
    }

    #[ktest]
    fn resize_extend_sparse_skips_alloc() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 24, 0, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();

        let mut inner = file.inner.write();
        let target = block_size * 3 + 123;
        inner.resize(target).unwrap();
        assert_eq!(inner.desc.size as usize, target);
        assert_eq!(inner.desc.blocks, 0);
        assert!(inner.desc.block_ptrs.iter().all(|ptr| *ptr == 0));
    }

    #[ktest]
    fn resize_shrink_zeroes_partial_tail_block() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 25, 0, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();
        let sectors_per_block = (block_size / SECTOR_SIZE) as u32;
        let keep_in_tail = 200usize;

        let payload = vec![0xabu8; block_size * 2];
        let mut payload_reader = VmReader::from(payload.as_slice()).to_fallible();
        file.write_direct_at(0, &mut payload_reader).unwrap();
        file.resize(block_size + keep_in_tail).unwrap();

        let mut kept = vec![0u8; keep_in_tail];
        let mut kept_writer = VmWriter::from(kept.as_mut_slice()).to_fallible();
        assert_eq!(
            file.read_at(block_size, &mut kept_writer).unwrap(),
            keep_in_tail
        );
        assert!(kept.iter().all(|b| *b == 0xab));

        let mut eof = [0x5au8; 32];
        let mut eof_writer = VmWriter::from(eof.as_mut_slice()).to_fallible();
        assert_eq!(
            file.read_at(block_size + keep_in_tail, &mut eof_writer)
                .unwrap(),
            0
        );
        assert_eq!(eof, [0x5au8; 32]);

        let inner = file.inner.read();
        assert_eq!(inner.desc.size as usize, block_size + keep_in_tail);
        assert_eq!(inner.desc.blocks, sectors_per_block.saturating_mul(2));
    }

    // TODO: this test will failed due to the bug of PageCache::discard_range.
    // #[ktest]
    // fn direct_io_dispatch_and_three_phase_write() {
    //     clocks::init_for_ktest();

    //     let f = Ext2FixtureBuilder::new(1, 256)
    //         .with_free_blocks(64, 64)
    //         .build()
    //         .unwrap();
    //     let file = make_live_file_inode(&f.ext2, 90, 0, 0, FileFlags::empty(), [0; 15]);
    //     let block_size = f.ext2.block_size();

    //     let buffered_old = vec![0x11u8; block_size * 2];
    //     let mut old_reader = VmReader::from(buffered_old.as_slice()).to_fallible();
    //     file.write_direct_at(0, &mut old_reader).unwrap();

    //     // Populate page cache with buffered read first, then overwrite via O_DIRECT.
    //     let mut warm_buf = vec![0u8; block_size * 2];
    //     let mut warm_writer = VmWriter::from(warm_buf.as_mut_slice()).to_fallible();
    //     file.read_at(0, &mut warm_writer).unwrap();

    //     let direct_new = vec![0x7au8; block_size * 2];
    //     let mut direct_writer = VmReader::from(direct_new.as_slice()).to_fallible();
    //     let written =
    //         InodeIo::write_at(&*file, 0, &mut direct_writer, StatusFlags::O_DIRECT).unwrap();
    //     assert_eq!(written, direct_new.len());

    //     let mut direct_read_buf = vec![0u8; block_size * 2];
    //     let mut direct_read_writer = VmWriter::from(direct_read_buf.as_mut_slice()).to_fallible();
    //     let read =
    //         InodeIo::read_at(&*file, 0, &mut direct_read_writer, StatusFlags::O_DIRECT).unwrap();
    //     assert_eq!(read, direct_new.len());
    //     assert_eq!(direct_read_buf, direct_new);

    //     let mut buffered_read_buf = vec![0u8; block_size * 2];
    //     let mut buffered_read_writer =
    //         VmWriter::from(buffered_read_buf.as_mut_slice()).to_fallible();
    //     let buffered_read = file.read_at(0, &mut buffered_read_writer).unwrap();
    //     assert_eq!(buffered_read, direct_new.len());
    //     assert_eq!(buffered_read_buf, direct_new);
    // }

    #[ktest]
    fn resize_truncate_indirect_frees_shared_path() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 26, 0, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();
        let ptrs = (block_size / size_of::<u32>()) as u32;
        let first_double_iblock = 12 + ptrs;

        let mut inner = file.inner.write();
        inner.get_or_alloc_block(first_double_iblock, true).unwrap();
        inner
            .get_or_alloc_block(first_double_iblock + 1, true)
            .unwrap();
        inner
            .get_or_alloc_block(first_double_iblock + 2, true)
            .unwrap();
        inner.desc.size = ((first_double_iblock as usize + 3) * block_size) as u64;

        inner
            .resize((first_double_iblock as usize + 1) * block_size)
            .unwrap();
        assert!(inner.get_block(first_double_iblock).unwrap().is_some());
        assert_eq!(inner.get_block(first_double_iblock + 1).unwrap(), None);
        assert_eq!(inner.get_block(first_double_iblock + 2).unwrap(), None);
    }

    #[ktest]
    fn resize_truncate_releases_all_indirect_blocks() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 27, 1, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();
        let ptrs = (block_size / size_of::<u32>()) as u32;
        let first_double_iblock = 12 + ptrs;
        let first_triple_iblock = 12 + ptrs + (1u32 << (ptrs.trailing_zeros() * 2));

        let mut inner = file.inner.write();
        inner.get_or_alloc_block(12, true).unwrap();
        inner.get_or_alloc_block(first_double_iblock, true).unwrap();
        inner.get_or_alloc_block(first_triple_iblock, true).unwrap();
        assert_ne!(inner.desc.block_ptrs[12], 0);
        assert_ne!(inner.desc.block_ptrs[13], 0);
        assert_ne!(inner.desc.block_ptrs[14], 0);

        inner.resize(0).unwrap();
        assert_eq!(inner.desc.block_ptrs[12], 0);
        assert_eq!(inner.desc.block_ptrs[13], 0);
        assert_eq!(inner.desc.block_ptrs[14], 0);
        assert_eq!(inner.get_block(12).unwrap(), None);
        assert_eq!(inner.get_block(first_double_iblock).unwrap(), None);
        assert_eq!(inner.get_block(first_triple_iblock).unwrap(), None);
        assert_eq!(inner.desc.blocks, 0);
    }

    #[ktest]
    fn free_branches_recursively_releases_blocks() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 28, 1, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();
        let sectors_per_block = (block_size / SECTOR_SIZE) as u32;
        let ptrs = (block_size / size_of::<u32>()) as u32;
        let first_triple_iblock = 12 + ptrs + (1u32 << (ptrs.trailing_zeros() * 2));

        let mut inner = file.inner.write();
        inner.get_or_alloc_block(first_triple_iblock, true).unwrap();
        let root = inner.desc.block_ptrs[14];
        assert_ne!(root, 0);
        assert_eq!(inner.desc.blocks, sectors_per_block.saturating_mul(4));

        let free_before = f.ext2.super_block().free_blocks_count();
        inner.free_branches(&f.ext2, root, 3);
        inner.desc.block_ptrs[14] = 0;
        let free_after = f.ext2.super_block().free_blocks_count();

        assert_eq!(free_after.saturating_sub(free_before), 4);
        assert_eq!(inner.desc.blocks, 0);
    }

    #[ktest]
    fn new_inode_initializes_page_cache_capacity() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256).build().unwrap();
        let block_size = f.ext2.block_size();

        let inode_empty = make_live_file_inode(&f.ext2, 60, 0, 0, FileFlags::empty(), [0; 15]);
        assert_eq!(inode_empty.inner.read().page_cache.pages().size(), 0);

        let inode_non_empty =
            make_live_file_inode(&f.ext2, 61, block_size + 1, 0, FileFlags::empty(), [0; 15]);
        assert_eq!(
            inode_non_empty.inner.read().page_cache.pages().size(),
            (block_size + 1).align_up(BLOCK_SIZE)
        );
    }

    #[ktest]
    fn page_cache_npages_matches_desc_size() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256).build().unwrap();
        let block_size = f.ext2.block_size();

        let inode =
            make_live_file_inode(&f.ext2, 62, block_size + 1, 0, FileFlags::empty(), [0; 15]);
        assert_eq!(<Inode as PageCacheBackend>::npages(&inode), 2);

        let inode_zero = make_live_file_inode(&f.ext2, 63, 0, 0, FileFlags::empty(), [0; 15]);
        assert_eq!(<Inode as PageCacheBackend>::npages(&inode_zero), 0);
    }

    #[ktest]
    fn read_write_via_page_cache_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 64, 0, 0, FileFlags::empty(), [0; 15]);

        let payload = b"hello-page-cache";
        let mut reader = VmReader::from(payload.as_slice()).to_fallible();
        let written = file.write_at(0, &mut reader).unwrap();
        assert_eq!(written, payload.len());

        let mut out = vec![0u8; payload.len()];
        let mut writer = VmWriter::from(out.as_mut_slice()).to_fallible();
        let read = file.read_at(0, &mut writer).unwrap();
        assert_eq!(read, payload.len());
        assert_eq!(out.as_slice(), payload.as_slice());
    }

    #[ktest]
    fn page_cache_write_at_directory_returns_eisdir() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let mut reader = VmReader::from(b"x".as_slice()).to_fallible();
        let err = root.write_at(0, &mut reader).unwrap_err();
        assert_eq!(err.error(), Errno::EISDIR);
    }

    #[ktest]
    fn page_cache_resize_extend_sparse_skips_alloc() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 65, 0, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();

        file.resize(block_size * 2 + 7).unwrap();

        let inner = file.inner.read();
        assert_eq!(inner.desc.size as usize, block_size * 2 + 7);
        assert_eq!(inner.desc.blocks, 0);
        assert!(inner.desc.block_ptrs.iter().all(|ptr| *ptr == 0));
    }

    #[ktest]
    fn page_cache_writeback_unmapped_returns_eio() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let block_size = f.ext2.block_size();
        let file = make_live_file_inode(&f.ext2, 66, block_size, 0, FileFlags::empty(), [0; 15]);

        {
            let inner = file.inner.read();
            inner.page_cache.resize(block_size).unwrap();

            let one_byte = [0x5au8];
            let mut reader = VmReader::from(one_byte.as_slice()).to_fallible();
            inner.page_cache.pages().write(0, &mut reader).unwrap();

            let err = inner.page_cache.evict_range(0..block_size).unwrap_err();
            assert_eq!(err.error(), Errno::EIO);
        }
    }
}
