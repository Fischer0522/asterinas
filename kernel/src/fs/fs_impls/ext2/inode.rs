// SPDX-License-Identifier: MPL-2.0

//! In-memory [`Inode`] and directory/symlink/file operations.

use core::{
    mem::size_of,
    sync::atomic::{AtomicUsize, Ordering},
};

use aster_block::bio::BioCompleteFn;
use device_id::DeviceId;
use ostd::{const_assert, mm::io::util::HasVmReaderWriter};

use super::{
    block_ptr_tree::{BlockPtrTree, Ext2Bid, RawBlockPtrs},
    dir::{DirBlock, DirEntryHeader},
    fs::Ext2,
    io_range_mapper::{IoRange, IoRangeMapper},
    prelude::*,
    utils::now,
    xattr::Xattr,
};
use crate::{
    fs::{
        ext2::dir::DirEntryFileType,
        file::InodeMode,
        vfs::{
            inode::{Extension, FallocMode, Metadata},
            xattr::{XattrName, XattrNamespace, XattrSetFlags},
        },
    },
    process::{Gid, Uid},
};

/// Maximum bytes storable in ext2 inode i_block area for fast symlink payload.
///
const MAX_FAST_SYMLINK_LEN: usize = size_of::<u32>() * 15;
const MAX_LINK_COUNT: u16 = 32000;

/// Ext2 file permission bits (lower 12 bits of `i_mode`).
#[derive(Clone, Copy, Debug)]
pub struct FilePerm(u16);

impl FilePerm {
    pub(super) fn from_bits_truncate(bits: u16) -> Self {
        Self(bits)
    }

    pub(super) fn bits(self) -> u16 {
        self.0
    }
}

/// Represents an in-memory ext2 inode.
///
/// Each `Inode` corresponds to one on-disk inode
/// identified by a unique inode number (`ino`).
/// It caches the inode descriptor, manages the data page cache,
/// and exposes directory, symlink, and regular-file operations
/// through the VFS [`VfsInode`] trait.
#[derive(Debug)]
pub struct Inode {
    ino: u32,
    type_: InodeType,
    block_size: usize,
    inner: RwMutex<InodeInner>,
    block_group_idx: usize,
    fs: Weak<Ext2>,
    xattr: Option<RwMutex<Xattr>>,
    extension: Extension,
}

struct RenameContext<'a> {
    source_dir: &'a Inode,
    target_dir: &'a Inode,
    old_name: &'a str,
    new_name: &'a str,
    old_ino: u32,
    old_inode: Arc<Inode>,
    existing_ino: Option<u32>,
    existing_inode: Option<Arc<Inode>>,
    old_is_dir: bool,
    moved_ft: DirEntryFileType,
}

impl RenameContext<'_> {
    fn is_same_dir(&self) -> bool {
        self.source_dir.ino == self.target_dir.ino
    }
}

struct MultiInodeInnerGuards<'a> {
    entries: Vec<(u32, RwMutexWriteGuard<'a, InodeInner>)>,
}

impl<'a> MultiInodeInnerGuards<'a> {
    // `inodes` must already be deduplicated by inode number.
    fn lock(inodes: &[&'a Inode]) -> Self {
        let guards = write_lock_multiple_inodes(inodes);
        let entries = inodes
            .iter()
            .map(|inode| inode.ino)
            .zip(guards)
            .collect::<Vec<_>>();
        Self { entries }
    }

    fn inner(&self, ino: u32) -> Result<&InodeInner> {
        let (_, guard) = self
            .entries
            .iter()
            .find(|(entry_ino, _)| *entry_ino == ino)
            .ok_or_else(|| Error::with_message(Errno::EIO, "missing inode inner lock"))?;
        Ok(&*guard)
    }

    fn inner_mut(&mut self, ino: u32) -> Result<&mut InodeInner> {
        let (_, guard) = self
            .entries
            .iter_mut()
            .find(|(entry_ino, _)| *entry_ino == ino)
            .ok_or_else(|| Error::with_message(Errno::EIO, "missing inode inner lock"))?;
        Ok(&mut *guard)
    }
}

impl Inode {
    pub(super) fn new(
        ino: u32,
        type_: InodeType,
        desc: Dirty<InodeDesc>,
        block_group_idx: usize,
        fs: Weak<Ext2>,
    ) -> Arc<Self> {
        let block_size = fs
            .upgrade()
            .expect("filesystem must be alive during inode creation")
            .block_size();

        // Use `new_cyclic` so `InodeInner` can keep a weak self pointer for
        // inode-internal workflows that need to upgrade to `Arc<Inode>`.

        Arc::new_cyclic(|weak_self: &Weak<Self>| Self {
            ino,
            type_,
            block_size,
            block_group_idx,

            xattr: match type_ {
                InodeType::Dir | InodeType::File => Some(RwMutex::new(Xattr::new(
                    desc.file_acl,
                    weak_self.clone(),
                    fs.clone(),
                ))),
                _ => None,
            },
            inner: RwMutex::new(InodeInner::new(desc, fs.clone())),
            fs,
            extension: Extension::new(),
        })
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.inner.read().is_dirty()
    }

    pub(super) fn ino(&self) -> u32 {
        self.ino
    }

    pub(super) fn block_group_idx(&self) -> usize {
        self.block_group_idx
    }

    pub(super) fn link_count(&self) -> u16 {
        self.inner.read().link_count()
    }

    pub(super) fn fs(&self) -> Result<Arc<Ext2>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))
    }

    fn is_invalid_child_name(name: &str) -> bool {
        let name_bytes = name.as_bytes();
        name_bytes.is_empty()
            || name_bytes.len() > u8::MAX as usize
            || name_bytes == b"."
            || name_bytes == b".."
    }

    pub(super) fn file_size(&self) -> usize {
        self.inner.read().file_size()
    }

    /// Returns the encoded device ID for special files.
    ///
    pub(super) fn device_id(&self) -> u64 {
        // Non-device inodes report rdev = 0.
        if self.type_ != InodeType::CharDevice && self.type_ != InodeType::BlockDevice {
            return 0;
        }

        // The `i_block` payload lives in the block_ptr_tree domain.
        let inner = self.inner.read();
        let backend = inner.backend();
        let block_ptr_tree = backend.block_ptr_tree.read();
        block_ptr_tree.raw_block_ptrs.decode_device_id()
    }

    /// Sets the encoded device ID for special files and persists it.
    ///
    pub(super) fn set_device_id(&self, device_id: u64) -> Result<()> {
        if self.type_ != InodeType::CharDevice && self.type_ != InodeType::BlockDevice {
            // Fail with EINVAL for non-device inodes; no lock/state mutation needed.
            return_errno!(Errno::EINVAL);
        }
        // Lock order: inner -> block_ptr_tree.
        let mut inner = self.inner.write();
        inner.encode_device_id(device_id)?;
        // Store the ext2 on-disk device encoding directly in `block_ptrs`
        // instead of caching a decoded value separately.
        inner.set_ctime(now());
        Ok(())
    }

    pub(super) fn resize(&self, new_size: usize) -> Result<()> {
        let block_size = self.block_size;
        if self.type_ != InodeType::File
            && self.type_ != InodeType::Dir
            && self.type_ != InodeType::SymLink
        {
            return_errno!(Errno::EINVAL);
        }

        let inner = self.inner.upread();
        // Keep resize invalid for existing fast symlinks (inline payload), but
        // allow empty newly-created symlink inodes to grow into slow symlinks.
        if inner.desc.is_fast_symlink(block_size) && inner.file_size() != 0 {
            return_errno!(Errno::EINVAL);
        }

        if inner
            .desc
            .flags
            .intersects(FileFlags::APPEND_ONLY | FileFlags::IMMUTABLE)
        {
            return_errno!(Errno::EPERM);
        }

        let old_size = inner.file_size();

        if new_size == old_size {
            return Ok(());
        }

        let mut inner = inner.upgrade();
        if new_size < old_size {
            inner.shrink(new_size)?;
        } else {
            inner.expand(new_size)?;
        }
        inner.touch_mtime_ctime(now());
        Ok(())
    }

    pub(super) fn metadata(&self) -> Metadata {
        // Lock order: inner -> block_ptr_tree.
        let inner = self.inner.read();
        let backend = inner.backend();
        let block_ptr_tree = backend.block_ptr_tree.read();

        let container_dev_id = match self.fs.upgrade() {
            Some(fs) => fs.block_device().id(),
            None => DeviceId::null(),
        };
        let self_dev_id =
            if self.type_ == InodeType::CharDevice || self.type_ == InodeType::BlockDevice {
                // For device inodes, decode rdev from i_block old/new format.
                DeviceId::from_encoded_u64(block_ptr_tree.raw_block_ptrs.decode_device_id())
            } else {
                None
            };
        Metadata {
            ino: self.ino as u64,
            size: inner.file_size(),
            optimal_block_size: self.block_size,
            nr_sectors_allocated: block_ptr_tree.raw_block_ptrs.sector_count as usize,
            last_access_at: inner.atime(),
            last_modify_at: inner.mtime(),
            last_meta_change_at: inner.ctime(),
            type_: self.type_,
            mode: inner.mode(),
            nr_hard_links: inner.link_count() as usize,
            uid: Uid::new(inner.uid()),
            gid: Gid::new(inner.gid()),
            container_dev_id,
            self_dev_id,
        }
    }

    pub(super) fn inode_type(&self) -> InodeType {
        self.type_
    }

    pub(super) fn mode(&self) -> InodeMode {
        let inner = self.inner.read();
        inner.mode()
    }

    pub(super) fn set_mode(&self, mode: InodeMode) -> Result<()> {
        let mut inner = self.inner.write();
        inner.set_mode(mode);
        inner.set_ctime(now());
        Ok(())
    }

    pub(super) fn uid(&self) -> u32 {
        self.inner.read().uid()
    }

    pub(super) fn set_uid(&self, uid: u32) -> Result<()> {
        let mut inner = self.inner.write();
        inner.set_uid(uid);
        inner.set_ctime(now());
        Ok(())
    }

    pub(super) fn gid(&self) -> u32 {
        self.inner.read().gid()
    }

    pub(super) fn set_gid(&self, gid: u32) -> Result<()> {
        let mut inner = self.inner.write();
        inner.set_gid(gid);
        inner.set_ctime(now());
        Ok(())
    }

    pub(super) fn atime(&self) -> Duration {
        self.inner.read().atime()
    }

    pub(super) fn set_atime(&self, time: Duration) {
        self.inner.write().set_atime(time);
    }

    pub(super) fn mtime(&self) -> Duration {
        self.inner.read().mtime()
    }

    pub(super) fn set_mtime(&self, time: Duration) {
        self.inner.write().set_mtime(time);
    }

    pub(super) fn ctime(&self) -> Duration {
        self.inner.read().ctime()
    }

    pub(super) fn set_ctime(&self, time: Duration) {
        self.inner.write().set_ctime(time);
    }

    /// Reads one extended-attribute value and writes it to `value_writer`.
    ///
    pub(super) fn get_xattr(&self, name: XattrName, value_writer: &mut VmWriter) -> Result<usize> {
        let mut xattr = self
            .xattr
            .as_ref()
            .ok_or(Error::with_message(
                Errno::EOPNOTSUPP,
                "inode does not support extended attributes",
            ))?
            .write();
        xattr.get_xattr(name, value_writer)
    }

    /// Lists extended attribute names in one namespace and writes them to `list_writer`.
    ///
    pub(super) fn list_xattr(
        &self,
        namespace: XattrNamespace,
        list_writer: &mut VmWriter,
    ) -> Result<usize> {
        let mut xattr = self
            .xattr
            .as_ref()
            .ok_or(Error::with_message(
                Errno::EOPNOTSUPP,
                "inode does not support extended attributes",
            ))?
            .write();
        xattr.list_xattr(namespace, list_writer)
    }

    /// Creates or replaces one extended attribute.
    ///
    pub(super) fn set_xattr(
        &self,
        name: XattrName,
        value_reader: &mut VmReader,
        flags: XattrSetFlags,
    ) -> Result<()> {
        let mut xattr = self
            .xattr
            .as_ref()
            .ok_or(Error::with_message(
                Errno::EOPNOTSUPP,
                "inode does not support extended attributes",
            ))?
            .write();
        xattr.set_xattr(name, value_reader, flags)?;
        let new_bid = xattr.bid();
        drop(xattr);

        let mut inner = self.inner.write();
        inner.set_file_acl(new_bid);
        inner.set_ctime(now());
        Ok(())
    }

    /// Removes one extended attribute.
    ///
    pub(super) fn remove_xattr(&self, name: XattrName) -> Result<()> {
        let mut xattr = self
            .xattr
            .as_ref()
            .ok_or(Error::with_message(
                Errno::EOPNOTSUPP,
                "inode does not support extended attributes",
            ))?
            .write();
        xattr.remove_xattr(name)?;
        let new_bid = xattr.bid();
        drop(xattr);

        let mut inner = self.inner.write();
        inner.set_file_acl(new_bid);
        Ok(())
    }

    /// Reads symbolic link target bytes and decodes them as UTF-8.
    ///
    pub(super) fn read_link(&self) -> Result<String> {
        if self.type_ != InodeType::SymLink {
            return_errno!(Errno::EINVAL);
        }

        let inner = self.inner.read();
        inner.read_link()
    }

    /// Writes symbolic link target bytes into either fast-inline or slow-page-cache storage.
    ///
    pub(super) fn write_link(&self, target: &str) -> Result<()> {
        if self.type_ != InodeType::SymLink {
            return_errno!(Errno::EINVAL);
        }

        let target_len = target.len();
        let with_nul = target_len.checked_add(1).ok_or_else(|| {
            Error::with_message(Errno::ENAMETOOLONG, "symlink target length overflow")
        })?;

        if with_nul > self.block_size {
            return_errno!(Errno::ENAMETOOLONG);
        }
        let mut inner = self.inner.write();
        inner.write_link(target)?;
        inner.touch_mtime_ctime(now());
        Ok(())
    }

    pub(super) fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        if writer.avail() == 0 {
            return Ok(0);
        }

        let inner = self.inner.upread();
        let file_size = inner.file_size();
        if offset >= file_size {
            return Ok(0);
        }
        let read_len = writer.avail().min(file_size - offset);
        writer.limit(read_len);
        inner.page_cache().read(offset, writer)?;
        inner.upgrade().set_atime(now());
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

        let end = offset
            .checked_add(write_len)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "write range overflow"))?;

        let mut inner = self.inner.write();
        let old_size = inner.file_size();

        // Zero the partial blocks and allocate new blocks
        if let Err(err) = inner.prepare_write(offset, end) {
            inner.rollback_write(old_size, end);
            return Err(err);
        }

        if let Err(err) = inner.page_cache().write(offset, reader) {
            inner.rollback_write(old_size, end);
            return Err(err);
        }

        let current = now();
        inner.touch_mtime_ctime(current);
        if end > old_size {
            inner.set_file_size(end);
        }
        Ok(write_len)
    }

    /// Direct-I/O read path.
    ///
    pub(super) fn read_direct_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        let block_size = self.block_size;
        if !offset.is_multiple_of(block_size) || !writer.avail().is_multiple_of(block_size) {
            return_errno_with_message!(Errno::EINVAL, "not block-aligned");
        }

        let inner = self.inner.upread();

        let file_size = inner.file_size();
        if offset >= file_size || writer.avail() == 0 {
            return Ok(0);
        }

        let read_len = writer.avail().min(file_size - offset);
        writer.limit(read_len);
        let end = offset
            .checked_add(read_len)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "read range overflow"))?;

        // Flush the dirty pages in the read range to make sure the read data is up to date.
        inner.page_cache().flush_range(offset..end)?;
        inner.read_direct_at(offset, end, writer)?;
        inner.upgrade().set_atime(now());
        Ok(read_len)
    }

    /// Direct-I/O write path with pre-allocation and rollback.
    ///
    pub(super) fn write_direct_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        let block_size = self.block_size;
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
        let mut inner = self.inner.write();
        let old_size = inner.file_size();

        // Preallocate direct-write blocks up front so the data path does not
        // need to fall back when it encounters a hole.
        if let Err(err) = inner.prepare_write(offset, end) {
            inner.rollback_write(old_size, end);
            return Err(err);
        }

        // Discard overlapping cached pages before direct write.
        let discard_start = offset.min(old_size);
        let discard_end = end.min(old_size);
        if discard_start < discard_end {
            inner.page_cache().flush_range(discard_start..discard_end)?;
            inner.page_cache().evict_range(discard_start..discard_end)?;
        }

        if let Err(err) = inner.write_direct_at(offset, reader) {
            inner.rollback_write(old_size, end);
            return Err(err);
        }

        let current = now();
        inner.touch_mtime_ctime(current);
        if end > inner.file_size() {
            inner.set_file_size(end);
        }
        Ok(write_len)
    }

    pub(super) fn lookup(&self, name: &str) -> Result<Arc<Inode>> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let inner = self.inner.read();
        let ino = inner.find_entry(name)?;
        let fs = self.fs()?;
        fs.read_inode(ino)
    }

    pub(super) fn readdir_at(
        &self,
        offset: usize,
        visitor: &mut dyn DirentVisitor,
    ) -> Result<usize> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let inner = self.inner.read();
        inner.readdir_at(offset, visitor)
    }

    pub(super) fn rmdir(&self, name: &str) -> Result<()> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        if Self::is_invalid_child_name(name) {
            return_errno!(Errno::EINVAL);
        }

        const RMDIR_RETRY_LIMIT: usize = 8;
        for _ in 0..RMDIR_RETRY_LIMIT {
            let child_ino = {
                let parent_inner = self.inner.read();
                parent_inner.find_entry(name)?
            };
            let fs = self.fs()?;
            let child = fs.read_inode(child_ino)?;
            let lock_targets = [self, child.as_ref()];
            let mut guards = MultiInodeInnerGuards::lock(&lock_targets);

            let parent_inner = guards.inner(self.ino())?;
            let rechecked_child_ino = parent_inner.find_entry(name)?;
            if rechecked_child_ino != child_ino {
                continue;
            }

            let child_inner = guards.inner_mut(child.ino())?;
            if child_inner.inode_type() != InodeType::Dir {
                return_errno!(Errno::ENOTDIR);
            }
            if !child_inner.empty_dir(child.ino())? {
                return_errno!(Errno::ENOTEMPTY);
            }

            child_inner.set_ctime(now());
            child_inner.sub_link_count_saturating(2);

            if child_inner.link_count() == 0 {
                child_inner.persist(child_ino)?;
                let _ = fs.remove_inode_cache(child_ino);
            }

            let parent_inner = guards.inner_mut(self.ino())?;

            let target_entry = parent_inner.find_entry_target(name)?;
            parent_inner.delete_entry(&target_entry)?;
            parent_inner.sub_link_count_saturating(1);
            parent_inner.touch_mtime_ctime(now());

            return Ok(());
        }

        return_errno_with_message!(
            Errno::EAGAIN,
            "rmdir retried due concurrent directory updates"
        )
    }

    /// Implements fallocate operations for ext2.
    pub(super) fn fallocate(&self, mode: FallocMode, offset: usize, len: usize) -> Result<()> {
        match mode {
            FallocMode::Allocate => {
                if len == 0 {
                    return Ok(());
                }

                let end = offset.checked_add(len).ok_or_else(|| {
                    Error::with_message(Errno::EINVAL, "fallocate range overflow")
                })?;
                let mut inner = self.inner.write();
                let old_size = inner.file_size();
                if end > old_size {
                    inner.ensure_size_within_limit(end)?;
                }

                let block_size = self.block_size;
                let new_blocks = match inner.allocate_range_blocks(offset, end, block_size) {
                    Ok(new_blocks) => new_blocks,
                    Err(err) => {
                        inner.rollback_write(old_size, end);
                        return Err(err);
                    }
                };

                if let Err(err) = inner.zero_new_blocks(&new_blocks) {
                    inner.rollback_write(old_size, end);
                    return Err(err);
                }

                if end > old_size {
                    if let Err(err) = inner.expand(end) {
                        inner.rollback_write(old_size, end);
                        return Err(err);
                    }
                }

                inner.touch_mtime_ctime(now());
                Ok(())
            }
            _ => {
                return_errno_with_message!(
                    Errno::EOPNOTSUPP,
                    "fallocate with the specified flags is not supported"
                );
            }
        }
    }

    pub(super) fn sync_all(&self, sync_inode_table: bool) -> Result<()> {
        // Fsync step 1: flush dirty data pages before metadata writeback.
        let inner = self.inner.upread();
        let backend = inner.backend();
        inner.sync_data_pages()?;

        // Fsync step 2: flush inode-local indirect metadata before
        // persisting inode-table state.
        backend
            .block_ptr_tree
            .write()
            .sync_indirect_blocks()?;

        // Fsync step 3: persist inode metadata. The caller is
        // responsible for the final device-cache flush.
        let mut inner = inner.upgrade();
        inner.sync_metadata(self.ino, self.block_group_idx, sync_inode_table)?;

        // Fsync step 4: flush the xattr.
        if let Some(xattr) = &self.xattr {
            xattr.write().flush()?;
        }
        Ok(())
    }

    /// Persists inode metadata without flushing the device write cache.
    ///
    pub(super) fn sync_metadata(&self, sync_inode_table: bool) -> Result<()> {
        let mut inner = self.inner.write();
        inner.sync_metadata(self.ino, self.block_group_idx, sync_inode_table)?;
        Ok(())
    }

    /// Attempts final reclaim for a deleted inode.
    pub(super) fn try_reclaim_deleted_inode(&self) -> Result<bool> {
        if self.link_count() != 0 {
            return Ok(false);
        }

        let fs = self.fs()?;
        let group = fs.block_group(self.block_group_idx);
        let inode_idx = {
            let inodes_per_group = fs.super_block().inodes_per_group();
            (self.ino - 1) % inodes_per_group
        };
        let inode_bit = u16::try_from(inode_idx)
            .map_err(|_| Error::with_message(Errno::EINVAL, "inode index out of range"))?;
        if !group.metadata().inode_bitmap.is_allocated(inode_bit) {
            return Ok(false);
        }

        if let Some(xattr) = self.xattr.as_ref() {
            xattr.write().delete_xattr_block()?;
        }

        let mut inner = self.inner.write();
        let old_size = inner.file_size();
        inner.resize_page_cache_and_update_npages(0, old_size)?;
        let backend = inner.backend().clone();
        inner.set_dtime(now());
        inner.set_file_size(0);
        inner.set_file_acl(0);
        if inner.desc.sector_count > 0 {
            let mut block_ptr_tree = backend.block_ptr_tree.write();
            block_ptr_tree.truncate_blocks(&fs, 0)?;
            inner.sync_desc_block_map_from_snapshot(*block_ptr_tree.raw_block_ptrs());
        }
        inner.persist(self.ino)?;

        fs.free_inode(self.ino, self.type_ == InodeType::Dir)?;
        Ok(true)
    }

    pub(super) fn sync_data(&self) -> Result<()> {
        let inner = self.inner.upread();
        let backend = inner.backend();

        // Fdatasync writes back dirty data pages first. The caller is
        // responsible for the final device-cache flush.
        inner.sync_data_pages()?;

        // fdatasync must also persist dirty indirect metadata needed to reach
        // newly written data blocks before the final device flush.
        backend.block_ptr_tree.write().sync_indirect_blocks()?;

        // Persist metadata conservatively whenever the descriptor is dirty so
        // `fdatasync` does not miss file-size or block-mapping updates.
        let mut inner = inner.upgrade();
        if inner.is_dirty() {
            inner.persist(self.ino)?;
        }
        Ok(())
    }

    /// Creates a child inode and directory entry under this directory.
    ///
    pub(super) fn create(
        &self,
        name: &str,
        type_: InodeType,
        perm: FilePerm,
    ) -> Result<Arc<Inode>> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        if Self::is_invalid_child_name(name) {
            return_errno!(Errno::EINVAL);
        }

        if !matches!(
            type_,
            InodeType::File
                | InodeType::Dir
                | InodeType::SymLink
                | InodeType::CharDevice
                | InodeType::BlockDevice
                | InodeType::NamedPipe
        ) {
            return_errno!(Errno::EINVAL);
        }

        let is_dir = type_ == InodeType::Dir;
        let dir_ft = DirEntryFileType::from(type_);

        // Scan for a slot before creating the child inode to avoid wasting
        // an inode allocation when the name already exists (EEXIST).
        let mut parent_inner = self.inner.write();
        let slot = match parent_inner.scan_dir_for_slot(name)? {
            Some(slot) => slot,
            None => parent_inner.grow_dir_block()?,
        };

        let fs = self.fs()?;
        let child = fs.create_inode(self.ino, type_, perm)?;
        let child_ino = child.ino();

        if is_dir {
            let mut child_inner = child.inner.write();
            child_inner.make_empty(child_ino, self.ino)?;
        }

        parent_inner.add_entry(&slot, name, child_ino, dir_ft)?;

        // Link the child dir's `..` to parent dir.
        if is_dir {
            parent_inner.add_link_count_saturating(1);
        }
        parent_inner.touch_mtime_ctime(now());

        fs.insert_inode_cache(child.clone());
        Ok(child)
    }

    /// Adds a hard link in this directory to an existing non-directory inode.
    ///
    pub(super) fn link(&self, old: &Inode, name: &str) -> Result<()> {
        // Self must be a directory.
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        // Hard links to directories are not allowed.
        if old.type_ == InodeType::Dir {
            return_errno!(Errno::EPERM);
        }

        if Self::is_invalid_child_name(name) {
            return_errno!(Errno::EINVAL);
        }

        // Cross-filesystem check.
        let fs = self.fs()?;
        let old_fs = old.fs()?;
        if !Arc::ptr_eq(&fs, &old_fs) {
            return_errno!(Errno::EINVAL);
        }

        let dir_ft = DirEntryFileType::from(old.type_);
        let (mut dir_inner, mut old_inner) = write_lock_two_inodes(self, old);

        if old_inner.link_count() >= MAX_LINK_COUNT {
            return_errno!(Errno::EOVERFLOW);
        }


        let slot = match dir_inner.scan_dir_for_slot(name)? {
            Some(slot) => slot,
            None => dir_inner.grow_dir_block()?,
        };
        dir_inner.add_entry(&slot, name, old.ino, dir_ft)?;
        dir_inner.touch_mtime_ctime(now());

        old_inner.set_ctime(now());
        old_inner.add_link_count_saturating(1);
        Ok(())
    }

    /// Removes a non-directory entry from this directory.
    ///
    pub(super) fn unlink(&self, name: &str) -> Result<()> {
        // Self must be a directory.
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        if Self::is_invalid_child_name(name) {
            return_errno!(Errno::EINVAL);
        }

        const UNLINK_RETRY_LIMIT: usize = 8;
        for _ in 0..UNLINK_RETRY_LIMIT {
            let child_ino = {
                let parent_inner = self.inner.read();
                parent_inner.find_entry(name)?
            };
            let fs = self.fs()?;
            let child = fs.read_inode(child_ino)?;
            let lock_targets = [self, child.as_ref()];
            let mut guards = MultiInodeInnerGuards::lock(&lock_targets);

            let parent_inner = guards.inner(self.ino())?;
            let rechecked_child_ino = parent_inner.find_entry(name)?;
            if rechecked_child_ino != child_ino {
                continue;
            }

            let child_inner = guards.inner_mut(child.ino())?;
            if child_inner.inode_type() == InodeType::Dir {
                return_errno!(Errno::EISDIR);
            }

            let parent_inner = guards.inner_mut(self.ino())?;

            let target_entry = parent_inner.find_entry_target(name)?;
            parent_inner.delete_entry(&target_entry)?;

            // Update timestamps before dropping the target link count.
            let child_inner = guards.inner_mut(child.ino())?;
            child_inner.set_ctime(now());
            child_inner.sub_link_count_saturating(1);

            if child_inner.link_count() == 0 {
                child_inner.persist(child_ino)?;
                let _ = fs.remove_inode_cache(child_ino);
            }
            return Ok(());
        }

        return_errno_with_message!(
            Errno::EAGAIN,
            "unlink retried due concurrent directory updates"
        )
    }

    /// Renames or moves an entry from this directory to `target` directory.
    ///
    pub(super) fn rename(&self, old_name: &str, target: &Inode, new_name: &str) -> Result<()> {
        // Both self and target must be directories.
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }
        if target.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        if Self::is_invalid_child_name(old_name) {
            return_errno!(Errno::EISDIR);
        }
        if Self::is_invalid_child_name(new_name) {
            return_errno!(Errno::EISDIR);
        }

        // Cross-filesystem check.
        let fs = self.fs()?;
        let target_fs = target.fs()?;
        if !Arc::ptr_eq(&fs, &target_fs) {
            return_errno!(Errno::EINVAL);
        }

        // Rename to itself is a no-op.
        if self.ino == target.ino && old_name == new_name {
            return Ok(());
        }

        const RENAME_RETRY_LIMIT: usize = 8;
        for _ in 0..RENAME_RETRY_LIMIT {
            // Snapshot-then-lock can race with concurrent directory updates.
            // Retry when post-lock recheck detects stale snapshot state.
            if self.do_rename_attempt(target, old_name, new_name)? {
                return Ok(());
            }
        }

        return_errno_with_message!(
            Errno::EAGAIN,
            "rename retried due concurrent directory updates"
        );
    }

    fn do_rename_attempt(&self, target: &Inode, old_name: &str, new_name: &str) -> Result<bool> {
        // Step 1: read the current source/target snapshot without write locks.
        let ctx = self.prepare_rename_context(target, old_name, new_name)?;

        // Step 2: lock all participating inode inner domains in global inode-number order.
        let lock_targets = self.rename_lock_targets(&ctx);
        let mut guards = MultiInodeInnerGuards::lock(&lock_targets);

        // Step 3: verify that the snapshot is still valid under the locks.
        if !self.recheck_rename_state(&ctx, &guards)? {
            return Ok(false);
        }
        // Step 4: apply the rename mutations and persist the metadata.
        self.validate_rename_overwrite(&ctx, &guards)?;
        self.apply_rename_with_locks(&ctx, &mut guards)?;
        drop(guards);

        if let Some(existing) = ctx.existing_inode.as_ref() {
            let mut inner = existing.inner.write();
            if inner.link_count() == 0 {
                inner.persist(existing.ino())?;
                let fs = self.fs()?;
                let _ = fs.remove_inode_cache(existing.ino());
            }
        }
        Ok(true)
    }

    fn prepare_rename_context<'a>(
        &'a self,
        target: &'a Inode,
        old_name: &'a str,
        new_name: &'a str,
    ) -> Result<RenameContext<'a>> {
        let old_ino = {
            let source_inner = self.inner.read();
            source_inner.find_entry(old_name)?
        };
        let fs = self.fs()?;
        let old_inode = fs.read_inode(old_ino)?;
        let existing_ino = {
            let target_inner = target.inner.read();
            target_inner.find_entry(new_name).ok()
        };
        let existing_inode = if let Some(ino) = existing_ino {
            Some(fs.read_inode(ino)?)
        } else {
            None
        };

        let old_is_dir = old_inode.type_ == InodeType::Dir;
        let moved_ft = DirEntryFileType::from(old_inode.type_);
        Ok(RenameContext {
            source_dir: self,
            target_dir: target,
            old_name,
            new_name,
            old_ino,
            old_inode,
            existing_ino,
            existing_inode,
            old_is_dir,
            moved_ft,
        })
    }

    fn rename_lock_targets<'a>(&'a self, ctx: &'a RenameContext<'a>) -> Vec<&'a Inode> {
        let mut targets = Vec::new();
        Self::add_unique_inode_lock_target(&mut targets, ctx.source_dir);
        Self::add_unique_inode_lock_target(&mut targets, ctx.target_dir);
        Self::add_unique_inode_lock_target(&mut targets, ctx.old_inode.as_ref());
        if let Some(existing) = ctx.existing_inode.as_ref() {
            Self::add_unique_inode_lock_target(&mut targets, existing.as_ref());
        }
        targets
    }

    fn add_unique_inode_lock_target<'a>(targets: &mut Vec<&'a Inode>, inode: &'a Inode) {
        if targets.iter().any(|target| target.ino == inode.ino) {
            return;
        }
        targets.push(inode);
    }

    fn recheck_rename_state(
        &self,
        ctx: &RenameContext<'_>,
        guards: &MultiInodeInnerGuards<'_>,
    ) -> Result<bool> {
        let source_inner = guards.inner(ctx.source_dir.ino)?;
        let rechecked_old_ino = source_inner.find_entry(ctx.old_name)?;
        if rechecked_old_ino != ctx.old_ino {
            // Source entry changed after snapshot; caller should retry.
            return Ok(false);
        }

        let target_inner = guards.inner(ctx.target_dir.ino)?;
        let rechecked_existing_ino = target_inner.find_entry(ctx.new_name).ok();
        if rechecked_existing_ino != ctx.existing_ino {
            // Destination state changed after snapshot; caller should retry.
            return Ok(false);
        }

        if ctx.old_is_dir {
            let old_inner = guards.inner(ctx.old_inode.ino())?;
            let dotdot_ino = old_inner.find_entry("..")?;
            if dotdot_ino != ctx.source_dir.ino {
                return_errno_with_message!(Errno::EIO, "failed to update dotdot entry");
            }
        }

        Ok(true)
    }

    fn validate_rename_overwrite(
        &self,
        ctx: &RenameContext<'_>,
        guards: &MultiInodeInnerGuards<'_>,
    ) -> Result<()> {
        let Some(existing) = ctx.existing_inode.as_ref() else {
            return Ok(());
        };

        let existing_is_dir = existing.type_ == InodeType::Dir;
        if ctx.old_is_dir && !existing_is_dir {
            return_errno!(Errno::ENOTDIR);
        }
        if !ctx.old_is_dir && existing_is_dir {
            return_errno!(Errno::EISDIR);
        }
        if existing_is_dir {
            let existing_inner = guards.inner(existing.ino())?;
            if !existing_inner.empty_dir(existing.ino())? {
                return_errno!(Errno::ENOTEMPTY);
            }
        }
        Ok(())
    }

    fn apply_rename_with_locks(
        &self,
        ctx: &RenameContext<'_>,
        guards: &mut MultiInodeInnerGuards<'_>,
    ) -> Result<()> {
        if ctx.is_same_dir() {
            // Same directory: replace/add target name then delete old name in one directory lock.
            let dir_inner = guards.inner_mut(ctx.source_dir.ino)?;
            Self::apply_rename_target_locked(
                dir_inner,
                ctx.new_name,
                ctx.old_ino,
                ctx.moved_ft,
                ctx.existing_inode.is_some(),
            )?;
            let old_target = dir_inner.find_entry_target(ctx.old_name)?;
            dir_inner.delete_entry(&old_target)?;
            if ctx.old_is_dir {
                if ctx.existing_inode.is_none() {
                    dir_inner.add_link_count_saturating(1);
                }
                dir_inner.sub_link_count_saturating(1);
            }
            dir_inner.touch_mtime_ctime(now());
        } else {
            // Cross-directory: publish destination first, then remove source entry.
            {
                let target_inner = guards.inner_mut(ctx.target_dir.ino)?;
                Self::apply_rename_target_locked(
                    target_inner,
                    ctx.new_name,
                    ctx.old_ino,
                    ctx.moved_ft,
                    ctx.existing_inode.is_some(),
                )?;
                if ctx.old_is_dir && ctx.existing_inode.is_none() {
                    target_inner.add_link_count_saturating(1);
                }
                target_inner.touch_mtime_ctime(now());
            }
            {
                let source_inner = guards.inner_mut(ctx.source_dir.ino)?;
                let source_de = source_inner.find_entry_target(ctx.old_name)?;
                source_inner.delete_entry(&source_de)?;
                if ctx.old_is_dir {
                    source_inner.sub_link_count_saturating(1);
                }
                source_inner.touch_mtime_ctime(now());
            }
        }

        if let Some(existing) = ctx.existing_inode.as_ref() {
            // Replaced inode can be distinct from moved inode, or the same inode in corner cases.
            let existing_inner = guards.inner_mut(existing.ino())?;
            existing_inner.set_ctime(now());
            if ctx.old_is_dir {
                existing_inner.sub_link_count_saturating(1);
            }
            existing_inner.sub_link_count_saturating(1);
        }

        let old_inner = guards.inner_mut(ctx.old_inode.ino())?;
        old_inner.set_ctime(now());
        if ctx.old_is_dir && !ctx.is_same_dir() {
            let dotdot = old_inner.find_entry_target("..")?;
            old_inner.set_link(&dotdot, ctx.target_dir.ino, DirEntryFileType::Dir)?;
            old_inner.remove_flags(FileFlags::INDEX_DIR);
        }
        Ok(())
    }

    fn apply_rename_target_locked(
        target_inner: &mut InodeInner,
        new_name: &str,
        old_ino: u32,
        moved_ft: DirEntryFileType,
        has_existing: bool,
    ) -> Result<()> {
        if has_existing {
            // Existing destination entry: ext2_set_link semantics.
            let target_de = target_inner.find_entry_target(new_name)?;
            target_inner.set_link(&target_de, old_ino, moved_ft)?;
            return Ok(());
        }

        // No destination entry: ext2_add_link semantics.
        let slot = match target_inner.scan_dir_for_slot(new_name)? {
            Some(slot) => slot,
            None => target_inner.grow_dir_block()?,
        };
        target_inner.add_entry(&slot, new_name, old_ino, moved_ft)?;
        Ok(())
    }

    pub(super) fn extension(&self) -> &Extension {
        &self.extension
    }

    pub(super) fn page_cache_vmo(&self) -> Arc<Vmo> {
        self.inner.read().page_cache().clone()
    }
}

/// [`PageCacheBackend`] implementation for inode data.
///
/// Translates logical page indices to physical device blocks
/// via the block-pointer tree,
/// then submits BIO requests to the underlying block device.
#[derive(Debug)]
pub(super) struct InodeBackend {
    /// Serializes backend traversal vs foreground block-map mutations.
    block_ptr_tree: RwMutex<BlockPtrTree>,
    /// Cached `npages` bound for PageCache.
    npages: AtomicUsize,
    /// Filesystem handle for indirect I/O and BIO submission.
    fs: Weak<Ext2>,
}

impl Drop for Inode {
    fn drop(&mut self) {
        if let Err(err) = self.try_reclaim_deleted_inode() {
            debug!(
                "ext2: failed to reclaim deleted inode {} during drop: {:?}",
                self.ino, err
            );
        }
    }
}

impl InodeBackend {
    pub(super) fn new(block_ptr_tree: BlockPtrTree, fs: Weak<Ext2>, npages: usize) -> Arc<Self> {
        Arc::new(Self {
            block_ptr_tree: RwMutex::new(block_ptr_tree),
            npages: AtomicUsize::new(npages),
            fs,
        })
    }

    pub(super) fn npages(&self) -> usize {
        self.npages.load(Ordering::Acquire)
    }

    fn fs(&self) -> Result<Arc<Ext2>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))
    }
}

impl PageCacheBackend for InodeBackend {
    fn read_page_raw(
        &self,
        idx: usize,
        bio_segment: BioSegment,
        complete_fn: Option<BioCompleteFn>,
    ) -> Result<BioWaiter> {
        let block_ptr_tree = self.block_ptr_tree.read();
        let fs = self.fs()?;
        let iblock = u32::try_from(idx)
            .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;
        match block_ptr_tree.lookup_block(&fs, iblock)? {
            Some(bid) => fs.read_blocks_async(bid, bio_segment, complete_fn),
            None => {
                // Found a hole, zero fill the page.
                let mut segment_writer = bio_segment.inner_dma_slice().writer().map_err(|_| {
                    Error::with_message(Errno::EIO, "failed to access zero-fill bio segment")
                })?;
                segment_writer.fill_zeros(bio_segment.nbytes());
                if let Some(complete_fn) = complete_fn {
                    complete_fn(true);
                }
                Ok(BioWaiter::new())
            }
        }
    }

    fn write_page_raw(
        &self,
        idx: usize,
        bio_segment: BioSegment,
        complete_fn: Option<BioCompleteFn>,
    ) -> Result<BioWaiter> {
        let block_ptr_tree = self.block_ptr_tree.read();
        let fs = self.fs()?;
        let iblock = u32::try_from(idx)
            .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;

        match block_ptr_tree.lookup_block(&fs, iblock)? {
            Some(bid) => fs.write_blocks_async(bid, bio_segment, complete_fn),
            None => {
                error!(
                    "faild to find a block mapping in PageCacheBackend, idx: {}",
                    idx
                );
                return_errno!(Errno::EIO);
            }
        }
    }

    fn npages(&self) -> usize {
        self.npages.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
struct InodeInner {
    /// Full persistence mirror of the on-disk inode.
    desc: Dirty<InodeDesc>,
    /// Per-inode data/directory page cache.
    page_cache: PageCache,
    /// Dedicated backend used by pager callbacks.
    backend: Arc<InodeBackend>,
    fs: Weak<Ext2>,
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
    fn new(desc: Dirty<InodeDesc>, fs: Weak<Ext2>) -> Self {
        let num_page_bytes = (desc.size as usize).align_up(BLOCK_SIZE);
        let num_pages = num_page_bytes / BLOCK_SIZE;
        let backend = InodeBackend::new(
            BlockPtrTree::new(
                RawBlockPtrs::from_parts(desc.sector_count, desc.block_ptrs),
                fs.clone(),
            ),
            fs.clone(),
            num_pages,
        );
        let page_cache_backend: Weak<dyn PageCacheBackend> = Arc::downgrade(&backend) as _;
        // Keep page-cache capacity aligned with inode size so `npages`/VMO window
        // and on-disk data extent stay consistent from mount time.
        let page_cache = PageCacheOps::with_capacity(num_page_bytes, page_cache_backend)
            .expect("ext2 inode page cache allocation failed");

        Self {
            desc,
            page_cache,
            backend,
            fs,
        }
    }

    fn page_cache(&self) -> &PageCache {
        &self.page_cache
    }

    fn backend(&self) -> &Arc<InodeBackend> {
        &self.backend
    }

    fn sync_desc_block_map_from_snapshot(&mut self, block_map_desc: RawBlockPtrs) {
        self.desc.sector_count = block_map_desc.sector_count;
        self.desc.block_ptrs = block_map_desc.block_ptrs;
    }

    fn resize_page_cache_and_update_npages(
        &mut self,
        new_size_bytes: usize,
        old_size_bytes: usize,
    ) -> Result<()> {
        self.page_cache.resize(new_size_bytes, old_size_bytes)?;
        self.backend
            .npages
            .store(new_size_bytes.div_ceil(BLOCK_SIZE), Ordering::Release);
        Ok(())
    }

    fn clear_dirty(&mut self) {
        self.desc.clear_dirty();
    }

    fn is_dirty(&self) -> bool {
        self.desc.is_dirty()
    }

    fn inode_type(&self) -> InodeType {
        self.desc.type_
    }

    fn mode(&self) -> InodeMode {
        InodeMode::from_bits_truncate(self.desc.perm.bits())
    }

    fn set_mode(&mut self, mode: InodeMode) {
        self.desc.perm = FilePerm::from_bits_truncate(mode.bits() as u16);
    }

    fn uid(&self) -> u32 {
        self.desc.uid
    }

    fn set_uid(&mut self, uid: u32) {
        self.desc.uid = uid;
    }

    fn gid(&self) -> u32 {
        self.desc.gid
    }

    fn set_gid(&mut self, gid: u32) {
        self.desc.gid = gid;
    }

    fn file_size(&self) -> usize {
        self.desc.size as usize
    }

    fn set_file_size(&mut self, new_size: usize) {
        self.desc.size = new_size as u64;
    }

    fn atime(&self) -> Duration {
        self.desc.atime
    }

    fn set_atime(&mut self, t: Duration) {
        self.desc.atime = t;
    }

    fn mtime(&self) -> Duration {
        self.desc.mtime
    }

    fn set_mtime(&mut self, t: Duration) {
        self.desc.mtime = t;
    }

    fn ctime(&self) -> Duration {
        self.desc.ctime
    }

    fn set_ctime(&mut self, t: Duration) {
        self.desc.ctime = t;
    }

    fn touch_mtime_ctime(&mut self, t: Duration) {
        self.set_mtime(t);
        self.set_ctime(t);
    }

    fn set_dtime(&mut self, t: Duration) {
        self.desc.dtime = t;
    }

    fn link_count(&self) -> u16 {
        self.desc.link_count
    }

    fn add_link_count_saturating(&mut self, delta: u16) {
        self.desc.link_count = self.desc.link_count.saturating_add(delta);
    }

    fn sub_link_count_saturating(&mut self, delta: u16) {
        self.desc.link_count = self.desc.link_count.saturating_sub(delta);
    }

    fn remove_flags(&mut self, flags: FileFlags) {
        self.desc.flags.remove(flags);
    }

    fn set_file_acl(&mut self, file_acl: u32) {
        self.desc.file_acl = file_acl;
    }

    fn fs(&self) -> Result<Arc<Ext2>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))
    }

    /// Returns the maximum on-disk size supported for this inode.
    fn max_size(&self) -> Result<usize> {
        match self.inode_type() {
            InodeType::File => Ok(self.fs()?.max_file_size()),
            _ => Ok(u32::MAX as usize),
        }
    }

    /// Rejects growth beyond the ext2-representable size limit before mutating state.
    fn ensure_size_within_limit(&self, new_size: usize) -> Result<()> {
        if new_size > self.max_size()? {
            return_errno_with_message!(Errno::EFBIG, "inode size exceeds ext2 maximum");
        }

        Ok(())
    }

    fn encode_device_id(&mut self, device_id: u64) -> Result<()> {
        let backend = self.backend().clone();
        let mut block_ptr_tree = backend.block_ptr_tree.write();
        block_ptr_tree.raw_block_ptrs.encode_device_id(device_id);
        let snapshot = *block_ptr_tree.raw_block_ptrs();
        self.sync_desc_block_map_from_snapshot(snapshot);
        Ok(())
    }

    fn persist(&mut self, ino: u32) -> Result<()> {
        let fs = self.fs()?;
        let raw = RawInode::from(&*self.desc);
        fs.write_inode_desc(ino, &raw)?;
        self.clear_dirty();
        Ok(())
    }

    fn sync_metadata(
        &mut self,
        ino: u32,
        block_group_idx: usize,
        sync_inode_table: bool,
    ) -> Result<()> {
        self.persist(ino)?;
        if sync_inode_table {
            let fs = self.fs()?;
            let block_group = fs.block_group(block_group_idx);
            block_group.sync_inode_table()?;
        }
        Ok(())
    }

    /// Initializes an empty directory with `.` and `..` entries.
    ///
    fn make_empty(&mut self, ino: u32, parent_ino: u32) -> Result<()> {
        let fs = self.fs()?;
        let block_size = fs.block_size();

        {
            let block_ptr_tree = self.backend().block_ptr_tree.read();
            if block_ptr_tree.raw_block_ptrs.block_ptrs[0] != 0 {
                return_errno_with_message!(Errno::EIO, "dir block pointer already occupied");
            }
        }

        // Allocate one block for the directory.
        self.prepare_write(0, block_size)?;

        let block = DirBlock::new(self.page_cache(), 0, block_size);
        let dot_len = DirEntryHeader::dir_rec_len(1) as usize;
        let write_result = (|| -> Result<()> {
            block.write_entry(
                0,
                ino,
                DirEntryHeader::dir_rec_len(1),
                b".",
                DirEntryFileType::Dir,
            )?;
            block.write_entry(
                dot_len,
                parent_ino,
                (block_size - dot_len) as u16,
                b"..",
                DirEntryFileType::Dir,
            )?;
            Ok(())
        })();

        if let Err(err) = write_result {
            self.rollback_write(0, block_size);
            return Err(err);
        }

        self.set_file_size(block_size);

        Ok(())
    }

    /// Reads file data directly from data blocks into `writer`.
    ///
    fn read_direct_at(&self, offset: usize, end: usize, writer: &mut VmWriter) -> Result<()> {
        let fs = self.fs()?;
        let block_size = fs.block_size();
        let block_ptr_tree = self.backend.block_ptr_tree.read();

        let mut range_mapper = IoRangeMapper::new(
            (offset / block_size) as u32..(end.div_ceil(block_size)) as u32,
            block_ptr_tree,
            &fs,
        );
        while let Some(range) = range_mapper.next()? {
            match range {
                IoRange::Mapped(mapped_range) => {
                    let nblocks =
                        mapped_range.device_block_range.end - mapped_range.device_block_range.start;
                    let segment = BioSegment::alloc(nblocks as usize, BioDirection::FromDevice);
                    fs.read_blocks(mapped_range.device_block_range.start, segment.clone())?;
                    let mut segment_reader = segment.reader()?;
                    segment_reader.read_fallible(writer)?;
                }
                IoRange::Hole(range) => {
                    let n_bytes = (range.end as usize - range.start as usize) * block_size;
                    writer.fill_zeros(n_bytes)?;
                }
            }
        }
        Ok(())
    }

    /// Writes file data directly to already-allocated data blocks.
    ///
    fn write_direct_at(&self, offset: usize, reader: &mut VmReader) -> Result<()> {
        let fs = self.fs()?;
        let block_size = fs.block_size();
        let write_len = reader.remain();
        debug_assert_eq!(write_len % block_size, 0);
        // end is already checked in `Inode::write_direct_at`.
        let end = offset + write_len;
        let block_ptr_tree = self.backend.block_ptr_tree.read();

        let mut range_mapper = IoRangeMapper::new(
            (offset / block_size) as u32..(end.div_ceil(block_size)) as u32,
            block_ptr_tree,
            &fs,
        );
        while let Some(range) = range_mapper.next()? {
            match range {
                IoRange::Mapped(m) => {
                    // Perform direct write for the continuous block range
                    let nblocks = (m.device_block_range.end - m.device_block_range.start) as usize;
                    let segment = BioSegment::alloc(nblocks, BioDirection::ToDevice);
                    segment.writer()?.write_fallible(reader)?;
                    fs.write_blocks(m.device_block_range.start, segment)?;
                }
                IoRange::Hole(_) => {
                    // TODO: Consider falling back to buffered write like Linux.
                    // The upper layer should have performed allocation for the write
                    // range. Linux does not allocate blocks in the direct write path;
                    // when it encounters a hole it falls back to buffered write to
                    // prevent stale data exposure. We pre-allocate in prepare_write
                    // so holes here indicate a bug. Stale-read is not a concern
                    // because our inode-level lock serializes reads after this write.
                    return_errno_with_message!(Errno::EIO, "unexpected hole in direct write path");
                }
            }
        }
        Ok(())
    }

    /// Checks whether this directory contains only `.` and `..` as live entries.
    ///
    fn empty_dir(&self, self_ino: u32) -> Result<bool> {
        if self.inode_type() != InodeType::Dir {
            return Ok(false);
        }

        let fs = self.fs()?;
        let block_size = fs.block_size();
        let file_size = self.file_size();
        let data_blocks = file_size.div_ceil(block_size);

        for block_idx in 0..data_blocks {
            let block = DirBlock::from_index(self.page_cache(), block_idx, block_size, file_size);
            let block_offset = block_idx * block_size;
            let mut iter = block.iter_entries();

            loop {
                let (_entry_offset, entry) = match iter.next_entry(block_offset) {
                    Ok(Some(pair)) => pair,
                    Ok(None) => break,
                    Err(_) => return Ok(false),
                };

                if entry.header.inode == 0 {
                    continue;
                }

                let name = entry.name.as_bytes();
                if name == b"." {
                    if u32::from_le(entry.header.inode) != self_ino {
                        return Ok(false);
                    }
                    continue;
                }
                if name == b".." {
                    continue;
                }
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Finds a directory entry by name and returns its inode number.
    ///
    fn find_entry(&self, name: &str) -> Result<u32> {
        if self.inode_type() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let fs = self.fs()?;
        let block_size = fs.block_size();
        let file_size = self.file_size();
        let name_bytes = name.as_bytes();

        for block_idx in 0..file_size.div_ceil(block_size) {
            let block_offset = block_idx * block_size;
            let block = DirBlock::from_index(self.page_cache(), block_idx, block_size, file_size);
            let mut iter = block.iter_entries();
            while let Some((_entry_offset, entry)) = iter.next_entry(block_offset)? {
                let ino = u32::from_le(entry.header.inode);
                if ino == 0 {
                    continue;
                }
                if entry.name.as_bytes() == name_bytes {
                    return Ok(ino);
                }
            }
        }

        return_errno!(Errno::ENOENT);
    }

    fn shrink(&mut self, new_size: usize) -> Result<()> {
        let fs = self.fs()?;
        let block_size = fs.block_size();
        let old_size = self.desc.size as usize;

        self.resize_page_cache_and_update_npages(new_size, old_size)?;

        let backend = self.backend.clone();
        let mut block_ptr_tree = backend.block_ptr_tree.write();
        // Block truncation is best-effort, matching Linux ext2 where
        // ext2_truncate_blocks() returns void. Leaked blocks from partial
        // failures are recoverable by e2fsck. Page cache and i_size are
        // already committed, so propagating an error here would leave the
        // inode in a worse inconsistent state.
        let _ = block_ptr_tree.truncate_blocks(&fs, new_size);
        let snapshot = *block_ptr_tree.raw_block_ptrs();

        // Drop the block map lock before fill zeros (might trigger PageCacheBackend.read_page_raw).
        drop(block_ptr_tree);

        // Fill the partial tail of the new EOF block.
        self.zero_eof_tail(new_size, block_size)?;
        self.sync_desc_block_map_from_snapshot(snapshot);
        self.set_file_size(new_size);
        Ok(())
    }

    fn expand(&mut self, new_size: usize) -> Result<()> {
        let fs = self.fs()?;
        let block_size = fs.block_size();
        let old_size = self.file_size();

        if new_size <= old_size {
            return Ok(());
        }
        self.ensure_size_within_limit(new_size)?;
        self.resize_page_cache_and_update_npages(new_size, old_size)?;

        // Zero the partial tail of the old EOF block and the new EOF block.
        self.zero_eof_tail(old_size, block_size)?;
        self.zero_eof_tail(new_size, block_size)?;
        self.set_file_size(new_size);
        Ok(())
    }

    /// Reads directory entries starting at byte offset and feeds visitor.
    ///
    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        if self.inode_type() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let size = self.file_size();
        let min_rec_len = DirEntryHeader::dir_rec_len(1) as usize;
        if size < min_rec_len || offset > size - min_rec_len {
            return Ok(0);
        }

        let fs = self.fs()?;
        let block_size = fs.block_size();

        let start_block = offset / block_size;
        let mut current_offset = offset;
        let mut advanced = 0usize;

        let total_blocks = (size + block_size - 1) / block_size;
        for block_idx in start_block..total_blocks {
            let block_offset = block_idx * block_size;
            if block_offset >= size {
                break;
            }

            let block = DirBlock::from_index(self.page_cache(), block_idx, block_size, size);
            let mut iter = block.iter_entries();
            while let Some((entry_off, entry)) = iter.next_entry(block_offset)? {
                let entry_offset = block_offset + entry_off;
                let rec_len = u16::from_le(entry.header.rec_len) as usize;
                let next_offset = entry_offset + rec_len;

                if next_offset <= current_offset {
                    continue;
                }
                if entry_offset < current_offset {
                    current_offset = next_offset;
                    continue;
                }

                let ino = u32::from_le(entry.header.inode);
                if ino != 0 {
                    let name = core::str::from_utf8(entry.name.as_bytes())
                        .map_err(|_| Error::with_message(Errno::EIO, "invalid dir entry name"))?;
                    let dtype = DirEntryFileType::from(entry.header.file_type);
                    let inode_type = InodeType::from(dtype);
                    if visitor
                        .visit(name, ino as u64, inode_type, next_offset)
                        .is_err()
                    {
                        advanced = current_offset - offset;
                        return Ok(advanced);
                    }
                }

                current_offset = next_offset;
            }

            advanced = current_offset - offset;
        }

        Ok(advanced)
    }

    fn write_link(&mut self, target: &str) -> Result<()> {
        let target_len = target.len();

        // Linux stores symlink targets as C-style strings with a trailing NUL,
        // so reserve one byte for the trailing NUL.
        if target.len() < MAX_FAST_SYMLINK_LEN {
            // Fast path.
            self.desc.block_ptrs.as_mut_bytes()[..target_len].copy_from_slice(target.as_bytes());
        } else {
            // Slow path: write through the page cache.
            self.prepare_write(0, target_len)?;
            self.page_cache().write_bytes(0, target.as_bytes())?;
        }

        self.set_file_size(target_len);
        Ok(())
    }

    fn read_link(&self) -> Result<String> {
        let link_size = self.file_size();
        let fs = self.fs()?;
        let block_size = fs.block_size();

        if self.desc.is_fast_symlink(block_size) {
            // Linux stores symlink targets as C-style strings with a trailing NUL,
            // so reserve one byte for the trailing NUL.
            let read_len = link_size.min(MAX_FAST_SYMLINK_LEN - 1);
            let raw = self.desc.block_ptrs.as_bytes();
            return String::from_utf8(raw[..read_len].to_vec())
                .map_err(|_| Error::with_message(Errno::EIO, "symlink target is not valid UTF-8"));
        }

        let mut target = vec![0u8; link_size];
        self.page_cache().read_bytes(0, &mut target).map_err(|_| {
            Error::with_message(Errno::EIO, "failed to read symlink target from page cache")
        })?;

        String::from_utf8(target)
            .map_err(|_| Error::with_message(Errno::EIO, "symlink target is not valid UTF-8"))
    }

    // 1. Zero the old partial tail if the write extends EOF.
    // 2. Zero the partial head and tail for the current write.
    // 3. Allocate new blocks for the write range.
    // 4. Fill zeros for newly exposed partial ranges.
    fn prepare_write(&mut self, offset: usize, end: usize) -> Result<()> {
        let fs = self.fs()?;
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }
        let old_size = self.file_size();
        if end > old_size {
            self.ensure_size_within_limit(end)?;
            self.resize_page_cache_and_update_npages(end, old_size)?;
        }

        // Note: zero-filling runs before block allocation, and dirty pages are
        // never evicted by memory pressure (no capacity-based eviction yet), so
        // concurrent mmap readers always see zeros or valid data from the page
        // cache. If capacity-based eviction is added in the future, per-page
        // locks (like Linux's folio lock) will be needed to prevent stale reads
        // from newly allocated blocks whose zero-filled page has been evicted.

        // If the write extends EOF, zero the partial tail of the old EOF block.
        if offset > old_size {
            self.zero_eof_tail(old_size, block_size)?;
        }

        // Treat the write end as the new partial EOF boundary.
        self.zero_partial_writes(offset, end, block_size)?;
        self.allocate_range_blocks(offset, end, block_size)?;
        Ok(())
    }
    // NOTE: Make sure the page cache is already resized before calling this function.
    // When the file expands, zero the partial tail of the old EOF block. EOF
    // cleanup belongs to the operation that changes the file size, such as
    // write, shrink, expand, or fallocate.
    fn zero_eof_tail(&self, eof: usize, block_size: usize) -> Result<()> {
        if !eof.is_multiple_of(block_size) {
            let block_end = eof.align_up(block_size);
            self.page_cache().fill_zeros(eof..block_end)?;
        }
        Ok(())
    }

    // NOTE: Make sure the page cache is already resized before calling this function.
    // Use this only on write paths for file data, symlinks, and directories.
    // Conditionally fill zeros for:
    // 1. The partial start block when it is a hole.
    // 2. The partial end block when it is a hole.
    fn zero_partial_writes(&mut self, start: usize, end: usize, block_size: usize) -> Result<()> {
        let fs = self.fs()?;
        let start_iblock = (start / block_size) as u32;
        let end_iblock = (end / block_size) as u32;

        // If the new start block is a hole and not aligned to block size,
        // we need to fill zeros to the partial block.

        let block_ptr_tree = self.backend().block_ptr_tree.read();

        if !start.is_multiple_of(block_size)
            && block_ptr_tree.lookup_block(&fs, start_iblock)?.is_none()
        {
            let new_start_block = start.align_down(block_size);
            self.page_cache().fill_zeros(new_start_block..start)?;
        }

        if !end.is_multiple_of(block_size)
            && block_ptr_tree.lookup_block(&fs, end_iblock)?.is_none()
        {
            let new_end_block = end.align_up(block_size);
            self.page_cache().fill_zeros(end..new_end_block)?;
        }
        Ok(())
    }

    /// Allocates missing data blocks that cover the requested file byte range.
    ///
    fn allocate_range_blocks(
        &mut self,
        offset: usize,
        end: usize,
        block_size: usize,
    ) -> Result<Vec<Ext2Bid>> {
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }
        if end <= offset {
            return Ok(Vec::new());
        }

        let fs = self.fs()?;
        let start_block = offset / block_size;
        let end_block = end.div_ceil(block_size);

        let (block_map_desc, new_blocks, alloc_result) = {
            let backend = self.backend().clone();
            let mut block_ptr_tree = backend.block_ptr_tree.write();
            let mut new_blocks = Vec::new();
            let alloc_result = (|| -> Result<()> {
                let mut current_block = start_block;
                while current_block < end_block {
                    let iblock = u32::try_from(current_block).map_err(|_| {
                        Error::with_message(Errno::EINVAL, "logical block number overflow")
                    })?;
                    let remaining = u32::try_from(end_block - current_block).map_err(|_| {
                        Error::with_message(Errno::EINVAL, "logical block range overflow")
                    })?;

                    if let Some(mapped_range) =
                        block_ptr_tree.lookup_block_range(&fs, iblock, remaining)?
                    {
                        current_block +=
                            mapped_range.end.saturating_sub(mapped_range.start) as usize;
                        continue;
                    }
                    let allocated_range = block_ptr_tree
                        .lookup_or_alloc_block_range(&fs, iblock, remaining, true)?
                        .ok_or_else(|| {
                            Error::with_message(
                                Errno::EIO,
                                "missing block mapping after allocation",
                            )
                        })?;
                    current_block +=
                        allocated_range.end.saturating_sub(allocated_range.start) as usize;
                    new_blocks.extend(allocated_range);
                }
                Ok(())
            })();
            (*block_ptr_tree.raw_block_ptrs(), new_blocks, alloc_result)
        };
        self.sync_desc_block_map_from_snapshot(block_map_desc);

        alloc_result?;
        Ok(new_blocks)
    }

    // TODO: Maybe zeroing only the page cache is sufficient here.
    /// Zeroes newly allocated data blocks before exposing them via mapped reads.
    ///
    fn zero_new_blocks(&self, blocks: &[Ext2Bid]) -> Result<()> {
        if blocks.is_empty() {
            return Ok(());
        }

        let fs = self.fs()?;
        let block_size = fs.block_size();
        let zero_block = vec![0u8; block_size];
        for &bid in blocks {
            let bio_segment = BioSegment::alloc(1, BioDirection::ToDevice);
            {
                let mut segment_writer = bio_segment.writer().map_err(|_| {
                    Error::with_message(Errno::EIO, "failed to access zero-write bio segment")
                })?;
                let mut zero_reader = VmReader::from(zero_block.as_slice()).to_fallible();
                segment_writer.write_fallible(&mut zero_reader)?;
            }
            fs.write_blocks(bid, bio_segment).map_err(|_| {
                Error::with_message(Errno::EIO, "failed to zero newly allocated data block")
            })?;
        }
        Ok(())
    }

    // Truncate page cache and blocks after a failed write.
    fn rollback_write(&mut self, old_size: usize, end: usize) {
        if end <= old_size {
            return;
        }
        if let Err(err) = self.resize_page_cache_and_update_npages(old_size, end) {
            error!(
                "ext2: write_at cleanup page cache resize failed: old_size={}, err={:?}",
                old_size, err
            );
        }

        let Ok(fs) = self.fs() else {
            error!("ext2: rollback_write: filesystem already dropped");
            return;
        };
        let backend = self.backend().clone();
        let mut block_ptr_tree = backend.block_ptr_tree.write();
        if let Err(err) = block_ptr_tree.truncate_blocks(&fs, old_size) {
            error!(
                "ext2: write_at cleanup truncate_blocks failed: old_size={}, err={:?}",
                old_size, err
            );
        }
        let desc = *block_ptr_tree.raw_block_ptrs();
        self.sync_desc_block_map_from_snapshot(desc);
    }

    /// Scans directory blocks for a reusable slot or a duplicate entry.
    ///
    fn scan_dir_for_slot(&self, name: &str) -> Result<Option<DirSlotInfo>> {
        if self.inode_type() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > u8::MAX as usize {
            return_errno!(Errno::EINVAL);
        }

        let fs = self.fs()?;
        let block_size = fs.block_size();
        let reclen = DirEntryHeader::dir_rec_len(name_bytes.len()) as usize;
        if reclen > block_size {
            return_errno_with_message!(Errno::ENOSPC, "dir entry too large for block");
        }

        let file_size = self.file_size();
        let data_blocks = file_size.div_ceil(block_size);

        for block_idx in 0..data_blocks {
            let block_offset = block_idx * block_size;
            let block = DirBlock::from_index(self.page_cache(), block_idx, block_size, file_size);
            let mut iter = block.iter_entries();

            while let Some((entry_offset, entry)) = iter.next_entry(block_offset)? {
                let ino = u32::from_le(entry.header.inode);
                let rec_len = u16::from_le(entry.header.rec_len) as usize;

                // Check for duplicate name.
                if ino != 0 && entry.name.as_bytes() == name_bytes {
                    return_errno!(Errno::EEXIST);
                }

                let used_len = if ino == 0 {
                    0
                } else {
                    DirEntryHeader::dir_rec_len(entry.header.name_len as usize) as usize
                };

                // Free entry can be reused, occupied entry can be split.
                if (ino == 0 && rec_len >= reclen)
                    || (ino != 0 && rec_len >= used_len.saturating_add(reclen))
                {
                    return Ok(Some(DirSlotInfo {
                        dir_offset: block_offset + entry_offset,
                        slot_rec_len: rec_len,
                        used_rec_len: used_len,
                    }));
                }
            }
        }

        Ok(None)
    }

    /// Grows the directory by one data block.
    ///
    fn grow_dir_block(&mut self) -> Result<DirSlotInfo> {
        let fs = self.fs()?;
        let block_size = fs.block_size();
        let old_size = self.file_size();

        self.prepare_write(old_size, old_size + block_size)?;
        let new_size = old_size.saturating_add(block_size);
        self.set_file_size(new_size);

        Ok(DirSlotInfo {
            dir_offset: old_size,
            slot_rec_len: block_size,
            used_rec_len: 0,
        })
    }

    /// Writes a new entry into the selected slot through `PageCache`.
    ///
    fn add_entry(
        &self,
        slot: &DirSlotInfo,
        name: &str,
        ino: u32,
        ft: DirEntryFileType,
    ) -> Result<()> {
        let fs = self.fs()?;
        let max_inumber = fs.super_block().total_inodes();
        if ino == 0 || ino > max_inumber {
            return_errno!(Errno::EINVAL);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > u8::MAX as usize {
            return_errno!(Errno::EINVAL);
        }

        let entry_reclen = DirEntryHeader::dir_rec_len(name_bytes.len()) as usize;
        if entry_reclen > slot.slot_rec_len {
            return_errno_with_message!(Errno::ENOSPC, "slot too small for dir entry");
        }

        let mut offset = slot.dir_offset;
        let mut rec_len = slot.slot_rec_len;
        if slot.used_rec_len != 0 {
            if slot.used_rec_len >= slot.slot_rec_len {
                return_errno_with_message!(Errno::EIO, "corrupted dir entry split");
            }
            // When splitting, update the predecessor's rec_len first.
            self.page_cache.write_bytes(
                slot.dir_offset.saturating_add(4),
                &(slot.used_rec_len as u16).to_le_bytes(),
            )?;
            offset = slot.dir_offset.saturating_add(slot.used_rec_len);
            rec_len = slot.slot_rec_len.saturating_sub(slot.used_rec_len);
        }

        let block = DirBlock::new(self.page_cache(), offset, rec_len);
        block.write_entry(0, ino, rec_len as u16, name_bytes, ft)?;
        Ok(())
    }

    /// Locate a target entry by name for delete/set_link operations.
    ///
    fn find_entry_target(&self, name: &str) -> Result<DirEntryTarget> {
        let fs = self.fs()?;
        let block_size = fs.block_size();
        let file_size = self.file_size();
        let name_bytes = name.as_bytes();

        for block_idx in 0..file_size.div_ceil(block_size) {
            let block_offset = block_idx * block_size;
            let block = DirBlock::from_index(self.page_cache(), block_idx, block_size, file_size);
            let mut iter = block.iter_entries();
            while let Some((entry_offset, entry)) = iter.next_entry(block_offset)? {
                let ino = u32::from_le(entry.header.inode);
                if ino == 0 {
                    continue;
                }
                if entry.name.as_bytes() == name_bytes {
                    return Ok(DirEntryTarget {
                        dir_offset: block_offset + entry_offset,
                        entry_rec_len: u16::from_le(entry.header.rec_len) as usize,
                    });
                }
            }
        }

        return_errno!(Errno::ENOENT)
    }

    /// Deletes a located entry by zeroing inode and merging rec_len.
    ///
    fn delete_entry(&self, target: &DirEntryTarget) -> Result<()> {
        let fs = self.fs()?;
        let block_size = fs.block_size();
        let block_base = (target.dir_offset / block_size) * block_size;
        let block_idx = block_base / block_size;
        let entry_offset = target.dir_offset - block_base;

        let block =
            DirBlock::from_index(self.page_cache(), block_idx, block_size, self.file_size());
        block.delete_entry(block_size, entry_offset, target.entry_rec_len)?;
        Ok(())
    }

    /// Rewrites a located entry's inode/type.
    ///
    fn set_link(&self, target: &DirEntryTarget, new_ino: u32, ft: DirEntryFileType) -> Result<()> {
        let fs = self.fs()?;
        let block_size = fs.block_size();
        let block_base = (target.dir_offset / block_size) * block_size;
        let block_idx = block_base / block_size;
        let entry_offset = target.dir_offset - block_base;

        let block =
            DirBlock::from_index(self.page_cache(), block_idx, block_size, self.file_size());
        block.set_inode(entry_offset, new_ino)?;
        block.set_file_type(entry_offset, ft)?;
        Ok(())
    }

    fn sync_data_pages(&self) -> Result<()> {
        // A file_write_and_wait_range on an empty file is a no-op.
        let file_size = self.file_size();
        if file_size == 0 {
            return Ok(());
        }

        // The evict_range writes back dirty pages in [0, file_size), waits for
        // completion, and keeps pages cached as UpToDate.
        self.page_cache.flush_range(0..file_size)
    }
}

/// Acquires `inner.read()` locks on two inodes in ascending ino order.
/// Returns guards in `(a, b)` order regardless of which ino is smaller.
fn _read_lock_two_inodes<'a>(
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

/// Acquires `inner.write()` locks on two inodes in ascending ino order.
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

/// Acquires `inner.write()` locks on an arbitrary number of inodes in ascending ino order.
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

bitflags! {
    struct FileFlags: u32 {
        /// Secure deletion.
        const SECURE_DEL = 1 << 0;
        /// Undelete.
        const UNDELETE = 1 << 1;
        /// Compresses the file.
        const COMPRESS = 1 << 2;
        /// Synchronous updates.
        const SYNC_UPDATE = 1 << 3;
        /// Immutable file.
        const IMMUTABLE = 1 << 4;
        /// Append only.
        const APPEND_ONLY = 1 << 5;
        /// Do not dump file.
        const NO_DUMP = 1 << 6;
        /// Does not update `atime`.
        const NO_ATIME = 1 << 7;
        /// Dirty.
        const DIRTY = 1 << 8;
        /// One or more compressed clusters.
        const COMPRESS_BLK = 1 << 9;
        /// Does not compress.
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
        /// Dirsync behavior (directories only).
        const DIR_SYNC = 1 << 16;
        /// Top of directory hierarchies.
        const TOP_DIR = 1 << 17;
        /// Reserved for the ext2 library.
        const RESERVED = 1 << 31;
    }
}

/// Parsed in-memory mirror of an on-disk inode's metadata fields.
///
/// Unlike [`RawInode`], fields are decoded into Rust types
/// (e.g., `Duration` for timestamps, [`InodeType`] for file type).
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
    link_count: u16,
    sector_count: u32,
    flags: FileFlags,
    file_acl: u32,
    generation: u32,
    block_ptrs: [u32; 15],
}

impl InodeDesc {
    pub(super) fn type_(&self) -> InodeType {
        self.type_
    }

    /// Determines whether the symlink payload is stored inline in `i_block[15]`.
    ///
    fn is_fast_symlink(&self, block_size: usize) -> bool {
        let ea_blocks = if self.file_acl != 0 {
            (block_size / SECTOR_SIZE) as u32
        } else {
            0
        };

        self.type_ == InodeType::SymLink && self.sector_count.checked_sub(ea_blocks) == Some(0)
    }
}

impl TryFrom<&RawInode> for InodeDesc {
    type Error = Error;
    fn try_from(raw: &RawInode) -> Result<Self> {
        if raw.link_count == 0 {
            return_errno_with_message!(Errno::ESTALE, "inode has been deleted");
        }

        let mode = raw.mode;
        let type_ = InodeType::from_raw_mode(mode)?;
        let perm = FilePerm::from_bits_truncate(mode & 0o7777);
        let uid = (raw.uid as u32) | ((raw.uid_high as u32) << 16);
        let gid = (raw.gid as u32) | ((raw.gid_high as u32) << 16);
        let atime = Duration::from_secs(raw.atime as u64);
        let ctime = Duration::from_secs(raw.ctime as u64);
        let mtime = Duration::from_secs(raw.mtime as u64);

        let mut size = raw.size_lo as u64;
        if type_ == InodeType::File {
            size |= (raw.size_high as u64) << 32;
        }
        if size > i64::MAX as u64 {
            return_errno_with_message!(Errno::EUCLEAN, "corrupted inode on disk");
        }

        let flags = FileFlags::from_bits(raw.flags)
            .ok_or_else(|| Error::with_message(Errno::EIO, "invalid inode flags"))?;
        let block_ptr_tree = RawBlockPtrs::from_raw(raw);

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
            link_count: raw.link_count,
            sector_count: block_ptr_tree.sector_count,
            flags,
            file_acl: raw.file_acl,
            generation: raw.generation,
            block_ptrs: block_ptr_tree.block_ptrs,
        })
    }
}

impl From<&InodeDesc> for RawInode {
    fn from(desc: &InodeDesc) -> Self {
        let mode = (desc.type_ as u16) | (desc.perm.0 & 0o7777);
        let uid = desc.uid as u16;
        let gid = desc.gid as u16;
        let uid_high = (desc.uid >> 16) as u16;
        let gid_high = (desc.gid >> 16) as u16;

        let (size_lo, size_high) = if desc.type_ == InodeType::File {
            (desc.size as u32, (desc.size >> 32) as u32)
        } else {
            (desc.size as u32, 0)
        };

        Self {
            mode,
            uid,
            size_lo,
            atime: desc.atime.as_secs() as u32,
            ctime: desc.ctime.as_secs() as u32,
            mtime: desc.mtime.as_secs() as u32,
            dtime: desc.dtime.as_secs() as u32,
            gid,
            link_count: desc.link_count,
            sector_count: desc.sector_count,
            flags: desc.flags.bits(),
            osd1: 0,
            block: desc.block_ptrs,
            generation: desc.generation,
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
    pub link_count: u16,   // i_link_count
    pub sector_count: u32, // i_blocks (512-byte sectors)
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

#[cfg(ktest)]
mod test {
    use core::time::Duration;

    use ostd::{mm::VmIo, prelude::ktest};

    use super::*;
    use crate::{
        fs::{
            file::StatusFlags,
            fs_impls::ext2::{
                fs::ROOT_INO,
                testkit::{
                    self, CollectDirentVisitor, ErrorBioDisk, Ext2FixtureBuilder, RawInodeBuilder,
                    StopAfterVisitor, encode_dir_entry,
                },
            },
            vfs::{
                inode::{Inode as VfsInodeTrait, InodeIo},
                xattr::{XattrName, XattrNamespace, XattrSetFlags},
            },
        },
        prelude::*,
        time::clocks,
    };

    fn make_raw_inode(mode: u16) -> RawInode {
        RawInodeBuilder::new(mode).build()
    }

    fn lookup_ino(dir: &Arc<Inode>, name: &str) -> Result<u32> {
        Ok(dir.lookup(name)?.ino())
    }

    fn inode_size(inode: &Arc<Inode>) -> usize {
        VfsInodeTrait::size(inode.as_ref())
    }

    fn inode_nlinks(inode: &Arc<Inode>) -> usize {
        VfsInodeTrait::metadata(inode.as_ref()).nr_hard_links
    }

    fn read_raw_inode_from_disk(f: &testkit::Ext2Fixture, ino: u32) -> RawInode {
        let inodes_per_group = f.sb.inodes_per_group();
        let group_idx = ((ino - 1) / inodes_per_group) as usize;
        let index_in_group = (ino - 1) % inodes_per_group;
        let inode_size = f.sb.inode_size();
        let block_size = f.sb.block_size();
        let offset_bytes = (index_in_group as usize) * inode_size;
        let block_index = offset_bytes / block_size;
        let offset_in_block = offset_bytes % block_size;
        let table_block = f.descs[group_idx].inode_table + block_index as u32;
        f.disk
            .segment()
            .read_val(Bid::new(table_block as u64).to_offset() + offset_in_block)
            .unwrap()
    }

    fn make_live_dir_inode(
        ext2: &Arc<Ext2>,
        ino: u32,
        size: usize,
        sector_count: u32,
        flags: FileFlags,
        block_ptrs: [u32; 15],
    ) -> Arc<Inode> {
        let mut raw = make_raw_inode(0o040755);
        raw.size_lo = size as u32;
        raw.sector_count = sector_count;
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
        sector_count: u32,
        flags: FileFlags,
        block_ptrs: [u32; 15],
    ) -> Arc<Inode> {
        let mut raw = make_raw_inode(0o100644);
        raw.size_lo = size as u32;
        raw.sector_count = sector_count;
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

    #[ktest]
    fn vfs_inode_sync_all_flushes_device_once() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "sync_me",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();

        VfsInodeTrait::sync_all(file.as_ref()).unwrap();
        assert_eq!(f.disk.flush_count(), 1);
    }

    #[ktest]
    fn namei_create_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();

        // Allocate the inode before publishing its directory entry.
        let created = root
            .create(
                "alpha",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        assert_eq!(lookup_ino(&root, "alpha").unwrap(), created.ino());
        assert_eq!(inode_nlinks(&created), 1);

        // A new directory starts with `.` and `..`, and increments the parent link count.
        let created_dir = root
            .create("sub", InodeType::Dir, FilePerm::from_bits_truncate(0o755))
            .unwrap();
        assert_eq!(lookup_ino(&root, "sub").unwrap(), created_dir.ino());
        assert_eq!(inode_nlinks(&created_dir), 2);
        assert_eq!(inode_nlinks(&root), 3);

        assert_eq!(
            root.create(".", InodeType::File, FilePerm::from_bits_truncate(0o644))
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
        let free_inodes_before_dup = f.ext2.super_block().free_inodes_count();
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
            f.ext2.super_block().free_inodes_count(),
            free_inodes_before_dup
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
    fn namei_link_unlink_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();

        let old = root
            .create("old", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();
        let old_ino = old.ino();
        let old_links_before = inode_nlinks(&old) as u16;
        let block_size = f.ext2.block_size();
        let payload = vec![0x6au8; block_size];
        let mut payload_reader = VmReader::from(payload.as_slice()).to_fallible();
        old.write_direct_at(0, &mut payload_reader).unwrap();

        // Increase the target link count before publishing the new name.
        root.link(&old, "alias").unwrap();
        assert_eq!(lookup_ino(&root, "alias").unwrap(), old_ino);
        assert_eq!(inode_nlinks(&old) as u16, old_links_before + 1);

        let dir = root
            .create("dir", InodeType::Dir, FilePerm::from_bits_truncate(0o755))
            .unwrap();
        assert_eq!(
            root.link(&dir, "dir_hard").unwrap_err().error(),
            Errno::EPERM
        );

        // Remove the name before dropping the target link count.
        root.unlink("alias").unwrap();
        assert_eq!(
            lookup_ino(&root, "alias").unwrap_err().error(),
            Errno::ENOENT
        );
        assert_eq!(inode_nlinks(&old) as u16, old_links_before);

        assert_eq!(root.unlink("dir").unwrap_err().error(), Errno::EISDIR);
        assert_eq!(root.unlink(".").unwrap_err().error(), Errno::EINVAL);

        let free_blocks_before_sync = f.ext2.super_block().free_blocks_count();
        root.unlink("old").unwrap();
        assert_eq!(
            f.ext2.read_inode(old_ino).unwrap_err().error(),
            Errno::ESTALE
        );
        assert!(
            f.ext2
                .block_group(0)
                .metadata()
                .inode_bitmap
                .is_allocated((old_ino - 1) as u16)
        );
        f.ext2.sync_all().unwrap();
        let raw_before_drop = read_raw_inode_from_disk(&f, old_ino);
        assert_eq!(raw_before_drop.link_count, 0);
        assert_eq!(raw_before_drop.dtime, 0);
        assert_ne!(raw_before_drop.sector_count, 0);

        drop(old);
        f.ext2.sync_all().unwrap();
        assert_eq!(
            f.ext2.read_inode(old_ino).unwrap_err().error(),
            Errno::ENOENT
        );
        assert!(
            !f.ext2
                .block_group(0)
                .metadata()
                .inode_bitmap
                .is_allocated((old_ino - 1) as u16)
        );
        let raw_after_drop = read_raw_inode_from_disk(&f, old_ino);
        assert_eq!(raw_after_drop.link_count, 0);
        assert_eq!(raw_after_drop.sector_count, 0);
        assert_eq!(raw_after_drop.block[0], 0);
        assert_eq!(
            f.ext2.super_block().free_blocks_count(),
            free_blocks_before_sync.saturating_add(1)
        );
    }

    #[ktest]
    fn namei_rename_ok() {
        clocks::init_for_ktest();
        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();

        let src = root
            .create("src", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();
        root.create(
            "target",
            InodeType::File,
            FilePerm::from_bits_truncate(0o644),
        )
        .unwrap();

        // Rename to itself is a no-op success.
        root.rename("target", &root, "target").unwrap();

        // Same-dir no-replacement path should insert new name then delete old name.
        let solo = root
            .create("solo", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();
        let solo_ino = solo.ino();
        root.rename("solo", &root, "solo_renamed").unwrap();
        assert_eq!(lookup_ino(&root, "solo_renamed").unwrap(), solo_ino);
        assert_eq!(
            lookup_ino(&root, "solo").unwrap_err().error(),
            Errno::ENOENT
        );

        // Cross-dir no-replacement path should use the same add-entry lock protocol.
        let dst_dir = root
            .create("dst", InodeType::Dir, FilePerm::from_bits_truncate(0o755))
            .unwrap();
        let move_src = root
            .create(
                "move_src",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        let move_src_ino = move_src.ino();
        root.rename("move_src", &dst_dir, "move_dst").unwrap();
        assert_eq!(
            lookup_ino(&root, "move_src").unwrap_err().error(),
            Errno::ENOENT
        );
        assert_eq!(lookup_ino(&dst_dir, "move_dst").unwrap(), move_src_ino);

        // Cross-dir replacement path should set-link destination then delete source.
        let src_dir = root
            .create(
                "src_dir",
                InodeType::Dir,
                FilePerm::from_bits_truncate(0o755),
            )
            .unwrap();
        let dst_replace_dir = root
            .create(
                "dst_replace",
                InodeType::Dir,
                FilePerm::from_bits_truncate(0o755),
            )
            .unwrap();
        let moving = src_dir
            .create(
                "moving",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        let replaced = dst_replace_dir
            .create(
                "target",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        let moving_ino = moving.ino();
        let replaced_ino_cross = replaced.ino();
        src_dir
            .rename("moving", &dst_replace_dir, "target")
            .unwrap();
        assert_eq!(
            lookup_ino(&src_dir, "moving").unwrap_err().error(),
            Errno::ENOENT
        );
        assert_eq!(lookup_ino(&dst_replace_dir, "target").unwrap(), moving_ino);

        // Directory move should update `..` to the new parent.
        let parent_a = root
            .create(
                "parent_a",
                InodeType::Dir,
                FilePerm::from_bits_truncate(0o755),
            )
            .unwrap();
        let parent_b = root
            .create(
                "parent_b",
                InodeType::Dir,
                FilePerm::from_bits_truncate(0o755),
            )
            .unwrap();
        parent_a
            .create("kid", InodeType::Dir, FilePerm::from_bits_truncate(0o755))
            .unwrap();
        assert_eq!(inode_nlinks(&parent_a), 3);
        assert_eq!(inode_nlinks(&parent_b), 2);
        parent_a.rename("kid", &parent_b, "kid_moved").unwrap();
        let moved_dir = parent_b.lookup("kid_moved").unwrap();
        assert_eq!(lookup_ino(&moved_dir, "..").unwrap(), parent_b.ino());
        assert_eq!(inode_nlinks(&parent_a), 2);
        assert_eq!(inode_nlinks(&parent_b), 3);

        // Directory replacement with empty dir should drop exactly one parent link.
        let empty_dst = parent_b
            .create(
                "empty_dst",
                InodeType::Dir,
                FilePerm::from_bits_truncate(0o755),
            )
            .unwrap();
        let empty_dst_ino = empty_dst.ino();
        let moved_dir_ino = moved_dir.ino();
        assert_eq!(inode_nlinks(&parent_b), 4);
        parent_b
            .rename("kid_moved", &parent_b, "empty_dst")
            .unwrap();
        assert_eq!(lookup_ino(&parent_b, "empty_dst").unwrap(), moved_dir_ino);
        assert_eq!(
            lookup_ino(&parent_b, "kid_moved").unwrap_err().error(),
            Errno::ENOENT
        );
        assert_eq!(inode_nlinks(&parent_b), 3);

        let old = root
            .create("old", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();
        let new = root
            .create("new", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();
        let old_ino = old.ino();
        let replaced_ino = new.ino();

        // Replacement rename overwrites the destination and removes the old name.
        root.rename("old", &root, "new").unwrap();
        assert_eq!(lookup_ino(&root, "new").unwrap(), old_ino);
        assert_eq!(lookup_ino(&root, "old").unwrap_err().error(), Errno::ENOENT);
        assert_eq!(
            f.ext2.read_inode(replaced_ino).unwrap_err().error(),
            Errno::ESTALE
        );
        assert_eq!(
            f.ext2.read_inode(replaced_ino_cross).unwrap_err().error(),
            Errno::ESTALE
        );
        drop(replaced);
        drop(new);
        f.ext2.sync_all().unwrap();
        assert_eq!(
            f.ext2.read_inode(replaced_ino).unwrap_err().error(),
            Errno::ENOENT
        );
        assert_eq!(
            f.ext2.read_inode(replaced_ino_cross).unwrap_err().error(),
            Errno::ENOENT
        );
        drop(empty_dst);
        f.ext2.sync_all().unwrap();
        assert_eq!(
            f.ext2.read_inode(empty_dst_ino).unwrap_err().error(),
            Errno::ENOENT
        );

        let _ = src;
    }

    #[ktest]
    fn namei_cross_fs_exdev() {
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
    fn inode_desc_from_raw_ok() {
        let mut raw = make_raw_inode(0o100644);
        raw.size_lo = 0x1122_3344;
        raw.size_high = 0x5566_7788;
        raw.uid = 0x1234;
        raw.uid_high = 0x5678;
        raw.gid = 0x4321;
        raw.gid_high = 0x8765;
        raw.sector_count = 99;
        raw.block[0] = 42;
        raw.dtime = 123;

        let desc = InodeDesc::try_from(&raw).unwrap();
        assert_eq!(desc.size, 0x5566_7788_1122_3344);
        assert_eq!(desc.uid, 0x5678_1234);
        assert_eq!(desc.gid, 0x8765_4321);
        assert_eq!(desc.sector_count, 99);
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
    fn inode_desc_from_raw_err() {
        let mut deleted_inode = make_raw_inode(0);
        deleted_inode.link_count = 0;
        deleted_inode.dtime = 1;
        let deleted_err = InodeDesc::try_from(&deleted_inode).unwrap_err();
        assert_eq!(deleted_err.error(), Errno::ESTALE);

        let mut zero_link_live_inode = make_raw_inode(0o100644);
        zero_link_live_inode.link_count = 0;
        let zero_link_err = InodeDesc::try_from(&zero_link_live_inode).unwrap_err();
        assert_eq!(zero_link_err.error(), Errno::ESTALE);

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

        assert_eq!(lookup_ino(&root, "foo").unwrap(), foo.ino());
        assert_eq!(lookup_ino(&root, "subdir").unwrap(), subdir.ino());
        assert_eq!(
            lookup_ino(&root, "missing").unwrap_err().error(),
            Errno::ENOENT
        );

        let mut visitor = CollectDirentVisitor::default();
        root.readdir_at(0, &mut visitor).unwrap();
        // Root has ".", "..", "foo", "subdir".
        assert_eq!(visitor.entries.len(), 4);
        assert_eq!(visitor.entries[0].0, ".");
        assert_eq!(visitor.entries[1].0, "..");
        assert_eq!(visitor.entries[2].0, "foo");
        assert_eq!(visitor.entries[2].2, InodeType::File);
        assert_eq!(visitor.entries[3].0, "subdir");
        assert_eq!(visitor.entries[3].2, InodeType::Dir);
        assert_eq!(visitor.entries[0].3, 12);
        assert_eq!(visitor.entries[1].3, 24);
        assert_eq!(visitor.entries[2].3, 36);
        assert_eq!(visitor.entries[3].3, inode_size(&root));

        let mut stop_visitor = StopAfterVisitor::new(2);
        let stop_advanced = root.readdir_at(0, &mut stop_visitor).unwrap();
        let root_size = inode_size(&root);
        assert!(stop_advanced > 0 && stop_advanced < root_size);

        let first_entry_end = visitor.entries[0].3;
        let mut offset_visitor = CollectDirentVisitor::default();
        root.readdir_at(first_entry_end, &mut offset_visitor)
            .unwrap();
        assert_eq!(offset_visitor.entries.len(), 3);
        assert_eq!(offset_visitor.entries[0].0, "..");
        assert_eq!(offset_visitor.entries[0].3, visitor.entries[1].3);
    }

    #[ktest]
    fn dir_lookup_readdir_err() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let (disk, ext2) = (&f.disk, &f.ext2);
        let block_size = ext2.block_size();

        let mut file_ptrs = [0u32; 15];
        file_ptrs[0] = 80;
        let file_inode = make_live_file_inode(ext2, 50, 0, 0, FileFlags::empty(), file_ptrs);
        assert_eq!(
            lookup_ino(&file_inode, "foo").unwrap_err().error(),
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

        let hole_inode = make_live_dir_inode(ext2, 3, 12, 8, FileFlags::empty(), [0u32; 15]);
        assert_eq!(
            lookup_ino(&hole_inode, "foo").unwrap_err().error(),
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
            make_live_dir_inode(ext2, 4, block_size, 0, FileFlags::empty(), ptrs);
        assert_eq!(
            lookup_ino(&limited_blocks_inode, "missing")
                .unwrap_err()
                .error(),
            Errno::ENOENT
        );

        let tiny_dir_inode = make_live_dir_inode(ext2, 5, 11, 8, FileFlags::empty(), ptrs);
        let mut vec_visitor = Vec::<String>::new();
        assert_eq!(tiny_dir_inode.readdir_at(0, &mut vec_visitor).unwrap(), 0);

        let mut vec_visitor = Vec::<String>::new();
        assert_eq!(
            limited_blocks_inode
                .readdir_at(block_size - 11, &mut vec_visitor)
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
        let bad_inode = make_live_dir_inode(ext2, 6, 12, 8, FileFlags::empty(), bad_ptrs);
        assert_eq!(lookup_ino(&bad_inode, ".").unwrap_err().error(), Errno::EIO);
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
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();

        let bar = root
            .create("bar", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();

        assert_eq!(lookup_ino(&root, "bar").unwrap(), bar.ino());

        let foo = root
            .create("foo", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();
        assert_eq!(lookup_ino(&root, "foo").unwrap(), foo.ino());

        let dup = root
            .create("foo", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap_err();
        assert_eq!(dup.error(), Errno::EEXIST);

        root.unlink("foo").unwrap();

        assert_eq!(lookup_ino(&root, ".").unwrap(), ROOT_INO);
        assert_eq!(lookup_ino(&root, "bar").unwrap(), bar.ino());
        assert_eq!(lookup_ino(&root, "foo").unwrap_err().error(), Errno::ENOENT);
    }

    #[ktest]
    fn dir_grow_lookup_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let block_size = f.ext2.block_size();

        let size_before = inode_size(&root);
        let name_pad = "x".repeat(240);
        let mut first_name = None::<String>;
        let mut last_name = None::<String>;

        // Create enough entries to force directory growth beyond the direct block pointers.
        // This validates user-visible behavior (lookup/readdir) without inspecting inode internals.
        for idx in 0..200u32 {
            let name = alloc::format!("e{idx:04}{name_pad}");
            if first_name.is_none() {
                first_name = Some(name.clone());
            }
            last_name = Some(name.clone());

            root.create(&name, InodeType::File, FilePerm::from_bits_truncate(0o644))
                .unwrap();
        }

        let size_after = inode_size(&root);
        assert!(size_after > size_before);
        assert!(size_after >= size_before.saturating_add(block_size));

        let first = first_name.unwrap();
        let last = last_name.unwrap();
        assert!(root.lookup(&first).is_ok());
        assert!(root.lookup(&last).is_ok());

        let mut visitor = CollectDirentVisitor::default();
        root.readdir_at(0, &mut visitor).unwrap();
        assert!(visitor.entries.len() >= 4);
    }

    #[ktest]
    fn dir_mutation_err() {
        let f = Ext2FixtureBuilder::new(2, 256).build().unwrap();
        let ext2 = &f.ext2;

        let file_inode = make_live_file_inode(ext2, 50, 0, 0, FileFlags::empty(), [0u32; 15]);
        assert_eq!(
            file_inode
                .create("foo", InodeType::File, FilePerm::from_bits_truncate(0o644))
                .unwrap_err()
                .error(),
            Errno::ENOTDIR
        );
        assert_eq!(
            file_inode.unlink("foo").unwrap_err().error(),
            Errno::ENOTDIR
        );

        let mut dir_ptrs = [0u32; 15];
        dir_ptrs[0] = 80;
        let dir_inode = make_live_dir_inode(ext2, 2, 0, 8, FileFlags::empty(), dir_ptrs);
        assert_eq!(
            dir_inode
                .create("", InodeType::File, FilePerm::from_bits_truncate(0o644))
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
        assert_eq!(dir_inode.unlink("").unwrap_err().error(), Errno::EINVAL);
    }

    #[ktest]
    fn dir_make_empty_ok() {
        clocks::init_for_ktest();
        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let block_size = f.ext2.block_size();

        let dir = root
            .create("empty", InodeType::Dir, FilePerm::from_bits_truncate(0o755))
            .unwrap();
        assert_eq!(inode_size(&dir), block_size);

        let mut visitor = CollectDirentVisitor::default();
        dir.readdir_at(0, &mut visitor).unwrap();

        assert_eq!(visitor.entries.len(), 2);
        assert_eq!(visitor.entries[0].0, ".");
        assert_eq!(visitor.entries[0].1, dir.ino() as u64);
        assert_eq!(visitor.entries[0].2, InodeType::Dir);
        assert_eq!(visitor.entries[1].0, "..");
        assert_eq!(visitor.entries[1].1, root.ino() as u64);
        assert_eq!(visitor.entries[1].2, InodeType::Dir);
    }

    struct RmdirTestEnv {
        f: testkit::Ext2Fixture,
        parent: Arc<Inode>,
        child: Arc<Inode>,
    }

    /// Sets up a parent directory (ROOT_INO) with a "sub" child directory.
    /// If `add_child_file` is true, creates a file "foo" inside "sub".
    fn prepare_rmdir_env(add_child_file: bool) -> RmdirTestEnv {
        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let parent = f.ext2.read_inode(ROOT_INO).unwrap();

        let child = parent
            .create("sub", InodeType::Dir, FilePerm::from_bits_truncate(0o755))
            .unwrap();

        if add_child_file {
            child
                .create("foo", InodeType::File, FilePerm::from_bits_truncate(0o644))
                .unwrap();
        }

        RmdirTestEnv { f, parent, child }
    }

    #[ktest]
    fn dir_rmdir_ok() {
        clocks::init_for_ktest();

        let RmdirTestEnv { f, parent, child } = prepare_rmdir_env(false);
        let child_ino = child.ino();

        parent.rmdir("sub").unwrap();
        assert_eq!(inode_nlinks(&parent), 2);
        assert_eq!(
            lookup_ino(&parent, "sub").unwrap_err().error(),
            Errno::ENOENT
        );
        assert_eq!(inode_size(&child), f.ext2.block_size());
        assert_eq!(inode_nlinks(&child), 0);
        assert_eq!(
            f.ext2.read_inode(child_ino).unwrap_err().error(),
            Errno::ESTALE
        );

        drop(child);
        f.ext2.sync_all().unwrap();
        assert_eq!(
            f.ext2.read_inode(child_ino).unwrap_err().error(),
            Errno::ENOENT
        );
    }

    #[ktest]
    fn dir_rmdir_notempty_err() {
        clocks::init_for_ktest();

        let env = prepare_rmdir_env(true);
        let parent = &env.parent;
        let child_ino = env.child.ino();

        let err = parent.rmdir("sub").unwrap_err();
        assert_eq!(err.error(), Errno::ENOTEMPTY);

        assert_eq!(lookup_ino(parent, "sub").unwrap(), child_ino);
        assert_eq!(inode_nlinks(parent), 3);
    }

    #[ktest]
    fn file_direct_write_truncate_ok() {
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
            InodeIo::write_at(file.as_ref(), 0, &mut payload_reader, StatusFlags::O_DIRECT)
                .unwrap(),
            payload.len()
        );
        let free_after_write = f.ext2.super_block().free_blocks_count();
        assert_eq!(free_before_write.saturating_sub(free_after_write), 1);

        assert_eq!(inode_size(&file), payload.len());
        let mut readback = vec![0u8; block_size];
        let mut readback_writer = VmWriter::from(readback.as_mut_slice()).to_fallible();
        assert_eq!(
            InodeIo::read_at(
                file.as_ref(),
                0,
                &mut readback_writer,
                StatusFlags::O_DIRECT
            )
            .unwrap(),
            payload.len()
        );
        assert_eq!(&readback[..payload.len()], payload.as_slice());

        let free_before_truncate = f.ext2.super_block().free_blocks_count();
        VfsInodeTrait::resize(file.as_ref(), 0).unwrap();
        let free_after_truncate = f.ext2.super_block().free_blocks_count();
        assert_eq!(free_after_truncate.saturating_sub(free_before_truncate), 1);

        assert_eq!(inode_size(&file), 0);
        assert_eq!(
            VfsInodeTrait::metadata(file.as_ref()).nr_sectors_allocated,
            0
        );

        let on_disk = f.ext2.read_inode_desc(file.ino()).unwrap();
        assert_eq!(on_disk.size, 0);
        assert_eq!(on_disk.sector_count, 0);
    }

    #[ktest]
    fn file_direct_write_sparse_hole_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let block_size = f.ext2.block_size();
        let file =
            make_live_file_inode(&f.ext2, 72, block_size * 3, 0, FileFlags::empty(), [0; 15]);
        let payload = vec![0x6bu8; block_size];

        let mut payload_reader = VmReader::from(payload.as_slice()).to_fallible();
        assert_eq!(
            InodeIo::write_at(
                file.as_ref(),
                block_size,
                &mut payload_reader,
                StatusFlags::O_DIRECT
            )
            .unwrap(),
            block_size
        );

        let mut out = vec![0u8; block_size];
        let mut out_writer = VmWriter::from(out.as_mut_slice()).to_fallible();
        assert_eq!(
            InodeIo::read_at(
                file.as_ref(),
                block_size,
                &mut out_writer,
                StatusFlags::O_DIRECT
            )
            .unwrap(),
            block_size
        );
        assert_eq!(out, payload);

        let mut untouched_hole = vec![0xa5u8; block_size];
        let mut hole_writer = VmWriter::from(untouched_hole.as_mut_slice()).to_fallible();
        assert_eq!(
            InodeIo::read_at(file.as_ref(), 0, &mut hole_writer, StatusFlags::O_DIRECT).unwrap(),
            block_size
        );
        assert!(untouched_hole.iter().all(|byte| *byte == 0));
    }

    #[ktest]
    fn file_direct_write_extend_eof_zero_tail_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let block_size = f.ext2.block_size();
        let sectors_per_block = (block_size / SECTOR_SIZE) as u32;
        let old_size = 100usize;
        let data_bid = 80u32;

        let mut on_disk_block = vec![0xaau8; block_size];
        on_disk_block[..old_size].fill(0x11);
        f.disk
            .segment()
            .write_bytes(Bid::new(data_bid as u64).to_offset(), &on_disk_block)
            .unwrap();

        let mut ptrs = [0u32; 15];
        ptrs[0] = data_bid;
        let file = make_live_file_inode(
            &f.ext2,
            73,
            old_size,
            sectors_per_block,
            FileFlags::empty(),
            ptrs,
        );
        let payload = vec![0x5cu8; block_size];

        let mut payload_reader = VmReader::from(payload.as_slice()).to_fallible();
        assert_eq!(
            InodeIo::write_at(
                file.as_ref(),
                block_size,
                &mut payload_reader,
                StatusFlags::O_DIRECT
            )
            .unwrap(),
            block_size
        );

        let mut first_block = vec![0u8; block_size];
        let mut first_writer = VmWriter::from(first_block.as_mut_slice()).to_fallible();
        assert_eq!(
            InodeIo::read_at(file.as_ref(), 0, &mut first_writer, StatusFlags::empty()).unwrap(),
            block_size
        );
        assert!(first_block[..old_size].iter().all(|byte| *byte == 0x11));
        assert!(first_block[old_size..].iter().all(|byte| *byte == 0));

        let mut second_block = vec![0u8; block_size];
        let mut second_writer = VmWriter::from(second_block.as_mut_slice()).to_fallible();
        assert_eq!(
            InodeIo::read_at(
                file.as_ref(),
                block_size,
                &mut second_writer,
                StatusFlags::O_DIRECT
            )
            .unwrap(),
            block_size
        );
        assert_eq!(second_block, payload);
    }

    #[ktest]
    fn symlink_fast_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let link = root
            .create(
                "fast_link",
                InodeType::SymLink,
                FilePerm::from_bits_truncate(0o777),
            )
            .unwrap();

        let target = "./phase08/fast-target";
        link.write_link(target).unwrap();
        assert_eq!(link.read_link().unwrap(), target);

        assert_eq!(inode_size(&link), target.len());
        assert_eq!(
            VfsInodeTrait::metadata(link.as_ref()).nr_sectors_allocated,
            0
        );
    }

    #[ktest]
    fn symlink_slow_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let link = root
            .create(
                "slow_link",
                InodeType::SymLink,
                FilePerm::from_bits_truncate(0o777),
            )
            .unwrap();

        let target = "x".repeat(MAX_FAST_SYMLINK_LEN);
        link.write_link(&target).unwrap();
        assert_eq!(link.read_link().unwrap(), target);

        assert_eq!(inode_size(&link), MAX_FAST_SYMLINK_LEN);
        assert!(VfsInodeTrait::metadata(link.as_ref()).nr_sectors_allocated > 0);
    }

    #[ktest]
    fn symlink_too_long_err() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let link = root
            .create(
                "long_link",
                InodeType::SymLink,
                FilePerm::from_bits_truncate(0o777),
            )
            .unwrap();

        let too_long = "y".repeat(f.ext2.block_size());
        let err = link.write_link(&too_long).unwrap_err();
        assert_eq!(err.error(), Errno::ENAMETOOLONG);
    }

    #[ktest]
    fn symlink_non_symlink_err() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "regular_file",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();

        assert_eq!(file.read_link().unwrap_err().error(), Errno::EINVAL);
        assert_eq!(
            file.write_link("target").unwrap_err().error(),
            Errno::EINVAL
        );
    }

    #[ktest]
    fn file_write_partial_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "partial",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        let block_size = f.ext2.block_size();
        let original = vec![0x11u8; block_size];
        let patch = vec![0x7cu8; 257];
        let patch_off = 123usize;

        let mut original_reader = VmReader::from(original.as_slice()).to_fallible();
        InodeIo::write_at(file.as_ref(), 0, &mut original_reader, StatusFlags::empty()).unwrap();
        let mut patch_reader = VmReader::from(patch.as_slice()).to_fallible();
        InodeIo::write_at(
            file.as_ref(),
            patch_off,
            &mut patch_reader,
            StatusFlags::empty(),
        )
        .unwrap();

        let mut out = vec![0u8; block_size];
        let mut out_writer = VmWriter::from(out.as_mut_slice()).to_fallible();
        assert_eq!(
            InodeIo::read_at(file.as_ref(), 0, &mut out_writer, StatusFlags::empty()).unwrap(),
            block_size
        );
        assert_eq!(&out[..patch_off], &original[..patch_off]);
        assert_eq!(&out[patch_off..patch_off + patch.len()], patch.as_slice());
        assert_eq!(
            &out[patch_off + patch.len()..],
            &original[patch_off + patch.len()..]
        );
    }

    #[ktest]
    fn file_write_cross_block_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "cross",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        let block_size = f.ext2.block_size();
        let crossing_off = block_size - 64;
        let crossing_data = (0..128)
            .map(|i| (i as u8).wrapping_add(1))
            .collect::<Vec<_>>();

        let zeros = vec![0u8; block_size * 2];
        let mut zeros_reader = VmReader::from(zeros.as_slice()).to_fallible();
        InodeIo::write_at(file.as_ref(), 0, &mut zeros_reader, StatusFlags::empty()).unwrap();
        let mut crossing_reader = VmReader::from(crossing_data.as_slice()).to_fallible();
        InodeIo::write_at(
            file.as_ref(),
            crossing_off,
            &mut crossing_reader,
            StatusFlags::empty(),
        )
        .unwrap();

        let mut out = vec![0u8; block_size * 2];
        let mut out_writer = VmWriter::from(out.as_mut_slice()).to_fallible();
        assert_eq!(
            InodeIo::read_at(file.as_ref(), 0, &mut out_writer, StatusFlags::empty()).unwrap(),
            block_size * 2
        );
        assert_eq!(
            &out[crossing_off..crossing_off + 128],
            crossing_data.as_slice()
        );
        assert_eq!(inode_size(&file), block_size * 2);
    }

    #[ktest]
    fn file_write_sparse_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "sparse",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        let block_size = f.ext2.block_size();
        let write_off = block_size * 2 + 128;
        let payload = vec![0x3au8; 256];

        let free_before = f.ext2.super_block().free_blocks_count();
        let mut payload_reader = VmReader::from(payload.as_slice()).to_fallible();
        InodeIo::write_at(
            file.as_ref(),
            write_off,
            &mut payload_reader,
            StatusFlags::empty(),
        )
        .unwrap();
        let free_after = f.ext2.super_block().free_blocks_count();

        assert_eq!(inode_size(&file), write_off + payload.len());
        assert_eq!(free_before.saturating_sub(free_after), 1);

        let mut out = vec![0u8; payload.len()];
        let mut out_writer = VmWriter::from(out.as_mut_slice()).to_fallible();
        assert_eq!(
            InodeIo::read_at(
                file.as_ref(),
                write_off,
                &mut out_writer,
                StatusFlags::empty()
            )
            .unwrap(),
            payload.len()
        );
        assert_eq!(out, payload);

        let mut hole = vec![0xa5u8; block_size];
        let mut hole_writer = VmWriter::from(hole.as_mut_slice()).to_fallible();
        assert_eq!(
            InodeIo::read_at(file.as_ref(), 0, &mut hole_writer, StatusFlags::empty()).unwrap(),
            block_size
        );
        assert!(hole.iter().all(|b| *b == 0));
    }

    #[ktest]
    fn file_write_enospc_rollback() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(2, 2)
            .with_free_inodes(1000, 1000)
            .with_group0_used_dirs(1)
            .build()
            .unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "enospc",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        let block_size = f.ext2.block_size();
        let base_data = vec![0x44u8; block_size];

        let mut base_reader = VmReader::from(base_data.as_slice()).to_fallible();
        InodeIo::write_at(file.as_ref(), 0, &mut base_reader, StatusFlags::O_DIRECT).unwrap();
        let free_before_fail = f.ext2.super_block().free_blocks_count();
        assert_eq!(free_before_fail, 1);

        let fail_payload = vec![0x66u8; block_size * 2];
        let mut fail_reader = VmReader::from(fail_payload.as_slice()).to_fallible();
        let err = InodeIo::write_at(
            file.as_ref(),
            block_size,
            &mut fail_reader,
            StatusFlags::O_DIRECT,
        )
        .unwrap_err();
        assert_eq!(err.error(), Errno::ENOSPC);

        assert_eq!(inode_size(&file), block_size);
        assert_eq!(f.ext2.super_block().free_blocks_count(), free_before_fail);

        let mut readback = vec![0u8; block_size];
        let mut readback_writer = VmWriter::from(readback.as_mut_slice()).to_fallible();
        assert_eq!(
            InodeIo::read_at(
                file.as_ref(),
                0,
                &mut readback_writer,
                StatusFlags::O_DIRECT
            )
            .unwrap(),
            block_size
        );
        assert_eq!(readback, base_data);
    }

    #[ktest]
    fn file_write_dir_eisdir() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let mut reader = VmReader::from(b"x".as_slice()).to_fallible();
        let err = root.write_at(0, &mut reader).unwrap_err();
        assert_eq!(err.error(), Errno::EISDIR);
    }

    #[ktest]
    fn file_read_sparse_ok() {
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
    fn file_read_direct_sparse_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let block_size = f.ext2.block_size();
        let file =
            make_live_file_inode(&f.ext2, 74, block_size * 2, 0, FileFlags::empty(), [0; 15]);

        let mut buf = vec![0xa5u8; block_size];
        let mut writer = VmWriter::from(buf.as_mut_slice()).to_fallible();
        assert_eq!(
            InodeIo::read_at(file.as_ref(), 0, &mut writer, StatusFlags::O_DIRECT).unwrap(),
            block_size
        );
        assert!(buf.iter().all(|byte| *byte == 0));
    }

    #[ktest]
    fn file_read_dir_eisdir() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let mut buf = [0u8; 1];
        let mut writer = VmWriter::from(buf.as_mut_slice()).to_fallible();
        let err = root.read_at(0, &mut writer).unwrap_err();
        assert_eq!(err.error(), Errno::EISDIR);
    }

    #[ktest]
    fn file_read_eio() {
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
    fn file_resize_extend_sparse_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "resize_sparse",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        let block_size = f.ext2.block_size();
        let target = block_size * 3 + 123;

        let free_before = f.ext2.super_block().free_blocks_count();
        VfsInodeTrait::resize(file.as_ref(), target).unwrap();
        let free_after = f.ext2.super_block().free_blocks_count();

        assert_eq!(inode_size(&file), target);
        assert_eq!(free_before, free_after);

        let mut buf = vec![0xa5u8; block_size];
        let mut writer = VmWriter::from(buf.as_mut_slice()).to_fallible();
        assert_eq!(
            InodeIo::read_at(file.as_ref(), 0, &mut writer, StatusFlags::empty()).unwrap(),
            block_size
        );
        assert!(buf.iter().all(|b| *b == 0));
    }

    #[ktest]
    fn file_resize_then_write_read_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "resize_then_write",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        let block_size = f.ext2.block_size();
        let target_size = block_size * 2 + 64;
        VfsInodeTrait::resize(file.as_ref(), target_size).unwrap();

        let write_off = block_size + 16;
        let payload = (0..128u16).map(|v| (v as u8) ^ 0x5a).collect::<Vec<_>>();
        let mut payload_reader = VmReader::from(payload.as_slice()).to_fallible();
        assert_eq!(
            file.write_at(write_off, &mut payload_reader).unwrap(),
            payload.len()
        );

        let mut out = vec![0u8; payload.len()];
        let mut out_writer = VmWriter::from(out.as_mut_slice()).to_fallible();
        assert_eq!(
            file.read_at(write_off, &mut out_writer).unwrap(),
            payload.len()
        );
        assert_eq!(out, payload);
        assert_eq!(inode_size(&file), target_size);
    }

    #[ktest]
    fn file_resize_updates_backend_npages() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "resize_npages",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        let block_size = f.ext2.block_size();

        assert_eq!(file.inner.read().backend().npages(), 0);

        VfsInodeTrait::resize(file.as_ref(), block_size + 1).unwrap();
        let expected_npages = (block_size + 1).align_up(BLOCK_SIZE) / BLOCK_SIZE;
        assert_eq!(file.inner.read().backend().npages(), expected_npages);

        VfsInodeTrait::resize(file.as_ref(), 0).unwrap();
        assert_eq!(file.inner.read().backend().npages(), 0);
    }

    #[ktest]
    fn file_sparse_shrink_discards_dirty_truncated_pages() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "sparse_shrink",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        let block_size = f.ext2.block_size();
        let sparse_size = block_size * 3;

        VfsInodeTrait::resize(file.as_ref(), sparse_size).unwrap();
        let vmo = VfsInodeTrait::page_cache(file.as_ref()).unwrap();
        let dirty = [0x5au8; 32];
        vmo.write_bytes(block_size * 2, &dirty).unwrap();

        VfsInodeTrait::resize(file.as_ref(), block_size).unwrap();
        assert_eq!(inode_size(&file), block_size);
        assert_eq!(vmo.size(), block_size.align_up(BLOCK_SIZE));
    }

    #[ktest]
    fn falloc_alloc_extends_size() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 67, 0, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();
        let new_size = block_size + 210;
        let free_before = f.ext2.super_block().free_blocks_count();

        file.fallocate(FallocMode::Allocate, block_size + 10, 200)
            .unwrap();
        assert_eq!(file.file_size(), new_size);

        let free_after = f.ext2.super_block().free_blocks_count();
        assert_eq!(free_before.saturating_sub(free_after), 1);

        let mut out = vec![0x5au8; 210];
        let mut out_writer = VmWriter::from(out.as_mut_slice()).to_fallible();
        assert_eq!(file.read_at(block_size, &mut out_writer).unwrap(), 210);
        assert!(out.iter().all(|byte| *byte == 0));
    }

    // #[ktest]
    // fn falloc_keep_size_allocates_blocks_without_changing_size() {
    //     clocks::init_for_ktest();

    //     let f = Ext2FixtureBuilder::new(1, 256)
    //         .with_free_blocks(64, 64)
    //         .build()
    //         .unwrap();
    //     let file = make_live_file_inode(&f.ext2, 68, 123, 0, FileFlags::empty(), [0; 15]);
    //     let block_size = f.ext2.block_size();
    //     let free_before = f.ext2.super_block().free_blocks_count();

    //     file.fallocate(FallocMode::AllocateKeepSize, block_size, 512)
    //         .unwrap();
    //     assert_eq!(file.file_size(), 123);

    //     let free_after = f.ext2.super_block().free_blocks_count();
    //     assert_eq!(free_before.saturating_sub(free_after), 1);

    //     file.resize(block_size + 512).unwrap();

    //     let mut out = vec![0x5au8; 512];
    //     let mut out_writer = VmWriter::from(out.as_mut_slice()).to_fallible();
    //     assert_eq!(file.read_at(block_size, &mut out_writer).unwrap(), 512);
    //     assert!(out.iter().all(|byte| *byte == 0));
    // }

    #[ktest]
    fn falloc_allocate_returns_enospc_after_consuming_blocks() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(2, 2)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 69, 0, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();

        file.fallocate(FallocMode::Allocate, 0, block_size * 2)
            .unwrap();
        assert_eq!(f.ext2.super_block().free_blocks_count(), 0);
        assert_eq!(file.file_size(), block_size * 2);

        let err = file
            .fallocate(FallocMode::Allocate, block_size * 2, block_size)
            .unwrap_err();
        assert_eq!(err.error(), Errno::ENOSPC);
        assert_eq!(f.ext2.super_block().free_blocks_count(), 0);
        assert_eq!(file.file_size(), block_size * 2);
    }

    // #[ktest]
    // fn falloc_punch_hole_zeroes() {
    //     clocks::init_for_ktest();

    //     let f = Ext2FixtureBuilder::new(1, 256)
    //         .with_free_blocks(64, 64)
    //         .build()
    //         .unwrap();
    //     let file = make_live_file_inode(&f.ext2, 70, 0, 0, FileFlags::empty(), [0; 15]);
    //     let block_size = f.ext2.block_size();

    //     let payload = vec![0xabu8; block_size];
    //     let mut payload_reader = VmReader::from(payload.as_slice()).to_fallible();
    //     file.write_at(0, &mut payload_reader).unwrap();

    //     let punch_off = 128usize;
    //     let punch_len = 512usize;
    //     file.fallocate(FallocMode::PunchHoleKeepSize, punch_off, punch_len)
    //         .unwrap();

    //     let mut out = vec![0u8; block_size];
    //     let mut out_writer = VmWriter::from(out.as_mut_slice()).to_fallible();
    //     assert_eq!(file.read_at(0, &mut out_writer).unwrap(), block_size);

    //     assert_eq!(&out[..punch_off], &payload[..punch_off]);
    //     assert!(
    //         out[punch_off..punch_off + punch_len]
    //             .iter()
    //             .all(|byte| *byte == 0)
    //     );
    //     assert_eq!(
    //         &out[punch_off + punch_len..],
    //         &payload[punch_off + punch_len..]
    //     );
    //     assert_eq!(file.file_size(), block_size);
    // }

    #[ktest]
    fn falloc_unsupported_mode() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 71, 0, 0, FileFlags::empty(), [0; 15]);

        for mode in [
            FallocMode::PunchHoleKeepSize,
            FallocMode::AllocateKeepSize,
            FallocMode::ZeroRange,
            FallocMode::ZeroRangeKeepSize,
            FallocMode::CollapseRange,
            FallocMode::InsertRange,
            FallocMode::AllocateUnshareRange,
        ] {
            let err = file.fallocate(mode, 0, 1).unwrap_err();
            assert_eq!(err.error(), Errno::EOPNOTSUPP);
        }
    }

    #[ktest]
    fn file_resize_shrink_zero_tail_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "shrink_tail",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        let block_size = f.ext2.block_size();
        let keep_in_tail = 200usize;

        let payload = vec![0xabu8; block_size * 2];
        let mut payload_reader = VmReader::from(payload.as_slice()).to_fallible();
        InodeIo::write_at(file.as_ref(), 0, &mut payload_reader, StatusFlags::O_DIRECT).unwrap();

        let free_before_resize = f.ext2.super_block().free_blocks_count();
        VfsInodeTrait::resize(file.as_ref(), block_size + keep_in_tail).unwrap();
        let free_after_resize = f.ext2.super_block().free_blocks_count();

        let mut kept = vec![0u8; keep_in_tail];
        let mut kept_writer = VmWriter::from(kept.as_mut_slice()).to_fallible();
        assert_eq!(
            InodeIo::read_at(
                file.as_ref(),
                block_size,
                &mut kept_writer,
                StatusFlags::empty()
            )
            .unwrap(),
            keep_in_tail
        );
        assert!(kept.iter().all(|b| *b == 0xab));

        let mut eof = [0x5au8; 32];
        let mut eof_writer = VmWriter::from(eof.as_mut_slice()).to_fallible();
        assert_eq!(
            InodeIo::read_at(
                file.as_ref(),
                block_size + keep_in_tail,
                &mut eof_writer,
                StatusFlags::empty()
            )
            .unwrap(),
            0
        );
        assert_eq!(eof, [0x5au8; 32]);

        assert_eq!(inode_size(&file), block_size + keep_in_tail);
        assert_eq!(free_before_resize, free_after_resize);
    }

    #[ktest]
    fn page_cache_vmo_size_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "pcache",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        let block_size = f.ext2.block_size();

        let vmo = VfsInodeTrait::page_cache(file.as_ref()).unwrap();
        assert_eq!(vmo.size(), 0);

        VfsInodeTrait::resize(file.as_ref(), block_size + 1).unwrap();
        assert_eq!(inode_size(&file), block_size + 1);
        assert_eq!(vmo.size(), (block_size + 1).align_up(BLOCK_SIZE));

        VfsInodeTrait::resize(file.as_ref(), 0).unwrap();
        assert_eq!(inode_size(&file), 0);
        assert_eq!(vmo.size(), 0);
    }

    #[ktest]
    fn xattr_roundtrip_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "xattr-file",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();

        let make_name = || XattrName::try_from_full_name("user.test").unwrap();
        let value = b"hello-xattr";
        let mut set_reader = VmReader::from(value.as_slice()).to_fallible();
        VfsInodeTrait::set_xattr(
            file.as_ref(),
            make_name(),
            &mut set_reader,
            XattrSetFlags::CREATE_OR_REPLACE,
        )
        .unwrap();

        let mut empty_writer = VmWriter::from(&mut [][..]).to_fallible();
        let queried =
            VfsInodeTrait::get_xattr(file.as_ref(), make_name(), &mut empty_writer).unwrap();
        assert_eq!(queried, value.len());

        let mut got = vec![0u8; value.len()];
        let mut get_writer = VmWriter::from(got.as_mut_slice()).to_fallible();
        let read = VfsInodeTrait::get_xattr(file.as_ref(), make_name(), &mut get_writer).unwrap();
        assert_eq!(read, value.len());
        assert_eq!(got.as_slice(), value);
    }

    #[ktest]
    fn xattr_remove_last_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "xattr-remove",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();

        let make_name = || XattrName::try_from_full_name("user.key").unwrap();
        let mut set_reader = VmReader::from(b"value".as_slice()).to_fallible();
        VfsInodeTrait::set_xattr(
            file.as_ref(),
            make_name(),
            &mut set_reader,
            XattrSetFlags::CREATE_OR_REPLACE,
        )
        .unwrap();

        VfsInodeTrait::remove_xattr(file.as_ref(), make_name()).unwrap();

        let err = VfsInodeTrait::remove_xattr(file.as_ref(), make_name()).unwrap_err();
        assert_eq!(err.error(), Errno::ENODATA);
    }

    #[ktest]
    fn xattr_list_namespace_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let file = root
            .create(
                "xattr-list",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();

        let user_name = XattrName::try_from_full_name("user.alpha").unwrap();
        let trusted_name = XattrName::try_from_full_name("trusted.beta").unwrap();

        let mut user_reader = VmReader::from(b"u".as_slice()).to_fallible();
        VfsInodeTrait::set_xattr(
            file.as_ref(),
            user_name,
            &mut user_reader,
            XattrSetFlags::CREATE_OR_REPLACE,
        )
        .unwrap();

        let mut trusted_reader = VmReader::from(b"t".as_slice()).to_fallible();
        VfsInodeTrait::set_xattr(
            file.as_ref(),
            trusted_name,
            &mut trusted_reader,
            XattrSetFlags::CREATE_OR_REPLACE,
        )
        .unwrap();

        let mut buf = vec![0u8; 128];
        let mut writer = VmWriter::from(buf.as_mut_slice()).to_fallible();
        let len =
            VfsInodeTrait::list_xattr(file.as_ref(), XattrNamespace::User, &mut writer).unwrap();
        let listed = &buf[..len];
        let names: Vec<&[u8]> = listed
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
            .collect();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0], b"user.alpha");
    }
}
