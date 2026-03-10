// SPDX-License-Identifier: MPL-2.0

use core::{
    mem::size_of,
    sync::atomic::{AtomicUsize, Ordering},
};

use device_id::{decode_device_numbers, encode_device_numbers};
use ostd::{const_assert, mm::io_util::HasVmReaderWriter};

use super::{
    block_ptr::{InodeMapping, InodeMappingDesc},
    fs::{Ext2, ROOT_INO},
    prelude::*,
    utils::now,
};
use crate::{
    fs::{
        ext2::{
            dir::{DirEntry, DirEntryIter},
            xattr::Xattr,
        },
        utils::{
            Extension, FallocMode, InodeMode, Metadata, XattrName, XattrNamespace, XattrSetFlags,
        },
    },
    process::{Gid, Uid},
};

/// Maximum bytes storable in ext2 inode i_block area for fast symlink payload.
///
/// Linux: /root/linux/fs/ext2/namei.c:177 (`sizeof(EXT2_I(inode)->i_data)`).
const MAX_FAST_SYMLINK_LEN: usize = size_of::<u32>() * 15;
const MAX_LINK_COUNT: u16 = 32000;

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
    xattr: Option<RwMutex<Xattr>>,
    extension: Extension,
}

struct RenameContext<'a> {
    fs: &'a Arc<Ext2>,
    source_dir: &'a Inode,
    target_dir: &'a Inode,
    old_name: &'a str,
    new_name: &'a str,
    old_ino: u32,
    old_inode: Arc<Inode>,
    existing_ino: Option<u32>,
    existing_inode: Option<Arc<Inode>>,
    old_is_dir: bool,
    moved_ft: u8,
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
        // Use `new_cyclic` so `InodeInner` can keep a weak self pointer for
        // inode-internal workflows that need to upgrade to `Arc<Inode>`.

        Arc::new_cyclic(|weak_self: &Weak<Self>| Self {
            ino,
            type_,
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

    pub fn is_dirty(&self) -> bool {
        self.inner.read().is_dirty()
    }

    pub(super) fn ino(&self) -> u32 {
        self.ino
    }

    pub(super) fn block_group_idx(&self) -> usize {
        self.block_group_idx
    }

    pub(super) fn fs_arc(&self) -> Result<Arc<Ext2>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))
    }

    fn has_invalid_child_name(name: &str) -> bool {
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
    /// Linux: /root/linux/fs/ext2/inode.c:1493-1500 (ext2_iget)
    pub(super) fn device_id(&self) -> u64 {
        // SPEC: non-device inodes report rdev = 0.
        if self.type_ != InodeType::CharDevice && self.type_ != InodeType::BlockDevice {
            return 0;
        }

        // SPEC: i_block payload lives in mapping domain.
        let inner = self.inner.read();
        let mapping_backend = Arc::clone(inner.backend());
        let mapping = mapping_backend.mapping.read();
        mapping.desc.decode_device_id()
    }

    /// Sets the encoded device ID for special files and persists it.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1589-1599 (__ext2_write_inode)
    pub(super) fn set_device_id(&self, device_id: u64) -> Result<()> {
        if self.type_ != InodeType::CharDevice && self.type_ != InodeType::BlockDevice {
            // SPEC: fail with EINVAL for non-device inodes; no lock/state mutation needed.
            return_errno!(Errno::EINVAL);
        }
        // Lock order: inner -> mapping.
        let mut inner = self.inner.write();
        inner.encode_device_id(device_id)?;
        // DIFF from Linux: Linux caches dev_t in i_rdev and encodes during write_inode;
        // Asterinas stores the Linux-compatible on-disk encoding directly in block_ptrs.
        inner.set_ctime(now());
        Ok(())
    }

    pub(super) fn resize(&self, new_size: usize) -> Result<()> {
        let fs = self.fs_arc()?;
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }
        if self.type_ != InodeType::File
            && self.type_ != InodeType::Dir
            && self.type_ != InodeType::SymLink
        {
            return_errno!(Errno::EINVAL);
        }

        let old_size = {
            let inner = self.inner.read();
            // Linux: /root/linux/fs/ext2/inode.c:48-55 (ext2_inode_is_fast_symlink).
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

            inner.file_size()
        };

        if new_size == old_size {
            return Ok(());
        }

        if new_size < old_size && new_size % block_size != 0 {
            let zero_to = new_size.align_up(block_size);
            self.inner
                .read()
                .page_cache()
                .fill_zeros(new_size..zero_to)?;
        }

        let mut inner = self.inner.write();
        if new_size < old_size {
            inner.shrink(new_size)
        } else {
            inner.expand(new_size)
        }
    }

    pub(super) fn metadata(&self) -> Metadata {
        // Lock order: inner -> mapping.
        let inner = self.inner.read();
        let mapping_backend = Arc::clone(inner.backend());
        let mapping = mapping_backend.mapping.read();

        // TODO: should we panic here?
        let (dev, blk_size) = match self.fs.upgrade() {
            Some(fs) => (fs.block_device().id().as_encoded_u64(), fs.block_size()),
            None => (0, BLOCK_SIZE),
        };
        let rdev = if self.type_ == InodeType::CharDevice || self.type_ == InodeType::BlockDevice {
            // SPEC: for device inodes, decode rdev from i_block old/new format.
            mapping.desc.decode_device_id()
        } else {
            0
        };
        Metadata {
            dev,
            ino: self.ino as u64,
            size: inner.file_size(),
            blk_size,
            blocks: mapping.desc.blocks as usize,
            atime: inner.atime(),
            mtime: inner.mtime(),
            ctime: inner.ctime(),
            type_: self.type_,
            mode: inner.mode(),
            nlinks: inner.links_count() as usize,
            uid: Uid::new(inner.uid()),
            gid: Gid::new(inner.gid()),
            rdev,
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
    /// Linux: /root/linux/fs/ext2/xattr.c:195-275 (ext2_xattr_get)
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

    /// Lists extended-attribute names in one namespace and writes them to `list_writer`.
    ///
    /// Linux: /root/linux/fs/ext2/xattr.c:287-364 (ext2_xattr_list)
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
    /// Linux: /root/linux/fs/ext2/xattr.c:405-651 (ext2_xattr_set)
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
    /// Linux: /root/linux/fs/ext2/xattr.c:405-651 (ext2_xattr_set with value == NULL)
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

    /// Reads symbolic-link target bytes and decodes them as UTF-8.
    ///
    /// Linux fast path: /root/linux/fs/ext2/inode.c:1483-1487
    /// Linux slow path: /root/linux/fs/namei.c:6227-6234 (page_get_link)
    pub(super) fn read_link(&self) -> Result<String> {
        if self.type_ != InodeType::SymLink {
            return_errno!(Errno::EINVAL);
        }

        let fs = self.fs_arc()?;
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        let inner = self.inner.read();
        inner.read_link()
    }

    /// Writes symbolic-link target bytes into either fast-inline or slow-pagecache storage.
    ///
    /// Linux length gate and fast/slow split: /root/linux/fs/ext2/namei.c:165-191
    /// Linux slow write primitive: /root/linux/fs/namei.c:6273-6302 (page_symlink)
    pub(super) fn write_link(&self, target: &str) -> Result<()> {
        if self.type_ != InodeType::SymLink {
            return_errno!(Errno::EINVAL);
        }

        let fs = self.fs_arc()?;
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        let target_len = target.len();
        let with_nul = target_len.checked_add(1).ok_or_else(|| {
            Error::with_message(Errno::ENAMETOOLONG, "symlink target length overflow")
        })?;

        // Linux: /root/linux/fs/ext2/namei.c:165-166 (`strlen(symname)+1 > sb->s_blocksize`).
        if with_nul > block_size {
            return_errno!(Errno::ENAMETOOLONG);
        }
        let mut inner = self.inner.write();
        inner.write_link(target)
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
        inner.page_cache().pages().read(offset, writer)?;
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

        let fs = self.fs_arc()?;
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        let end = offset
            .checked_add(write_len)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "write range overflow"))?;

        let mut inner = self.inner.write();
        let old_size = inner.file_size();
        if let Err(err) = inner.prepare_continuous_blocks(&fs, offset, end, block_size, false) {
            inner.write_failed_cleanup(&fs, old_size, end, block_size);
            return Err(err);
        }

        if let Err(err) = inner.page_cache().pages().write(offset, reader) {
            inner.write_failed_cleanup(&fs, old_size, end, block_size);
            return Err(err.into());
        }

        let current = now();
        inner.touch_mtime_ctime(current);
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

        inner.page_cache().evict_range(offset..end)?;
        inner.read_direct_at(&fs, offset, end, writer)?;
        inner.upgrade().set_atime(now());
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
        let mut inner = self.inner.write();
        let old_size = inner.file_size();

        if inner.should_fallback_direct_write(&fs, offset, end, block_size)? {
            if let Err(err) = inner.prepare_continuous_blocks(&fs, offset, end, block_size, false) {
                inner.write_failed_cleanup(&fs, old_size, end, block_size);
                return Err(err);
            }

            if let Err(err) = inner.page_cache().pages().write(offset, reader) {
                inner.write_failed_cleanup(&fs, old_size, end, block_size);
                return Err(err.into());
            }

            let current = now();
            inner.touch_mtime_ctime(current);
            drop(inner);

            self.sync_data()?;
            return Ok(write_len);
        }

        inner.zero_direct_write_eof_tail(&fs, old_size, offset, block_size)?;

        if let Err(err) = inner.prepare_continuous_blocks(&fs, offset, end, block_size, true) {
            inner.write_failed_cleanup(&fs, old_size, end, block_size);
            return Err(err);
        }

        if let Err(err) = inner.write_direct_at(&fs, offset, reader) {
            inner.write_failed_cleanup(&fs, old_size, end, block_size);
            return Err(err);
        }

        let current = now();
        inner.touch_mtime_ctime(current);
        Ok(write_len)
    }

    pub(super) fn lookup(&self, name: &str) -> Result<Arc<Inode>> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let fs = self.fs_arc()?;
        let inner = self.inner.read();
        let ino = inner.find_entry(&fs, name)?;
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

        let fs = self.fs_arc()?;
        let inner = self.inner.read();
        inner.readdir_at(&fs, offset, visitor)
    }

    /// Initializes a directory with `.` and `..`.
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

        let mut inner = self.inner.write();
        inner.make_empty(self.ino, parent_ino, &fs)?;
        Ok(())
    }

    pub(super) fn empty_dir(&self) -> bool {
        let Ok(fs) = self.fs_arc() else {
            return false;
        };
        let inner = self.inner.read();
        inner.empty_dir(&fs, self.ino)
    }

    pub(super) fn rmdir(&self, name: &str) -> Result<()> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        if Self::has_invalid_child_name(name) {
            return_errno!(Errno::EINVAL);
        }

        let fs = self.fs_arc()?;
        let parent_inner = self.inner.read();
        let child_ino = parent_inner.find_entry(&fs, name)?;
        drop(parent_inner);
        let child = fs.read_inode(child_ino)?;

        {
            let mut child_inner = child.inner.write();
            if child_inner.inode_type() != InodeType::Dir {
                return_errno!(Errno::ENOTDIR);
            }
            if !child_inner.empty_dir(&fs, child.ino()) {
                return_errno!(Errno::ENOTEMPTY);
            }

            child_inner.set_file_size(0);
            child_inner.sub_links_count_saturating(2);
            child_inner.set_dtime(now());
        }

        let mut parent_inner = self.inner.write();
        parent_inner.delete_entry(name)?;
        parent_inner.sub_links_count_saturating(1);
        // SPEC: parent link-count change in rmdir is a directory mutation; refresh
        // ctime/mtime the same way as add/delete entry paths.
        // Linux: /root/linux/fs/ext2/namei.c:312 (inode_dec_link_count(dir)).
        parent_inner.update_dir_timestamps_and_flags()?;
        Ok(())
    }

    /// Creates a subdirectory under this directory.
    ///
    /// Linux: /root/linux/fs/ext2/namei.c:228 (ext2_mkdir)
    pub(super) fn mkdir(&self, name: &str, perm: FilePerm) -> Result<Arc<Inode>> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        if Self::has_invalid_child_name(name) {
            return_errno!(Errno::EINVAL);
        }

        let fs = self.fs_arc()?;
        let mut parent_inner = self.inner.write();
        let slot = match parent_inner.scan_dir_for_slot(&fs, name)? {
            DirScanResult::Slot(slot) => slot,
            DirScanResult::NeedGrowth => parent_inner.grow_dir_block(&fs)?,
        };

        parent_inner.add_links_count_saturating(1);

        let child = match fs.create_inode(self.ino, InodeType::Dir, perm) {
            Ok(child) => child,
            Err(err) => {
                parent_inner.sub_links_count_saturating(1);
                return Err(err);
            }
        };
        let child_ino = child.ino();

        if let Err(err) = child.make_empty(self.ino) {
            let _ = fs.free_inode(child_ino, true);
            parent_inner.sub_links_count_saturating(1);
            return Err(err);
        }

        if let Err(err) =
            parent_inner.write_dir_entry(&fs, &slot, name, child_ino, DirEntryFileType::Dir as u8)
        {
            {
                let mut child_inner = child.inner.write();
                let _ = child_inner.release_dir_data_blocks_for_cleanup(&fs);
            }
            let _ = fs.free_inode(child_ino, true);
            parent_inner.sub_links_count_saturating(1);
            return Err(err);
        }

        parent_inner.update_dir_timestamps_and_flags()?;
        fs.insert_inode_cache(child.clone());
        Ok(child)
    }

    /// Implements fallocate operations for ext2.
    ///
    /// Linux ext2 has no native fallocate; this provides compatibility
    /// matching the old Asterinas ext2 implementation.
    ///
    /// Linux: /root/linux/fs/ext2/file.c:313-328 (`ext2_file_operations`, no `.fallocate`).
    /// Compat reference: /root/asterinas/kernel/src/fs/ext2_old/inode.rs:804-836.
    pub(super) fn fallocate(&self, mode: FallocMode, offset: usize, len: usize) -> Result<()> {
        match mode {
            FallocMode::PunchHoleKeepSize => {
                let inner = self.inner.read();
                let file_size = inner.file_size();
                if offset >= file_size {
                    return Ok(());
                }
                let end = offset
                    .checked_add(len)
                    .ok_or_else(|| Error::with_message(Errno::EINVAL, "fallocate range overflow"))?
                    .min(file_size);
                inner.page_cache().fill_zeros(offset..end)
            }
            FallocMode::Allocate | FallocMode::AllocateKeepSize => {
                if len == 0 {
                    return Ok(());
                }

                let fs = self.fs_arc()?;
                let block_size = fs.block_size();
                if block_size == 0 {
                    return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
                }

                let end = offset.checked_add(len).ok_or_else(|| {
                    Error::with_message(Errno::EINVAL, "fallocate range overflow")
                })?;
                let mut inner = self.inner.write();
                let old_size = inner.file_size();

                let new_blocks = match inner.allocate_range_blocks(&fs, offset, end, block_size) {
                    Ok(new_blocks) => new_blocks,
                    Err(err) => {
                        inner.write_failed_cleanup(&fs, old_size, end, block_size);
                        return Err(err);
                    }
                };

                if let Err(err) = inner.zero_new_blocks(&fs, &new_blocks, block_size) {
                    inner.write_failed_cleanup(&fs, old_size, end, block_size);
                    return Err(err);
                }

                if mode == FallocMode::Allocate && end > old_size {
                    if let Err(err) = inner.expand(end) {
                        inner.write_failed_cleanup(&fs, old_size, end, block_size);
                        return Err(err);
                    }
                }

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
        // SPEC: fsync step 1 flushes dirty data pages before metadata writeback.
        // Linux: /root/linux/fs/buffer.c:646 (generic_buffers_fsync)
        // -> /root/linux/mm/filemap.c:777 (file_write_and_wait_range).
        let mapping_backend = {
            let inner = self.inner.read();
            Arc::clone(inner.backend())
        };
        {
            let inner = self.inner.read();
            inner.sync_data_pages()?;
        }

        // SPEC: fsync step 2 flushes inode-local indirect metadata before
        // persisting inode-table state.
        mapping_backend.mapping.write().sync_indirect_blocks()?;

        // SPEC: fsync step 3 persists inode metadata. The caller is
        // responsible for the final device-cache flush.
        // Linux: /root/linux/fs/buffer.c:619 (sync_inode_metadata).
        self.sync_metadata(sync_inode_table)?;

        Ok(())
    }

    /// Persists inode metadata without flushing the device write cache.
    ///
    /// Linux: /root/linux/fs/buffer.c:619 (sync_inode_metadata)
    pub(super) fn sync_metadata(&self, sync_inode_table: bool) -> Result<()> {
        let fs = self.fs_arc()?;
        let mut inner = self.inner.write();
        inner.persist(self.ino, self.type_, &fs)?;

        if sync_inode_table {
            let block_group = fs.block_group(self.block_group_idx);
            block_group.sync_inode_table()?;
        }
        Ok(())
    }

    /// Prepares this inode for eviction.
    ///
    /// Returns `Ok(true)` if inode had `nlink == 0` and was truncated/persisted;
    /// returns `Ok(false)` if inode is still linked and only regular sync is needed.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:72 (ext2_evict_inode)
    pub(super) fn prepare_for_evict(&self) -> Result<bool> {
        if self.inner.read().desc.links_count > 0 {
            self.sync_all(false)?;
            return Ok(false);
        }

        if let Some(xattr) = self.xattr.as_ref() {
            xattr.write().delete_xattr_block()?;
        }

        let fs = self.fs_arc()?;
        let mut inner = self.inner.write();
        // inner.page_cache.discard_range(0..inner.file_size());
        inner.resize_page_cache_and_update_npages(0)?;
        let mapping_backend = Arc::clone(inner.backend());
        inner.set_dtime(now());
        inner.set_file_size(0);
        inner.set_file_acl(0);
        if inner.desc.blocks > 0 {
            let mut mapping = mapping_backend.mapping.write();
            mapping.truncate_blocks(&fs, 0)?;
            inner.sync_desc_mapping_from_snapshot(*mapping.get_desc());
        }
        inner.persist(self.ino, self.type_, &fs)?;

        Ok(true)
    }

    pub(super) fn sync_data(&self) -> Result<()> {
        let fs = self.fs_arc()?;
        let mapping_backend = {
            let inner = self.inner.read();
            Arc::clone(inner.backend())
        };

        // SPEC: fdatasync writes back dirty data pages first. The caller is
        // responsible for the final device-cache flush.
        // Linux: /root/linux/fs/buffer.c:609 (file_write_and_wait_range).
        {
            let inner = self.inner.read();
            inner.sync_data_pages()?;
        }

        // fdatasync must also persist dirty indirect metadata needed to reach
        // newly written data blocks before the final device flush.
        mapping_backend.mapping.write().sync_indirect_blocks()?;

        // SPEC: Linux writes metadata when I_DIRTY_DATASYNC is set.
        // Linux: /root/linux/fs/buffer.c:616-619.
        // Asterinas uses desc.is_dirty() as a conservative approximation
        // so fdatasync never misses i_size/block-mapping persistence.
        let mut inner = self.inner.write();
        if inner.is_dirty() {
            inner.persist(self.ino, self.type_, &fs)?;
        }
        Ok(())
    }

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

        if Self::has_invalid_child_name(name) {
            return_errno!(Errno::EINVAL);
        }

        // Linux: /root/linux/fs/ext2/namei.c:136-155 (ext2_mknod)
        // Accept ext2 special inode kinds needed by mknod in addition to regular kinds.
        if type_ != InodeType::File
            && type_ != InodeType::Dir
            && type_ != InodeType::SymLink
            && type_ != InodeType::CharDevice
            && type_ != InodeType::BlockDevice
            && type_ != InodeType::NamedPipe
        {
            return_errno!(Errno::EINVAL);
        }

        if type_ == InodeType::Dir {
            return self.mkdir(name, perm);
        } else {
            // Linux: ext2_create → ext2_new_inode + ext2_add_nondir
            let fs = self.fs_arc()?;
            let child = fs.create_inode(self.ino, type_, perm)?;
            let child_ino = child.ino();
            let dir_ft = Self::inode_type_to_dir_file_type(type_);
            let mut inner = self.inner.write();
            if let Err(err) = inner.add_entry(name, child_ino, dir_ft) {
                // SPEC: rollback — ext2_add_nondir failure path:
                // decrement link count and discard inode.
                let _ = fs.free_inode(child_ino, false);
                return Err(err);
            }

            fs.insert_inode_cache(child.clone());

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

        if Self::has_invalid_child_name(name) {
            return_errno!(Errno::EINVAL);
        }

        // SPEC: cross-filesystem rename check.
        let fs = self.fs_arc()?;
        let old_fs = old.fs_arc()?;
        if !Arc::ptr_eq(&fs, &old_fs) {
            return_errno!(Errno::EINVAL);
        }

        let dir_ft = Self::inode_type_to_dir_file_type(old.type_);
        let (mut dir_inner, mut old_inner) = write_lock_two_inodes(self, old);

        // Linux: inode_set_ctime_current + inode_inc_link_count before add_link.
        if old_inner.links_count() >= MAX_LINK_COUNT {
            return_errno!(Errno::EOVERFLOW);
        }
        old_inner.set_ctime(now());
        old_inner.add_links_count_saturating(1);

        let add_result = (|| -> Result<()> {
            let slot = match dir_inner.scan_dir_for_slot(&fs, name)? {
                DirScanResult::Slot(slot) => slot,
                DirScanResult::NeedGrowth => dir_inner.grow_dir_block(&fs)?,
            };
            dir_inner.write_dir_entry(&fs, &slot, name, old.ino, dir_ft as u8)?;
            dir_inner.update_dir_timestamps_and_flags()?;
            Ok(())
        })();

        if let Err(err) = add_result {
            // SPEC: rollback link count on add_entry failure.
            old_inner.sub_links_count_saturating(1);
            return Err(err);
        }
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

        if Self::has_invalid_child_name(name) {
            return_errno!(Errno::EINVAL);
        }

        let fs = self.fs_arc()?;
        let parent_inner = self.inner.upread();
        let child_ino = parent_inner.find_entry(&fs, name)?;
        let child = fs.read_inode(child_ino)?;

        // SPEC: unlink rejects directories — use rmdir instead.
        if child.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        let mut parent_inner = parent_inner.upgrade();
        parent_inner.delete_entry(name)?;

        // Linux: inode_set_ctime_to_ts(inode, inode_get_ctime(dir))
        // then inode_dec_link_count.
        let mut child_inner = child.inner.write();
        child_inner.set_ctime(now());
        child_inner.sub_links_count_saturating(1);

        // Defer ext2_evict_inode-style reclamation to cache eviction.
        if child_inner.links_count() == 0 {
            child_inner.set_dtime(now());
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

        if Self::has_invalid_child_name(old_name) {
            return_errno!(Errno::EISDIR);
        }
        if Self::has_invalid_child_name(new_name) {
            return_errno!(Errno::EISDIR);
        }

        // SPEC: cross-filesystem rename check.
        let fs = self.fs_arc()?;
        let target_fs = target.fs_arc()?;
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
            if self.do_rename_attempt(target, old_name, new_name, &fs)? {
                return Ok(());
            }
        }

        return_errno_with_message!(
            Errno::EAGAIN,
            "rename retried due concurrent directory updates"
        );
    }

    fn do_rename_attempt(
        &self,
        target: &Inode,
        old_name: &str,
        new_name: &str,
        fs: &Arc<Ext2>,
    ) -> Result<bool> {
        // Phase 1: read current source/target snapshot without write locks.
        let ctx = self.prepare_rename_context(target, old_name, new_name, fs)?;

        // Phase 2: lock all participating inode inner domains in global ino order.
        let lock_targets = self.rename_lock_targets(&ctx);
        let mut guards = MultiInodeInnerGuards::lock(&lock_targets);

        // Phase 3: verify snapshot is still valid under locks.
        if !self.recheck_rename_state(&ctx, &guards)? {
            return Ok(false);
        }
        // Phase 4: apply rename mutations and persist metadata.
        self.validate_rename_overwrite(&ctx, &guards)?;
        self.apply_rename_with_locks(&ctx, &mut guards)?;
        Ok(true)
    }

    fn prepare_rename_context<'a>(
        &'a self,
        target: &'a Inode,
        old_name: &'a str,
        new_name: &'a str,
        fs: &'a Arc<Ext2>,
    ) -> Result<RenameContext<'a>> {
        let old_ino = {
            let source_inner = self.inner.read();
            source_inner.find_entry(fs, old_name)?
        };
        let old_inode = fs.read_inode(old_ino)?;
        let existing_ino = {
            let target_inner = target.inner.read();
            target_inner.find_entry(fs, new_name).ok()
        };
        let existing_inode = if let Some(ino) = existing_ino {
            Some(fs.read_inode(ino)?)
        } else {
            None
        };

        let old_is_dir = old_inode.type_ == InodeType::Dir;
        let moved_ft = Self::inode_type_to_dir_file_type(old_inode.type_) as u8;
        Ok(RenameContext {
            fs,
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
        Self::add_unique_rename_lock_target(&mut targets, ctx.source_dir);
        Self::add_unique_rename_lock_target(&mut targets, ctx.target_dir);
        Self::add_unique_rename_lock_target(&mut targets, ctx.old_inode.as_ref());
        if let Some(existing) = ctx.existing_inode.as_ref() {
            Self::add_unique_rename_lock_target(&mut targets, existing.as_ref());
        }
        targets
    }

    fn add_unique_rename_lock_target<'a>(targets: &mut Vec<&'a Inode>, inode: &'a Inode) {
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
        let rechecked_old_ino = source_inner.find_entry(ctx.fs, ctx.old_name)?;
        if rechecked_old_ino != ctx.old_ino {
            // Source entry changed after snapshot; caller should retry.
            return Ok(false);
        }

        let target_inner = guards.inner(ctx.target_dir.ino)?;
        let rechecked_existing_ino = target_inner.find_entry(ctx.fs, ctx.new_name).ok();
        if rechecked_existing_ino != ctx.existing_ino {
            // Destination state changed after snapshot; caller should retry.
            return Ok(false);
        }

        if ctx.old_is_dir {
            let old_inner = guards.inner(ctx.old_inode.ino())?;
            let dotdot_ino = old_inner.find_entry(ctx.fs, "..")?;
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
            if !existing_inner.empty_dir(ctx.fs, existing.ino()) {
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
                ctx.target_dir,
                dir_inner,
                ctx.fs,
                ctx.new_name,
                ctx.old_ino,
                ctx.moved_ft,
                ctx.existing_inode.is_some(),
            )?;
            let old_target = dir_inner.find_entry_target(ctx.fs, ctx.old_name)?;
            dir_inner.delete_entry_in_page_cache(ctx.fs, &old_target)?;
            if ctx.old_is_dir {
                if ctx.existing_inode.is_none() {
                    dir_inner.add_links_count_saturating(1);
                }
                dir_inner.sub_links_count_saturating(1);
            }
            dir_inner.update_dir_timestamps_and_flags()?;
        } else {
            // Cross-directory: publish destination first, then remove source entry.
            // This mirrors Linux ext2 rename sequencing.
            {
                let target_inner = guards.inner_mut(ctx.target_dir.ino)?;
                Self::apply_rename_target_locked(
                    ctx.target_dir,
                    target_inner,
                    ctx.fs,
                    ctx.new_name,
                    ctx.old_ino,
                    ctx.moved_ft,
                    ctx.existing_inode.is_some(),
                )?;
                if ctx.old_is_dir && ctx.existing_inode.is_none() {
                    target_inner.add_links_count_saturating(1);
                }
                target_inner.update_dir_timestamps_and_flags()?;
            }
            {
                let source_inner = guards.inner_mut(ctx.source_dir.ino)?;
                let source_de = source_inner.find_entry_target(ctx.fs, ctx.old_name)?;
                source_inner.delete_entry_in_page_cache(ctx.fs, &source_de)?;
                if ctx.old_is_dir {
                    source_inner.sub_links_count_saturating(1);
                }
                source_inner.update_dir_timestamps_and_flags()?;
            }
        }

        if let Some(existing) = ctx.existing_inode.as_ref() {
            // Replaced inode can be distinct from moved inode, or the same inode in corner cases.
            let existing_inner = guards.inner_mut(existing.ino())?;
            existing_inner.set_ctime(now());
            if ctx.old_is_dir {
                existing_inner.sub_links_count_saturating(1);
            }
            existing_inner.sub_links_count_saturating(1);
            if existing_inner.links_count() == 0 {
                existing_inner.set_dtime(now());
            }
        }

        let old_inner = guards.inner_mut(ctx.old_inode.ino())?;
        old_inner.set_ctime(now());
        if ctx.old_is_dir && !ctx.is_same_dir() {
            let dotdot = old_inner.find_entry_target(ctx.fs, "..")?;
            old_inner.set_link_in_page_cache(
                ctx.fs,
                &dotdot,
                ctx.target_dir.ino,
                DirEntryFileType::Dir as u8,
            )?;
            old_inner.remove_flags(FileFlags::INDEX_DIR);
        }
        Ok(())
    }

    fn apply_rename_target_locked(
        _target: &Inode,
        target_inner: &mut InodeInner,
        fs: &Arc<Ext2>,
        new_name: &str,
        old_ino: u32,
        moved_ft: u8,
        has_existing: bool,
    ) -> Result<()> {
        if has_existing {
            // Existing destination entry: ext2_set_link semantics.
            let target_de = target_inner.find_entry_target(fs, new_name)?;
            target_inner.set_link_in_page_cache(fs, &target_de, old_ino, moved_ft)?;
            return Ok(());
        }

        // No destination entry: ext2_add_link semantics.
        let slot = match target_inner.scan_dir_for_slot(fs, new_name)? {
            DirScanResult::Slot(slot) => slot,
            DirScanResult::NeedGrowth => target_inner.grow_dir_block(fs)?,
        };
        target_inner.write_dir_entry(fs, &slot, new_name, old_ino, moved_ft)?;
        Ok(())
    }

    fn validate_set_link_input(&self, name: &str, new_ino: u32, fs: &Ext2) -> Result<()> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > u8::MAX as usize || name_bytes == b"." {
            return_errno!(Errno::EINVAL);
        }

        let max_inumber = fs.super_block().total_inodes();
        if new_ino < ROOT_INO || new_ino > max_inumber {
            return_errno!(Errno::EINVAL);
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
        let fs = self.fs_arc()?;
        self.validate_set_link_input(name, new_ino, &fs)?;
        let mut inner = self.inner.write();
        let target = inner.find_entry_target(&fs, name)?;
        inner.set_link_in_page_cache(&fs, &target, new_ino, file_type as u8)?;
        if update_times {
            inner.update_dir_timestamps_and_flags()?;
        } else {
            inner.remove_flags(FileFlags::INDEX_DIR);
        }
        Ok(())
    }

    pub(super) fn extension(&self) -> &Extension {
        &self.extension
    }

    pub(super) fn page_cache_vmo(&self) -> Arc<Vmo> {
        self.inner.read().page_cache().pages().clone()
    }
}

#[derive(Debug)]
pub(super) struct InodeBackend {
    /// Serializes backend traversal vs foreground mapping mutations.
    mapping: RwMutex<InodeMapping>,
    /// Cached `npages` bound for PageCache.
    npages: AtomicUsize,
    /// Filesystem handle for indirect I/O and BIO submission.
    fs: Weak<Ext2>,
}

impl InodeBackend {
    pub(super) fn new(mapping: InodeMapping, fs: Weak<Ext2>, npages: usize) -> Arc<Self> {
        Arc::new(Self {
            mapping: RwMutex::new(mapping),
            npages: AtomicUsize::new(npages),
            fs,
        })
    }

    pub(super) fn npages(&self) -> usize {
        self.npages.load(Ordering::Acquire)
    }

    fn fs_arc(&self) -> Result<Arc<Ext2>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))
    }
}

impl PageCacheBackend for InodeBackend {
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter> {
        let mapping = self.mapping.read();
        let fs = self.fs_arc()?;
        let iblock = u32::try_from(idx)
            .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;

        match mapping.get_block(&fs, iblock)? {
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
        let mapping = self.mapping.read();
        let fs = self.fs_arc()?;
        let iblock = u32::try_from(idx)
            .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;

        let bid = mapping.get_block(&fs, iblock)?.ok_or_else(|| {
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
    fn new(desc: Dirty<InodeDesc>, fs: Weak<Ext2>) -> Self {
        let num_page_bytes = (desc.size as usize).align_up(BLOCK_SIZE);
        let num_pages = num_page_bytes / BLOCK_SIZE;
        let backend = InodeBackend::new(
            InodeMapping::new_with_fs(
                InodeMappingDesc::from_parts(desc.blocks, desc.block_ptrs),
                fs.clone(),
            ),
            fs.clone(),
            num_pages,
        );
        let page_cache_backend: Weak<dyn PageCacheBackend> = Arc::downgrade(&backend) as _;
        // Keep page-cache capacity aligned with inode size so `npages`/VMO window
        // and on-disk data extent stay consistent from mount time.
        let page_cache = if num_page_bytes == 0 {
            PageCache::new(page_cache_backend)
        } else {
            PageCache::with_capacity(num_page_bytes, page_cache_backend)
        }
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

    fn sync_desc_mapping_from_snapshot(&mut self, mapping_desc: InodeMappingDesc) {
        self.desc.blocks = mapping_desc.blocks;
        self.desc.block_ptrs = mapping_desc.block_ptrs;
    }

    fn resize_page_cache_and_update_npages(&mut self, new_size_bytes: usize) -> Result<()> {
        self.page_cache.resize(new_size_bytes)?;
        self.backend.npages.store(
            self.page_cache.pages().size() / BLOCK_SIZE,
            Ordering::Release,
        );
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

    fn touch_ctime(&mut self, t: Duration) {
        self.set_ctime(t);
    }

    fn touch_mtime_ctime(&mut self, t: Duration) {
        self.set_mtime(t);
        self.set_ctime(t);
    }

    fn dtime(&self) -> Duration {
        self.desc.dtime
    }

    fn set_dtime(&mut self, t: Duration) {
        self.desc.dtime = t;
    }

    fn links_count(&self) -> u16 {
        self.desc.links_count
    }

    fn set_links_count(&mut self, nlinks: u16) {
        self.desc.links_count = nlinks;
    }

    fn add_links_count_saturating(&mut self, delta: u16) {
        self.desc.links_count = self.desc.links_count.saturating_add(delta);
    }

    fn sub_links_count_saturating(&mut self, delta: u16) {
        self.desc.links_count = self.desc.links_count.saturating_sub(delta);
    }

    fn flags(&self) -> FileFlags {
        self.desc.flags
    }

    fn set_flags(&mut self, flags: FileFlags) {
        self.desc.flags = flags;
    }

    fn remove_flags(&mut self, flags: FileFlags) {
        self.desc.flags.remove(flags);
    }

    fn file_acl(&self) -> u32 {
        self.desc.file_acl
    }

    fn set_file_acl(&mut self, file_acl: u32) {
        self.desc.file_acl = file_acl;
    }

    fn fs_arc(&self) -> Result<Arc<Ext2>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))
    }

    fn encode_device_id(&mut self, device_id: u64) -> Result<()> {
        let mapping_backend = self.backend().clone();
        let mut mapping = mapping_backend.mapping.write();
        mapping.desc.encode_device_id(device_id);
        let snapshot = *mapping.get_desc();
        self.sync_desc_mapping_from_snapshot(snapshot);
        Ok(())
    }

    /// Persists inode to disk.
    ///
    /// # Lock
    /// The caller must hold `inner.write()`.
    fn persist(&mut self, ino: u32, type_: InodeType, fs: &Ext2) -> Result<()> {
        debug_assert_eq!(self.inode_type(), type_);
        let raw = RawInode::from(&*self.desc);
        fs.write_inode_desc(ino, &raw)?;
        self.clear_dirty();
        Ok(())
    }

    /// Initializes an empty directory with `.` and `..` entries.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:617 (ext2_make_empty)
    fn make_empty(&mut self, ino: u32, parent_ino: u32, fs: &Ext2) -> Result<()> {
        let block_size = fs.block_size();
        let old_size = self.file_size();
        let (old_mapping_desc, new_mapping_desc, new_bid) = {
            let mapping_backend = Arc::clone(self.backend());
            let mut mapping = mapping_backend.mapping.write();
            if mapping.desc.block_ptrs[0] != 0 {
                return_errno_with_message!(Errno::EIO, "dir block pointer already occupied");
            }
            let old_mapping_desc = *mapping.get_desc();
            let bid = mapping
                .get_or_alloc_block(fs, 0, true)?
                .ok_or_else(|| {
                    Error::with_message(Errno::ENOSPC, "failed to allocate first dir block")
                })?
                .to_raw() as u32;
            let new_mapping_desc = *mapping.get_desc();
            (old_mapping_desc, new_mapping_desc, bid)
        };
        self.sync_desc_mapping_from_snapshot(new_mapping_desc);
        self.set_file_size(block_size);

        if let Err(err) = self.resize_page_cache_and_update_npages(block_size) {
            self.rollback_make_empty(old_size, old_mapping_desc, new_bid, fs);
            return Err(err);
        }

        let mut buf = vec![0u8; block_size];
        Self::write_dir_entry_bytes(
            &mut buf,
            0,
            ino,
            DirEntry::dir_rec_len(1),
            b".",
            DirEntryFileType::Dir as u8,
        )?;
        let dot_len = DirEntry::dir_rec_len(1) as usize;
        Self::write_dir_entry_bytes(
            &mut buf,
            dot_len,
            parent_ino,
            (block_size - dot_len) as u16,
            b"..",
            DirEntryFileType::Dir as u8,
        )?;

        if let Err(err) = self.page_cache().pages().write_bytes(0, &buf) {
            self.page_cache().discard_range(0..block_size);
            if let Err(resize_err) = self.resize_page_cache_and_update_npages(old_size) {
                error!(
                    "ext2: make_empty rollback resize failed: old_size={}, err={:?}",
                    old_size, resize_err
                );
            }
            self.rollback_make_empty(old_size, old_mapping_desc, new_bid, fs);
            return Err(err.into());
        }

        Ok(())
    }

    fn rollback_make_empty(
        &mut self,
        old_size: usize,
        old_mapping_desc: InodeMappingDesc,
        new_bid: u32,
        fs: &Ext2,
    ) {
        self.page_cache().discard_range(0..fs.block_size());
        self.set_file_size(old_size);
        let mapping_backend = Arc::clone(self.backend());
        let mut mapping = mapping_backend.mapping.write();
        mapping.desc.block_ptrs = old_mapping_desc.block_ptrs;
        mapping.desc.blocks = old_mapping_desc.blocks;
        let _ = fs.free_blocks(new_bid, 1);
        let mapping_desc = *mapping.get_desc();
        drop(mapping);
        self.sync_desc_mapping_from_snapshot(mapping_desc);
    }

    /// Reads file data directly from data blocks into `writer`.
    ///
    /// Linux: /root/linux/fs/ext2/file.c:168 (ext2_dio_read_iter)
    fn read_direct_at(
        &self,
        fs: &Ext2,
        offset: usize,
        end: usize,
        writer: &mut VmWriter,
    ) -> Result<()> {
        let block_size = fs.block_size();
        let mut current_offset = offset;
        let mut mapping = self.backend.mapping.write();
        while current_offset < end {
            let iblock = u32::try_from(current_offset / block_size)
                .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;
            let offset_in_block = current_offset % block_size;
            let bytes_this_block = (block_size - offset_in_block).min(end - current_offset);

            match mapping.get_block(fs, iblock)? {
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

            current_offset += bytes_this_block;
        }

        Ok(())
    }

    /// Writes file data directly to already-allocated data blocks.
    ///
    /// Linux: /root/linux/fs/ext2/file.c:214 (ext2_dio_write_iter)
    fn write_direct_at(&self, fs: &Ext2, offset: usize, reader: &mut VmReader) -> Result<()> {
        let block_size = fs.block_size();
        let write_len = reader.remain();
        // end is already checked in `Inode::write_direct_at`.
        let end = offset + write_len;
        let mut current_offset = offset;
        let mut mapping = self.backend.mapping.write();

        while current_offset < end {
            let iblock = u32::try_from(current_offset / block_size)
                .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;
            let offset_in_block = current_offset % block_size;
            let bytes_this_block = (block_size - offset_in_block).min(end - current_offset);
            let bid = mapping.get_block(fs, iblock)?.ok_or_else(|| {
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

            current_offset += bytes_this_block;
        }

        Ok(())
    }

    /// Checks whether this directory contains only `.` and `..` as live entries.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:659 (ext2_empty_dir)
    fn empty_dir(&self, fs: &Ext2, self_ino: u32) -> bool {
        if self.inode_type() != InodeType::Dir {
            return false;
        }

        let block_size = fs.block_size();
        let size = self.file_size();
        let max_inumber = fs.super_block().total_inodes();
        let data_blocks = size.div_ceil(block_size);

        for block_idx in 0..data_blocks {
            let mut buf = vec![0u8; block_size];
            let block_offset = block_idx * block_size;
            if self
                .page_cache
                .pages()
                .read_bytes(block_offset, &mut buf)
                .is_err()
            {
                return false;
            }

            let block_offset = block_idx * block_size;
            let limit = (size - block_offset).min(block_size);
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

    /// Finds a directory entry by name and returns its inode number.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:342 (ext2_find_entry)
    fn find_entry(&self, fs: &Ext2, name: &str) -> Result<u32> {
        if self.inode_type() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let sb = fs.super_block();
        let block_size = fs.block_size();
        let size = self.file_size();
        let max_inumber = sb.total_inodes();
        // SPEC: bound directory scan by i_size-derived pages (Linux dir_pages).
        // Linux: /root/linux/fs/ext2/dir.c:349 (npages = dir_pages(dir)).
        let max_blocks = size.div_ceil(block_size);

        for block_idx in 0..max_blocks {
            let block_offset = block_idx * block_size;
            let remain = size - block_offset;
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
        }

        return_errno!(Errno::ENOENT);
    }

    fn shrink(&mut self, new_size: usize) -> Result<()> {
        let fs = self.fs_arc()?;
        let block_size = fs.block_size();
        let old_size = self.desc.size as usize;
        let old_size_aligned = old_size.align_up(block_size);
        let new_size_aligned = new_size.align_up(block_size);

        if new_size_aligned < old_size_aligned {
            self.page_cache
                .discard_range(new_size_aligned..old_size_aligned);
        }
        self.resize_page_cache_and_update_npages(new_size_aligned)?;

        let backend = self.backend.clone();
        let mut mapping = backend.mapping.write();
        if let Err(err) = mapping.truncate_blocks(&fs, new_size) {
            self.resize_page_cache_and_update_npages(old_size_aligned)?;
            return Err(err);
        }
        let snapshot = *mapping.get_desc();
        self.sync_desc_mapping_from_snapshot(snapshot);
        self.set_file_size(new_size);
        self.touch_mtime_ctime(now());
        Ok(())
    }

    fn expand(&mut self, new_size: usize) -> Result<()> {
        self.resize_page_cache_and_update_npages(new_size)?;
        self.set_file_size(new_size);
        self.touch_mtime_ctime(now());
        Ok(())
    }

    /// Reads directory entries starting at byte offset and feeds visitor.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:257 (ext2_readdir)
    fn readdir_at(
        &self,
        fs: &Ext2,
        offset: usize,
        visitor: &mut dyn DirentVisitor,
    ) -> Result<usize> {
        if self.inode_type() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let size = self.file_size();
        let min_rec_len = DirEntry::dir_rec_len(1) as usize;
        if size < min_rec_len || offset > size - min_rec_len {
            return Ok(0);
        }

        let sb = fs.super_block();
        let block_size = fs.block_size();
        let max_inumber = sb.total_inodes();

        let start_block = offset / block_size;
        let mut current_offset = offset;
        let mut advanced = 0usize;

        let total_blocks = (size + block_size - 1) / block_size;
        for block_idx in start_block..total_blocks {
            let block_offset = block_idx * block_size;
            if block_offset >= size {
                break;
            }
            let remain = size - block_offset;
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
                let entry_offset = block_offset + inner_off;
                let next_offset = entry_offset + entry.rec_len as usize;

                if next_offset <= current_offset {
                    inner_off = next_offset - block_offset;
                    continue;
                }
                if entry_offset < current_offset {
                    current_offset = next_offset;
                    inner_off = next_offset - block_offset;
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
                        advanced = current_offset - offset;
                        return Ok(advanced);
                    }
                }

                current_offset = next_offset;
                inner_off = next_offset - block_offset;
            }

            advanced = current_offset - offset;
        }

        Ok(advanced)
    }

    fn write_link(&mut self, target: &str) -> Result<()> {
        let target_len = target.len();
        if target.len() < MAX_FAST_SYMLINK_LEN {
            // fast path
            self.desc.block_ptrs.as_bytes_mut()[..target_len].copy_from_slice(target.as_bytes());
        } else {
            let fs = self.fs_arc()?;
            let block_size = fs.block_size();
            // slow path, write to page cache
            self.prepare_continuous_blocks(&fs, 0, target_len, block_size, false)?;
            self.page_cache()
                .pages()
                .write_bytes(0, target.as_bytes())?;
        }

        self.set_file_size(target_len);
        self.touch_mtime_ctime(now());

        Ok(())
    }

    fn read_link(&self) -> Result<String> {
        let link_size = self.file_size();
        let fs = self.fs_arc()?;
        let block_size = fs.block_size();
        if self.desc.is_fast_symlink(block_size) {
            let read_len = link_size.min(MAX_FAST_SYMLINK_LEN - 1);
            let mut raw_bytes = [0u8; MAX_FAST_SYMLINK_LEN];
            for (idx, block_ptr) in self.desc.block_ptrs.iter().enumerate() {
                let offset = idx * size_of::<u32>();
                raw_bytes[offset..offset + size_of::<u32>()]
                    .copy_from_slice(&block_ptr.to_le_bytes());
            }

            return String::from_utf8(raw_bytes[..read_len].to_vec())
                .map_err(|_| Error::with_message(Errno::EIO, "symlink target is not valid UTF-8"));
        }

        let mut target = vec![0u8; link_size];
        self.page_cache()
            .pages()
            .read_bytes(0, &mut target)
            .map_err(|_| {
                Error::with_message(Errno::EIO, "failed to read symlink target from page cache")
            })?;

        String::from_utf8(target)
            .map_err(|_| Error::with_message(Errno::EIO, "symlink target is not valid UTF-8"))
    }

    fn prepare_continuous_blocks(
        &mut self,
        fs: &Ext2,
        offset: usize,
        end: usize,
        block_size: usize,
        discard_page_cache: bool,
    ) -> Result<()> {
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        let old_size = self.file_size();
        self.allocate_range_blocks(fs, offset, end, block_size)?;

        if end > old_size {
            self.resize_page_cache_and_update_npages(end.align_up(block_size))?;
            self.set_file_size(end);
        }

        if discard_page_cache {
            let discard_start = offset.min(old_size);
            let discard_end = end.min(old_size);
            if discard_start < discard_end {
                self.page_cache().discard_range(discard_start..discard_end);
            }
        }

        Ok(())
    }

    /// Returns whether a direct write touching current i_size must fall back to buffered I/O.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:823-854 (`ext2_iomap_begin`)
    fn should_fallback_direct_write(
        &self,
        fs: &Ext2,
        offset: usize,
        end: usize,
        block_size: usize,
    ) -> Result<bool> {
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        let scan_end = end.min(self.file_size());
        if offset >= scan_end {
            return Ok(false);
        }

        let start_block = offset / block_size;
        let end_block = scan_end.div_ceil(block_size);
        let mapping_backend = Arc::clone(self.backend());
        let mapping = mapping_backend.mapping.read();

        for iblock in start_block..end_block {
            let iblock = u32::try_from(iblock)
                .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;
            if mapping.get_block(fs, iblock)?.is_none() {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Zeroes the hidden stale-data window in the old EOF block before direct EOF extension.
    ///
    /// Linux: /root/linux/fs/direct-io.c:852-875 (`dio_zero_block`)
    fn zero_direct_write_eof_tail(
        &self,
        fs: &Ext2,
        old_size: usize,
        offset: usize,
        block_size: usize,
    ) -> Result<()> {
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }
        if offset <= old_size || old_size.is_multiple_of(block_size) {
            return Ok(());
        }

        let zero_end = offset.min(old_size.align_up(block_size));
        if zero_end <= old_size {
            return Ok(());
        }

        let eof_iblock = u32::try_from(old_size / block_size)
            .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;
        let mapping_backend = Arc::clone(self.backend());
        let mapping = mapping_backend.mapping.read();
        let Some(_bid) = mapping.get_block(fs, eof_iblock)? else {
            return Ok(());
        };
        drop(mapping);

        self.page_cache().fill_zeros(old_size..zero_end)?;
        self.sync_data_pages()?;
        Ok(())
    }

    /// Allocates missing data blocks that cover the requested file byte range.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:624 (`ext2_get_blocks`)
    fn allocate_range_blocks(
        &mut self,
        fs: &Ext2,
        offset: usize,
        end: usize,
        block_size: usize,
    ) -> Result<Vec<Bid>> {
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }
        if end <= offset {
            return Ok(Vec::new());
        }

        let start_block = offset / block_size;
        let end_block = end.div_ceil(block_size);

        let (mapping_desc, new_blocks, alloc_result) = {
            let mapping_backend = self.backend().clone();
            let mut mapping = mapping_backend.mapping.write();
            let mut new_blocks = Vec::new();
            let alloc_result = (|| -> Result<()> {
                for iblock in start_block..end_block {
                    let iblock = u32::try_from(iblock).map_err(|_| {
                        Error::with_message(Errno::EINVAL, "logical block number overflow")
                    })?;
                    if mapping.get_block(fs, iblock)?.is_some() {
                        continue;
                    }
                    let bid = mapping
                        .get_or_alloc_block(fs, iblock, true)?
                        .ok_or_else(|| {
                            Error::with_message(
                                Errno::EIO,
                                "missing block mapping after allocation",
                            )
                        })?;
                    new_blocks.push(bid);
                }
                Ok(())
            })();
            (*mapping.get_desc(), new_blocks, alloc_result)
        };
        self.sync_desc_mapping_from_snapshot(mapping_desc);

        if !new_blocks.is_empty() {
            self.set_ctime(now());
        }

        alloc_result?;
        Ok(new_blocks)
    }

    /// Zeroes newly allocated data blocks before exposing them via mapped reads.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:742-757 (`ext2_get_blocks`, DAX zeroout path)
    fn zero_new_blocks(&self, fs: &Ext2, blocks: &[Bid], block_size: usize) -> Result<()> {
        if blocks.is_empty() {
            return Ok(());
        }
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        let zero_block = vec![0u8; block_size];
        for &bid in blocks {
            fs.block_device()
                .write_bytes(bid.to_offset(), &zero_block)
                .map_err(|_| {
                    Error::with_message(Errno::EIO, "failed to zero newly allocated data block")
                })?;
        }
        Ok(())
    }

    fn write_failed_cleanup(&mut self, fs: &Ext2, old_size: usize, end: usize, block_size: usize) {
        if end <= old_size {
            return;
        }

        let old_size_aligned = old_size.align_up(block_size);
        let end_aligned = end.align_up(block_size);
        self.page_cache()
            .discard_range(old_size_aligned..end_aligned);

        if let Err(err) = self.resize_page_cache_and_update_npages(old_size_aligned) {
            error!(
                "ext2: write_at cleanup page cache resize failed: old_size_aligned={}, err={:?}",
                old_size_aligned, err
            );
        }

        let mapping_desc = {
            let mapping_backend = Arc::clone(self.backend());
            let mut mapping = mapping_backend.mapping.write();
            if let Err(err) = mapping.truncate_blocks(fs, old_size) {
                error!(
                    "ext2: write_at cleanup truncate_blocks failed: old_size={}, err={:?}",
                    old_size, err
                );
                None
            } else {
                Some(*mapping.get_desc())
            }
        };
        if let Some(mapping_desc) = mapping_desc {
            self.sync_desc_mapping_from_snapshot(mapping_desc);
        }

        self.set_file_size(old_size);
    }

    /// Resolves a logical block to physical, allocating a missing branch if requested.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:624 (ext2_get_blocks, create path)
    /// TODO: refactor this into a fast path
    fn get_or_alloc_block(&mut self, iblock: u32, create: bool) -> Result<Option<Bid>> {
        let fs = self.fs_arc()?;
        let mapping_backend = Arc::clone(self.backend());
        let mut mapping = mapping_backend.mapping.write();
        let old_blocks = mapping.blocks_512();
        let mapped = mapping.get_or_alloc_block(&fs, iblock, create)?;
        let allocated = mapping.blocks_512() != old_blocks;
        let mapping_desc = *mapping.get_desc();
        drop(mapping);
        if allocated {
            self.set_ctime(now());
        }
        self.sync_desc_mapping_from_snapshot(mapping_desc);
        Ok(mapped)
    }

    /// Phase 1: scan directory blocks for reusable slot or duplicate.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:476 (ext2_add_link scan loop)
    fn scan_dir_for_slot(&self, fs: &Ext2, name: &str) -> Result<DirScanResult> {
        if self.inode_type() != InodeType::Dir {
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

        let size = self.file_size();
        let max_inumber = fs.super_block().total_inodes();
        let data_blocks = size.div_ceil(block_size);

        for block_idx in 0..data_blocks {
            let block_offset = block_idx * block_size;
            let limit = (size - block_offset).min(block_size);
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
        let old_size = self.file_size();
        let data_blocks = old_size.div_ceil(block_size);
        let growth_iblock = u32::try_from(data_blocks)
            .map_err(|_| Error::with_message(Errno::EINVAL, "directory block index overflow"))?;

        // SPEC: allocation under mapping.write(); no PageCache/VMO operations in this scope.
        let mapping_desc = {
            let mapping_backend = Arc::clone(self.backend());
            let mut mapping = mapping_backend.mapping.write();
            mapping
                .get_or_alloc_block(fs, growth_iblock, true)?
                .ok_or_else(|| {
                    Error::with_message(Errno::ENOSPC, "failed to grow directory block")
                })?;
            *mapping.get_desc()
        };
        self.sync_desc_mapping_from_snapshot(mapping_desc);

        let new_size = old_size.saturating_add(block_size);
        self.set_file_size(new_size);
        if let Err(err) = self.resize_page_cache_and_update_npages(new_size) {
            // SPEC: rollback allocated growth on resize failure.
            self.page_cache.discard_range(old_size..new_size);
            self.set_file_size(old_size);
            let mapping_desc = {
                let mapping_backend = Arc::clone(self.backend());
                let mut mapping = mapping_backend.mapping.write();
                mapping.truncate_blocks(fs, old_size)?;
                *mapping.get_desc()
            };
            self.sync_desc_mapping_from_snapshot(mapping_desc);
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
    fn write_dir_entry(
        &self,
        fs: &Ext2,
        slot: &DirSlotInfo,
        name: &str,
        ino: u32,
        ft: u8,
    ) -> Result<()> {
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

    /// Locate a target entry by name for delete/set_link operations.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:342 (ext2_find_entry)
    fn find_entry_target(&self, fs: &Ext2, name: &str) -> Result<DirEntryTarget> {
        let max_inumber = fs.super_block().total_inodes();
        let block_size = fs.block_size();
        let size = self.file_size();
        let name_bytes = name.as_bytes();

        for block_idx in 0..size.div_ceil(block_size) {
            let block_offset = block_idx * block_size;
            let limit = (size - block_offset).min(block_size);
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
    fn delete_entry_in_page_cache(&self, fs: &Ext2, target: &DirEntryTarget) -> Result<()> {
        let block_size = fs.block_size();
        let block_base = (target.dir_offset / block_size).saturating_mul(block_size);
        let entry_offset = target.dir_offset.saturating_sub(block_base);
        let limit = (self.file_size())
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
    fn set_link_in_page_cache(
        &self,
        fs: &Ext2,
        target: &DirEntryTarget,
        new_ino: u32,
        ft: u8,
    ) -> Result<()> {
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

    /// Add a directory entry.
    ///
    /// Linux: /root/linux/fs/ext2/namei.c:various (ext2_add_link)
    fn add_entry(&mut self, name: &str, new_ino: u32, file_type: DirEntryFileType) -> Result<()> {
        let fs = self.fs_arc()?;
        let slot = match self.scan_dir_for_slot(&fs, name)? {
            DirScanResult::Slot(slot) => slot,
            DirScanResult::NeedGrowth => self.grow_dir_block(&fs)?,
        };
        self.write_dir_entry(&fs, &slot, name, new_ino, file_type as u8)?;
        self.update_dir_timestamps_and_flags()?;
        Ok(())
    }

    /// Delete a directory entry by name.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:560 (ext2_delete_entry)
    fn delete_entry(&mut self, name: &str) -> Result<()> {
        let fs = self.fs_arc()?;
        let target = self.find_entry_target(&fs, name).map_err(|err| {
            if err.error() == Errno::ENOENT {
                Error::with_message(Errno::EIO, "dir entry not found for delete")
            } else {
                err
            }
        })?;
        self.delete_entry_in_page_cache(&fs, &target)?;
        self.update_dir_timestamps_and_flags()?;
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
        // explicitly trigger truncate-based release on rollback/removal paths.
        // TODO: Move this logic into a shared truncate/evict pipeline, and make
        // free_inode trigger it instead of per-call-site cleanup.
        // SPEC: mapping truncation happens under mapping.write().
        let mapping_desc = {
            let mapping_backend = Arc::clone(self.backend());
            let mut mapping = mapping_backend.mapping.write();
            mapping.truncate_blocks(fs, 0)?;
            *mapping.get_desc()
        };
        self.sync_desc_mapping_from_snapshot(mapping_desc);
        self.page_cache.discard_range(0..self.file_size());
        self.resize_page_cache_and_update_npages(0)?;
        // SPEC: cleanup path must leave directory size at zero.
        self.set_file_size(0);
        Ok(())
    }

    fn update_dir_timestamps_and_flags(&mut self) -> Result<()> {
        let current = now();
        self.touch_mtime_ctime(current);
        self.remove_flags(FileFlags::INDEX_DIR);
        Ok(())
    }

    fn sync_data_pages(&self) -> Result<()> {
        // SPEC: file_write_and_wait_range on an empty file is a no-op.
        // Linux: /root/linux/mm/filemap.c:782-783.
        let file_size = self.file_size();
        if file_size == 0 {
            return Ok(());
        }

        // SPEC: evict_range writes back dirty pages in [0, file_size), waits for
        // completion, and keeps pages cached as UpToDate.
        self.page_cache.evict_range(0..file_size)
    }
}

/// Acquires `inner.read()` locks on two inodes in ascending ino order.
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
    generation: u32,
    block_ptrs: [u32; 15],
}

impl InodeDesc {
    pub fn type_(&self) -> InodeType {
        self.type_
    }

    /// Determines whether the symlink payload is stored inline in `i_block[15]`.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:48-55 (ext2_inode_is_fast_symlink).
    fn is_fast_symlink(&self, block_size: usize) -> bool {
        let ea_blocks = if self.file_acl != 0 {
            (block_size / SECTOR_SIZE) as u32
        } else {
            0
        };

        self.type_ == InodeType::SymLink && self.blocks.checked_sub(ea_blocks) == Some(0)
    }

    /// Decodes Linux ext2 old/new special-file device encoding from `i_block`.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1495-1500
    /// Linux: /root/linux/include/linux/kdev_t.h:34-37,46-51
    fn decode_device_id(&self) -> u64 {
        let (major, minor) = if self.block_ptrs[0] != 0 {
            let val = self.block_ptrs[0];
            // SPEC: old_decode_dev((major << 8) | minor) with 8-bit major/minor.
            (((val >> 8) & 0xFF), (val & 0xFF))
        } else {
            let dev = self.block_ptrs[1];
            // SPEC: new_decode_dev bit layout in Linux kdev_t.h.
            (
                ((dev & 0xFFF00) >> 8),
                ((dev & 0xFF) | ((dev >> 12) & 0xFFF00)),
            )
        };

        encode_device_numbers(major, minor)
    }

    /// Encodes an Asterinas u64 device ID into Linux ext2 `i_block` layout.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1589-1599
    /// Linux: /root/linux/include/linux/kdev_t.h:24-32,39-44
    fn encode_device_id(&mut self, device_id: u64) {
        let (major, minor) = decode_device_numbers(device_id);

        // SPEC: old_valid_dev => MAJOR/MINOR must both fit in 8 bits.
        if major < 256 && minor < 256 {
            self.block_ptrs[0] = (major << 8) | minor;
            self.block_ptrs[1] = 0;
        } else {
            self.block_ptrs[0] = 0;
            self.block_ptrs[1] = (minor & 0xFF) | (major << 8) | ((minor & !0xFF) << 12);
            self.block_ptrs[2] = 0;
        }
    }
}

impl TryFrom<&RawInode> for InodeDesc {
    type Error = Error;
    fn try_from(raw: &RawInode) -> Result<Self> {
        let mode = raw.mode;

        if raw.links_count == 0 && (mode == 0 || raw.dtime != 0) {
            return_errno_with_message!(Errno::ESTALE, "inode has been deleted");
        }

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
        let mapping = InodeMappingDesc::from_raw(raw);

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
            blocks: mapping.blocks,
            flags,
            file_acl: raw.file_acl,
            generation: raw.generation,
            block_ptrs: mapping.block_ptrs,
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
            links_count: desc.links_count,
            blocks: desc.blocks,
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
    use core::time::Duration;

    use ostd::{mm::VmIo, prelude::ktest};

    use super::*;
    use crate::{
        fs::{
            ext2::{
                fs::ROOT_INO,
                testkit::{
                    self, CollectDirentVisitor, ErrorBioDisk, Ext2FixtureBuilder, RawInodeBuilder,
                    StopAfterVisitor, encode_dir_entry,
                },
            },
            utils::{
                Inode as VfsInodeTrait, InodeIo, InodeMode, MknodType, StatusFlags, XattrName,
                XattrNamespace, XattrSetFlags,
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
        VfsInodeTrait::metadata(inode.as_ref()).nlinks
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

        // Linux ext2_create intent: allocate inode then publish dir entry.
        let created = root
            .create(
                "alpha",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();
        assert_eq!(lookup_ino(&root, "alpha").unwrap(), created.ino());
        assert_eq!(inode_nlinks(&created), 1);

        // Linux ext2_mkdir intent: child links=2 and parent link count +1.
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

        // Linux ext2_link intent: increase nlink before publishing name.
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

        // Linux ext2_unlink intent: remove name then decrement target nlink.
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
        drop(old);
        f.ext2.sync_all().unwrap();
        assert_eq!(
            f.ext2.read_inode(old_ino).unwrap_err().error(),
            Errno::ENOENT
        );
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
        let target = root
            .create(
                "target",
                InodeType::File,
                FilePerm::from_bits_truncate(0o644),
            )
            .unwrap();

        let ctime_before = root.ctime();
        let mtime_before = root.mtime();

        // Linux ext2_set_link with update_times=false keeps ctime/mtime unchanged.
        root.set_link("src", target.ino(), DirEntryFileType::File, false)
            .unwrap();
        assert_eq!(lookup_ino(&root, "src").unwrap(), target.ino());
        assert_eq!(ctime_before, root.ctime());
        assert_eq!(mtime_before, root.mtime());

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

        // Linux ext2_rename replacement path: dst is overwritten and old name removed.
        root.rename("old", &root, "new").unwrap();
        assert_eq!(lookup_ino(&root, "new").unwrap(), old_ino);
        assert_eq!(lookup_ino(&root, "old").unwrap_err().error(), Errno::ENOENT);
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
    fn unlink_special_inode_does_not_free_encoded_rdev_block() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();
        let free_blocks_before = f.ext2.super_block().free_blocks_count();

        let special = VfsInodeTrait::mknod(
            root.as_ref(),
            "null",
            InodeMode::from_bits_truncate(0o666),
            MknodType::CharDevice(encode_device_numbers(1, 3)),
        )
        .unwrap();
        let special_ino = special.ino();

        root.unlink("null").unwrap();
        drop(special);
        f.ext2.sync_all().unwrap();

        assert_eq!(f.ext2.super_block().free_blocks_count(), free_blocks_before);
        assert_eq!(f.ext2.read_inode(special_ino as u32).unwrap_err().error(), Errno::ENOENT);
    }

    #[ktest]
    fn raw_inode_roundtrip_preserves_special_inode_type_on_mode_update() {
        let mut raw = make_raw_inode(0o020600);
        raw.block[0] = 0x0103;

        let mut desc = InodeDesc::try_from(&raw).unwrap();
        assert_eq!(desc.type_, InodeType::CharDevice);
        assert_eq!(desc.perm.bits(), 0o600);
        assert_eq!(desc.decode_device_id(), encode_device_numbers(1, 3));

        desc.perm = FilePerm::from_bits_truncate(0o666);
        let updated_raw = RawInode::from(&desc);
        assert_eq!(updated_raw.mode, 0o020666);
        assert_eq!(updated_raw.block[0], 0x0103);
        assert_eq!(updated_raw.block[1], 0);
    }

    #[ktest]
    fn inode_desc_from_raw_err() {
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

        let mut stop_visitor = StopAfterVisitor::new(2);
        let stop_advanced = root.readdir_at(0, &mut stop_visitor).unwrap();
        let root_size = inode_size(&root);
        assert!(stop_advanced > 0 && stop_advanced < root_size);

        let first_entry_end = visitor.entries[0].3 + 1;
        let mut offset_visitor = CollectDirentVisitor::default();
        root.readdir_at(first_entry_end, &mut offset_visitor)
            .unwrap();
        assert_eq!(offset_visitor.entries.len(), 3);
        assert_eq!(offset_visitor.entries[0].0, "..");
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
        assert!(dir.empty_dir());
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

        let env = prepare_rmdir_env(false);
        let parent = &env.parent;
        let child = &env.child;

        parent.rmdir("sub").unwrap();
        assert_eq!(inode_nlinks(parent), 2);
        assert_eq!(
            lookup_ino(parent, "sub").unwrap_err().error(),
            Errno::ENOENT
        );
        assert_eq!(inode_size(child), 0);
        assert_eq!(inode_nlinks(child), 0);
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
        assert_eq!(VfsInodeTrait::metadata(file.as_ref()).blocks, 0);

        let on_disk = f.ext2.read_inode_desc(file.ino()).unwrap();
        assert_eq!(on_disk.size, 0);
        assert_eq!(on_disk.blocks, 0);
    }

    #[ktest]
    fn file_direct_write_sparse_hole_fallback_ok() {
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
        assert_eq!(VfsInodeTrait::metadata(link.as_ref()).blocks, 0);
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
        assert!(VfsInodeTrait::metadata(link.as_ref()).blocks > 0);
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

    #[ktest]
    fn falloc_keep_size_allocates_blocks_without_changing_size() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 68, 123, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();
        let free_before = f.ext2.super_block().free_blocks_count();

        file.fallocate(FallocMode::AllocateKeepSize, block_size, 512)
            .unwrap();
        assert_eq!(file.file_size(), 123);

        let free_after = f.ext2.super_block().free_blocks_count();
        assert_eq!(free_before.saturating_sub(free_after), 1);

        file.resize(block_size + 512).unwrap();

        let mut out = vec![0x5au8; 512];
        let mut out_writer = VmWriter::from(out.as_mut_slice()).to_fallible();
        assert_eq!(file.read_at(block_size, &mut out_writer).unwrap(), 512);
        assert!(out.iter().all(|byte| *byte == 0));
    }

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

    #[ktest]
    fn falloc_punch_hole_zeroes() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 70, 0, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();

        let payload = vec![0xabu8; block_size];
        let mut payload_reader = VmReader::from(payload.as_slice()).to_fallible();
        file.write_at(0, &mut payload_reader).unwrap();

        let punch_off = 128usize;
        let punch_len = 512usize;
        file.fallocate(FallocMode::PunchHoleKeepSize, punch_off, punch_len)
            .unwrap();

        let mut out = vec![0u8; block_size];
        let mut out_writer = VmWriter::from(out.as_mut_slice()).to_fallible();
        assert_eq!(file.read_at(0, &mut out_writer).unwrap(), block_size);

        assert_eq!(&out[..punch_off], &payload[..punch_off]);
        assert!(
            out[punch_off..punch_off + punch_len]
                .iter()
                .all(|byte| *byte == 0)
        );
        assert_eq!(
            &out[punch_off + punch_len..],
            &payload[punch_off + punch_len..]
        );
        assert_eq!(file.file_size(), block_size);
    }

    #[ktest]
    fn falloc_unsupported_mode() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 71, 0, 0, FileFlags::empty(), [0; 15]);

        for mode in [
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
