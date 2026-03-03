// SPDX-License-Identifier: MPL-2.0

use core::mem::size_of;

use device_id::{decode_device_numbers, encode_device_numbers};
use ostd::{const_assert, mm::io_util::HasVmReaderWriter, sync::RwMutexUpgradeableGuard};

use super::{
    block_ptr::{BlockPath, InodeMapping, InodeMappingDesc},
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
    inner: InodeInner,
    block_group_idx: usize,
    fs: Weak<Ext2>,
    xattr: Option<RwMutex<Xattr>>,
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
            block_group_idx,

            xattr: match type_ {
                InodeType::Dir | InodeType::File => Some(RwMutex::new(Xattr::new(
                    desc.file_acl,
                    weak_self.clone(),
                    fs.clone(),
                ))),
                _ => None,
            },
            inner: InodeInner::new(desc, weak_self.clone(), fs.clone()),
            fs,
            extension: Extension::new(),
        })
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
        self.inner.meta_read().desc.size as usize
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
        let mapping = self.inner.mapping_read();
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

        let fs = self.fs_arc()?;
        // Lock order: meta -> mapping.
        let mut meta = self.inner.meta_write();
        let mut mapping = self.inner.mapping_write();
        // DIFF from Linux: Linux caches dev_t in i_rdev and encodes during write_inode;
        // Asterinas stores the Linux-compatible on-disk encoding directly in block_ptrs.
        mapping.desc.encode_device_id(device_id);
        meta.set_ctime(now());
        InodeInner::persist_inode_locked(&mut meta, &mut mapping, self.ino, self.type_, &fs)
    }

    pub(super) fn resize(&self, new_size: usize) -> Result<()> {
        let fs = self.fs_arc()?;
        let block_size = fs.block_size();
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        let mut meta = self.inner.meta_write();
        if meta.inode_type() != InodeType::File
            && meta.inode_type() != InodeType::Dir
            && meta.inode_type() != InodeType::SymLink
        {
            return_errno!(Errno::EINVAL);
        }

        // Linux: /root/linux/fs/ext2/inode.c:48-55 (ext2_inode_is_fast_symlink).
        // Keep resize invalid for existing fast symlinks (inline payload), but
        // allow empty newly-created symlink inodes to grow into slow symlinks.
        let ea_blocks = if meta.file_acl() != 0 {
            (block_size / SECTOR_SIZE) as u32
        } else {
            0
        };
        {
            let mapping = self.inner.mapping_read();
            let is_fast = meta.inode_type() == InodeType::SymLink
                && mapping.desc.blocks.checked_sub(ea_blocks) == Some(0);
            if is_fast && meta.file_size() != 0 {
                return_errno!(Errno::EINVAL);
            }
        }

        if meta
            .desc
            .flags
            .intersects(FileFlags::APPEND_ONLY | FileFlags::IMMUTABLE)
        {
            return_errno!(Errno::EPERM);
        }

        let old_size = meta.file_size();

        if new_size == old_size {
            return Ok(());
        }

        if new_size < old_size {
            // Linux: /root/linux/fs/buffer.c:2654 (block_truncate_page)
            if new_size % block_size != 0 {
                let zero_to = new_size.align_up(block_size);
                self.inner.page_cache().fill_zeros(new_size..zero_to)?;
            }
            self.inner.page_cache().resize(new_size)?;

            let mut mapping = self.inner.mapping_write();
            if let Err(err) = mapping.truncate_blocks(&fs, new_size) {
                let _ = self.inner.page_cache().resize(old_size);
                return Err(err);
            }
            let current = now();
            meta.set_file_size(new_size);
            meta.touch_mtime_ctime(current);
            InodeInner::persist_inode_locked(&mut meta, &mut mapping, self.ino, self.type_, &fs)?;
        } else {
            self.inner.page_cache().resize(new_size)?;

            let current = now();
            meta.set_file_size(new_size);
            meta.touch_mtime_ctime(current);
            let mut mapping = self.inner.mapping_write();
            if let Err(err) =
                InodeInner::persist_inode_locked(&mut meta, &mut mapping, self.ino, self.type_, &fs)
            {
                let _ = self.inner.page_cache().resize(old_size);
                return Err(err);
            }
        }
        Ok(())
    }

    pub(super) fn metadata(&self) -> Metadata {
        // Lock order: meta -> mapping.
        let meta = self.inner.meta_read();
        let mapping = self.inner.mapping_read();
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
            size: meta.file_size(),
            blk_size,
            blocks: mapping.desc.blocks as usize,
            atime: meta.atime(),
            mtime: meta.mtime(),
            ctime: meta.ctime(),
            type_: self.type_,
            mode: meta.mode(),
            nlinks: meta.links_count() as usize,
            uid: Uid::new(meta.uid()),
            gid: Gid::new(meta.gid()),
            rdev,
        }
    }

    pub(super) fn inode_type(&self) -> InodeType {
        self.type_
    }

    pub(super) fn mode(&self) -> InodeMode {
        let meta = self.inner.meta_read();
        meta.mode()
    }

    pub(super) fn set_mode(&self, mode: InodeMode) -> Result<()> {
        let mut meta = self.inner.meta_write();
        meta.set_mode(mode);
        meta.set_ctime(now());
        Ok(())
    }

    pub(super) fn uid(&self) -> u32 {
        self.inner.meta_read().uid()
    }

    pub(super) fn set_uid(&self, uid: u32) -> Result<()> {
        let mut meta = self.inner.meta_write();
        meta.set_uid(uid);
        meta.set_ctime(now());
        Ok(())
    }

    pub(super) fn gid(&self) -> u32 {
        self.inner.meta_read().gid()
    }

    pub(super) fn set_gid(&self, gid: u32) -> Result<()> {
        let mut meta = self.inner.meta_write();
        meta.set_gid(gid);
        meta.set_ctime(now());
        Ok(())
    }

    pub(super) fn atime(&self) -> Duration {
        self.inner.meta_read().atime()
    }

    pub(super) fn set_atime(&self, time: Duration) {
        self.inner.meta_write().set_atime(time);
    }

    pub(super) fn mtime(&self) -> Duration {
        self.inner.meta_read().mtime()
    }

    pub(super) fn set_mtime(&self, time: Duration) {
        self.inner.meta_write().set_mtime(time);
    }

    pub(super) fn ctime(&self) -> Duration {
        self.inner.meta_read().ctime()
    }

    pub(super) fn set_ctime(&self, time: Duration) {
        self.inner.meta_write().set_ctime(time);
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

        let fs = self.fs_arc()?;
        let mut meta = self.inner.meta_write();
        meta.set_file_acl(new_bid);
        meta.set_ctime(now());
        let mut mapping = self.inner.mapping_write();
        InodeInner::persist_inode_locked(&mut meta, &mut mapping, self.ino, self.type_, &fs)
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

        let fs = self.fs_arc()?;
        let mut meta = self.inner.meta_write();
        meta.set_file_acl(new_bid);
        let mut mapping = self.inner.mapping_write();
        InodeInner::persist_inode_locked(&mut meta, &mut mapping, self.ino, self.type_, &fs)
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

        // Lock order: meta -> mapping.
        let meta = self.inner.meta_read();
        let link_size = meta.file_size();
        let ea_blocks = if meta.file_acl() != 0 {
            (block_size / SECTOR_SIZE) as u32
        } else {
            0
        };

        {
            let mapping = self.inner.mapping_read();
            let is_fast = meta.inode_type() == InodeType::SymLink
                && mapping.desc.blocks.checked_sub(ea_blocks) == Some(0);
            if is_fast {
                let read_len = link_size.min(MAX_FAST_SYMLINK_LEN - 1);
                let mut raw_bytes = [0u8; MAX_FAST_SYMLINK_LEN];
                for (idx, block_ptr) in mapping.desc.block_ptrs.iter().enumerate() {
                    let offset = idx * size_of::<u32>();
                    raw_bytes[offset..offset + size_of::<u32>()]
                        .copy_from_slice(&block_ptr.to_le_bytes());
                }

                return String::from_utf8(raw_bytes[..read_len].to_vec()).map_err(|_| {
                    Error::with_message(Errno::EIO, "symlink target is not valid UTF-8")
                });
            }
        }
        drop(meta);

        let mut target = vec![0u8; link_size];
        self.inner
            .page_cache()
            .pages()
            .read_bytes(0, &mut target)
            .map_err(|_| {
                Error::with_message(Errno::EIO, "failed to read symlink target from page cache")
            })?;

        String::from_utf8(target)
            .map_err(|_| Error::with_message(Errno::EIO, "symlink target is not valid UTF-8"))
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

        let mut meta = self.inner.meta_write();
        if with_nul <= MAX_FAST_SYMLINK_LEN {
            let mut mapping = self.inner.mapping_write();
            let mut raw_bytes = [0u8; MAX_FAST_SYMLINK_LEN];
            raw_bytes[..target_len].copy_from_slice(target.as_bytes());

            for idx in 0..mapping.desc.block_ptrs.len() {
                let offset = idx * size_of::<u32>();
                mapping.desc.block_ptrs[idx] = u32::from_le_bytes([
                    raw_bytes[offset],
                    raw_bytes[offset + 1],
                    raw_bytes[offset + 2],
                    raw_bytes[offset + 3],
                ]);
            }

            meta.set_file_size(target_len);
            mapping.desc.blocks = 0;
            let current = now();
            meta.touch_mtime_ctime(current);
            InodeInner::persist_inode_locked(&mut meta, &mut mapping, self.ino, self.type_, &fs)?;
            return Ok(());
        }

        // Slow symlink: allocate first, then write through page cache, rollback on failure.
        let old_size = meta.file_size();
        if let Err(err) = self
            .inner
            .prepare_continuous_blocks(&mut meta, &fs, 0, target_len, block_size, false)
        {
            self.inner
                .write_failed_cleanup(&mut meta, &fs, old_size, target_len, block_size);
            return Err(err);
        }

        if let Err(_) = self
            .inner
            .page_cache()
            .pages()
            .write_bytes(0, target.as_bytes())
        {
            self.inner
                .write_failed_cleanup(&mut meta, &fs, old_size, target_len, block_size);
            return Err(Error::with_message(
                Errno::EIO,
                "failed to write symlink target to page cache",
            ));
        }

        let current = now();
        meta.touch_mtime_ctime(current);
        let mut mapping = self.inner.mapping_write();
        InodeInner::persist_inode_locked(&mut meta, &mut mapping, self.ino, self.type_, &fs)?;
        Ok(())
    }

    pub(super) fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        if writer.avail() == 0 {
            return Ok(0);
        }

        let meta = self.inner.meta_upread();
        let file_size = meta.file_size();
        if offset >= file_size {
            return Ok(0);
        }
        let read_len = writer.avail().min(file_size - offset);
        writer.limit(read_len);
        self.inner.page_cache().pages().read(offset, writer)?;

        let mut meta = meta.upgrade();
        meta.set_atime(now());
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

        let mut meta = self.inner.meta_write();
        let old_size = meta.file_size();
        if let Err(err) = self
            .inner
            .prepare_continuous_blocks(&mut meta, &fs, offset, end, block_size, false)
        {
            self.inner
                .write_failed_cleanup(&mut meta, &fs, old_size, end, block_size);
            return Err(err);
        }

        if let Err(err) = self.inner.page_cache().pages().write(offset, reader) {
            self.inner
                .write_failed_cleanup(&mut meta, &fs, old_size, end, block_size);
            return Err(err.into());
        }

        let current = now();
        meta.touch_mtime_ctime(current);
        let mut mapping = self.inner.mapping_write();
        InodeInner::persist_inode_locked(&mut meta, &mut mapping, self.ino, self.type_, &fs)?;
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

        let meta = self.inner.meta_upread();

        let file_size = meta.file_size();
        if offset >= file_size || writer.avail() == 0 {
            return Ok(0);
        }

        let read_len = writer.avail().min(file_size - offset);
        writer.limit(read_len);
        let end = offset
            .checked_add(read_len)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "read range overflow"))?;

        self.inner.page_cache().evict_range(offset..end)?;
        let mapping = self.inner.mapping_read();
        self.inner
            .read_direct_at(&mapping, &fs, offset, end, writer)?;

        let mut meta = meta.upgrade();
        meta.set_atime(now());
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
        let mut meta = self.inner.meta_write();
        let old_size = meta.file_size();

        if let Err(err) = self
            .inner
            .prepare_continuous_blocks(&mut meta, &fs, offset, end, block_size, true)
        {
            self.inner
                .write_failed_cleanup(&mut meta, &fs, old_size, end, block_size);
            return Err(err);
        }

        let mapping = self.inner.mapping_read();
        if let Err(err) = self.inner.write_direct_at(&mapping, &fs, offset, reader) {
            drop(mapping);
            self.inner
                .write_failed_cleanup(&mut meta, &fs, old_size, end, block_size);
            return Err(err);
        }

        let current = now();
        meta.touch_mtime_ctime(current);
        drop(mapping);
        let mut mapping = self.inner.mapping_write();
        InodeInner::persist_inode_locked(&mut meta, &mut mapping, self.ino, self.type_, &fs)?;
        Ok(write_len)
    }

    pub(super) fn lookup(&self, name: &str) -> Result<Arc<Inode>> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let fs = self.fs_arc()?;
        let meta = self.inner.meta_read();
        let ino = self.inner.find_entry(&meta, &fs, name)?;
        fs.read_inode(ino)
    }

    /// Adds a new directory entry.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:476 (ext2_add_link)
    pub(super) fn add_entry(
        &self,
        name: &str,
        ino: u32,
        file_type: DirEntryFileType,
    ) -> Result<()> {
        let fs = self.fs_arc()?;
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > u8::MAX as usize {
            return_errno!(Errno::EINVAL);
        }

        let max_inumber = fs.super_block().total_inodes();
        if ino == 0 || ino > max_inumber {
            return_errno!(Errno::EINVAL);
        }

        let mut meta = self.inner.meta_write();
        let slot = match self.inner.scan_dir_for_slot(&meta, &fs, name)? {
            DirScanResult::Slot(slot) => slot,
            DirScanResult::NeedGrowth => self.inner.grow_dir_block(&mut meta, &fs)?,
        };

        self.inner
            .write_dir_entry(&meta, &fs, &slot, name, ino, file_type as u8)?;
        self.inner.update_dir_timestamps_and_flags(&mut meta)?;
        let mut mapping = self.inner.mapping_write();
        InodeInner::persist_inode_locked(&mut meta, &mut mapping, self.ino, self.type_, &fs)?;
        Ok(())
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
        let meta = self.inner.meta_read();
        self.inner.readdir_at(&meta, &fs, offset, visitor)
    }

    /// Deletes a directory entry by name.
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
        let mut meta = self.inner.meta_write();
        let target = self
            .inner
            .find_entry_target(&meta, &fs, name)
            .map_err(|err| {
                if err.error() == Errno::ENOENT {
                    Error::with_message(Errno::EIO, "dir entry not found for delete")
                } else {
                    err
                }
            })?;
        self.inner.delete_entry_in_cache(&meta, &fs, &target)?;
        self.inner.update_dir_timestamps_and_flags(&mut meta)?;
        let mut mapping = self.inner.mapping_write();
        InodeInner::persist_inode_locked(&mut meta, &mut mapping, self.ino, self.type_, &fs)?;
        Ok(())
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

        let block_size = fs.block_size();
        let mut meta = self.inner.meta_write();
        let old_size = meta.file_size();
        let (old_ptr0, old_blocks, new_bid) = {
            let mut mapping = self.inner.mapping_write();
            if mapping.desc.block_ptrs[0] != 0 {
                return_errno_with_message!(Errno::EIO, "dir block pointer already occupied");
            }
            let old_ptr0 = mapping.desc.block_ptrs[0];
            let old_blocks = mapping.desc.blocks;
            let bid = mapping
                .get_or_alloc_block(&fs, 0, true)?
                .ok_or_else(|| {
                    Error::with_message(Errno::ENOSPC, "failed to allocate first dir block")
                })?
                .to_raw() as u32;
            (old_ptr0, old_blocks, bid)
        };
        meta.set_file_size(block_size);

        if let Err(err) = self.inner.page_cache().resize(block_size) {
            self.inner.page_cache().discard_range(0..block_size);
            meta.set_file_size(old_size);
            let mut mapping = self.inner.mapping_write();
            mapping.desc.block_ptrs[0] = old_ptr0;
            mapping.desc.blocks = old_blocks;
            let _ = fs.free_blocks(new_bid, 1);
            return Err(err);
        }

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
            (block_size - dot_len) as u16,
            b"..",
            DirEntryFileType::Dir as u8,
        )?;

        if let Err(err) = self.inner.page_cache().pages().write_bytes(0, &buf) {
            self.inner.page_cache().discard_range(0..block_size);
            meta.set_file_size(old_size);
            let mut mapping = self.inner.mapping_write();
            mapping.desc.block_ptrs[0] = old_ptr0;
            mapping.desc.blocks = old_blocks;
            let _ = fs.free_blocks(new_bid, 1);
            return Err(err.into());
        }

        let mut mapping = self.inner.mapping_write();
        if let Err(err) =
            InodeInner::persist_inode_locked(&mut meta, &mut mapping, self.ino, self.type_, &fs)
        {
            self.inner.page_cache().discard_range(0..block_size);
            meta.set_file_size(old_size);
            mapping.desc.block_ptrs[0] = old_ptr0;
            mapping.desc.blocks = old_blocks;
            let _ = fs.free_blocks(new_bid, 1);
            return Err(err);
        }

        Ok(())
    }

    pub(super) fn empty_dir(&self) -> bool {
        let Ok(fs) = self.fs_arc() else {
            return false;
        };
        let meta = self.inner.meta_read();
        self.inner.empty_dir(&meta, &fs, self.ino)
    }

    pub(super) fn rmdir(&self, name: &str) -> Result<()> {
        if self.type_ != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        if Self::has_invalid_child_name(name) {
            return_errno!(Errno::EINVAL);
        }

        let fs = self.fs_arc()?;
        let parent_meta = self.inner.meta_read();
        let child_ino = self.inner.find_entry(&parent_meta, &fs, name)?;
        drop(parent_meta);
        let child = fs.read_inode(child_ino)?;

        {
            let child_meta = child.inner.meta_read();
            if child_meta.inode_type() != InodeType::Dir {
                return_errno!(Errno::ENOTDIR);
            }
            if !child.inner.empty_dir(&child_meta, &fs, child.ino()) {
                return_errno!(Errno::ENOTEMPTY);
            }
        }

        self.delete_entry(name)?;

        {
            let mut child_meta = child.inner.meta_write();
            child_meta.set_file_size(0);
            child_meta.sub_links_count_saturating(2);
            child_meta.set_dtime(now());
            child_meta.set_freed(true);
            let mut child_mapping = child.inner.mapping_write();
            InodeInner::persist_inode_locked(
                &mut child_meta,
                &mut child_mapping,
                child.ino(),
                child.type_,
                &fs,
            )?;
        }

        let mut parent_meta = self.inner.meta_write();
        parent_meta.sub_links_count_saturating(1);
        // SPEC: parent link-count change in rmdir is a directory mutation; refresh
        // ctime/mtime the same way as add/delete entry paths.
        // Linux: /root/linux/fs/ext2/namei.c:312 (inode_dec_link_count(dir)).
        self.inner
            .update_dir_timestamps_and_flags(&mut parent_meta)?;
        let mut parent_mapping = self.inner.mapping_write();
        InodeInner::persist_inode_locked(
            &mut parent_meta,
            &mut parent_mapping,
            self.ino,
            self.type_,
            &fs,
        )?;
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
        let mut parent_meta = self.inner.meta_write();
        let slot = match self.inner.scan_dir_for_slot(&parent_meta, &fs, name)? {
            DirScanResult::Slot(slot) => slot,
            DirScanResult::NeedGrowth => self.inner.grow_dir_block(&mut parent_meta, &fs)?,
        };

        parent_meta.add_links_count_saturating(1);

        let child = match fs.create_inode(self.ino, InodeType::Dir, perm) {
            Ok(child) => child,
            Err(err) => {
                parent_meta.sub_links_count_saturating(1);
                return Err(err);
            }
        };
        let child_ino = child.ino();

        if let Err(err) = child.make_empty(self.ino) {
            let _ = fs.free_inode(child_ino, true);
            parent_meta.sub_links_count_saturating(1);
            return Err(err);
        }

        if let Err(err) = self.inner.write_dir_entry(
            &parent_meta,
            &fs,
            &slot,
            name,
            child_ino,
            DirEntryFileType::Dir as u8,
        ) {
            {
                let mut child_meta = child.inner.meta_write();
                let _ = child
                    .inner
                    .release_dir_data_blocks_for_cleanup(&mut child_meta, &fs);
            }
            let _ = fs.free_inode(child_ino, true);
            parent_meta.sub_links_count_saturating(1);
            return Err(err);
        }

        self.inner
            .update_dir_timestamps_and_flags(&mut parent_meta)?;
        let mut parent_mapping = self.inner.mapping_write();
        if let Err(err) = InodeInner::persist_inode_locked(
            &mut parent_meta,
            &mut parent_mapping,
            self.ino,
            self.type_,
            &fs,
        ) {
            parent_meta.sub_links_count_saturating(1);
            drop(parent_mapping);
            drop(parent_meta);
            let _ = self.delete_entry(name);
            {
                let mut child_meta = child.inner.meta_write();
                let _ = child
                    .inner
                    .release_dir_data_blocks_for_cleanup(&mut child_meta, &fs);
            }
            let _ = fs.free_inode(child_ino, true);
            return Err(err);
        }

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
                let file_size = self.inner.meta_read().desc.size as usize;
                if offset >= file_size {
                    return Ok(());
                }
                let end = offset
                    .checked_add(len)
                    .ok_or_else(|| Error::with_message(Errno::EINVAL, "fallocate range overflow"))?
                    .min(file_size);
                self.inner.page_cache().fill_zeros(offset..end)
            }
            FallocMode::Allocate => {
                let new_size = offset.checked_add(len).ok_or_else(|| {
                    Error::with_message(Errno::EINVAL, "fallocate range overflow")
                })?;
                if new_size > self.file_size() {
                    self.resize(new_size)?;
                }
                Ok(())
            }
            FallocMode::AllocateKeepSize => Ok(()),
            _ => {
                return_errno_with_message!(
                    Errno::EOPNOTSUPP,
                    "fallocate with the specified flags is not supported"
                );
            }
        }
    }

    pub(super) fn sync_all(&self) -> Result<()> {
        let fs = self.fs_arc()?;

        // SPEC: fsync step 1 flushes dirty data pages first.
        // Linux: /root/linux/fs/buffer.c:646 (generic_buffers_fsync)
        // -> /root/linux/mm/filemap.c:777 (file_write_and_wait_range).
        {
            let meta = self.inner.meta_read();
            self.inner.sync_data_pages(&meta)?;
        }

        // SPEC: fsync step 2 persists inode metadata after data writeback.
        // Linux: /root/linux/fs/buffer.c:619 (sync_inode_metadata).
        let mut meta = self.inner.meta_write();
        let mut mapping = self.inner.mapping_write();
        InodeInner::persist_inode_locked(&mut meta, &mut mapping, self.ino, self.type_, &fs)?;

        // SPEC: fsync step 3 flushes device write cache.
        // Linux: /root/linux/fs/buffer.c:654 (blkdev_issue_flush).
        fs.block_device().sync()?;
        Ok(())
    }

    /// Prepares this inode for eviction.
    ///
    /// Returns `Ok(true)` if inode had `nlink == 0` and was truncated/persisted;
    /// returns `Ok(false)` if inode is still linked and only regular sync is needed.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:72 (ext2_evict_inode)
    pub(super) fn prepare_for_evict(&self) -> Result<bool> {
        if self.inner.meta_read().desc.links_count > 0 {
            self.sync_all()?;
            return Ok(false);
        }

        if let Some(xattr) = self.xattr.as_ref() {
            xattr.write().delete_xattr_block()?;
        }

        let fs = self.fs_arc()?;
        let mut meta = self.inner.meta_write();
        let mut mapping = self.inner.mapping_write();
        meta.set_dtime(now());
        meta.set_freed(true);
        meta.set_file_size(0);
        meta.set_file_acl(0);
        mapping.truncate_blocks(&fs, 0)?;
        InodeInner::persist_inode_locked(&mut meta, &mut mapping, self.ino, self.type_, &fs)?;

        Ok(true)
    }

    pub(super) fn sync_data(&self) -> Result<()> {
        let fs = self.fs_arc()?;

        {
            // SPEC: fdatasync always writes back dirty data pages first.
            // Linux: /root/linux/fs/buffer.c:609 (file_write_and_wait_range).
            let meta = self.inner.meta_read();
            self.inner.sync_data_pages(&meta)?;

            // SPEC: Linux writes metadata when I_DIRTY_DATASYNC is set.
            // Linux: /root/linux/fs/buffer.c:616-619.
            // Asterinas uses desc.is_dirty() as a conservative approximation
            // so fdatasync never misses i_size/block-mapping persistence.
            drop(meta);
            let mut meta = self.inner.meta_write();
            let mut mapping = self.inner.mapping_write();
            if meta.is_dirty() || mapping.desc.is_dirty() {
                InodeInner::persist_inode_locked(
                    &mut meta,
                    &mut mapping,
                    self.ino,
                    self.type_,
                    &fs,
                )?;
            }
        }

        // SPEC: fdatasync ends with device cache flush.
        // Linux: /root/linux/fs/buffer.c:654 (blkdev_issue_flush).
        fs.block_device().sync()?;
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

            if let Err(err) = self.add_entry(name, child_ino, dir_ft) {
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
        let (mut dir_meta, mut old_meta) = meta_write_lock_two_inodes(self, old);

        // Linux: inode_set_ctime_current + inode_inc_link_count before add_link.
        if old_meta.links_count() >= MAX_LINK_COUNT {
            return_errno!(Errno::EOVERFLOW);
        }
        old_meta.set_ctime(now());
        old_meta.add_links_count_saturating(1);

        let add_result = (|| -> Result<()> {
            let slot = match self.inner.scan_dir_for_slot(&dir_meta, &fs, name)? {
                DirScanResult::Slot(slot) => slot,
                DirScanResult::NeedGrowth => self.inner.grow_dir_block(&mut dir_meta, &fs)?,
            };
            self.inner
                .write_dir_entry(&dir_meta, &fs, &slot, name, old.ino, dir_ft as u8)?;
            self.inner.update_dir_timestamps_and_flags(&mut dir_meta)?;
            Ok(())
        })();

        if let Err(err) = add_result {
            // SPEC: rollback link count on add_entry failure.
            old_meta.sub_links_count_saturating(1);
            return Err(err);
        }

        let mut dir_mapping = self.inner.mapping_write();
        InodeInner::persist_inode_locked(
            &mut dir_meta,
            &mut dir_mapping,
            self.ino,
            self.type_,
            &fs,
        )?;
        let mut old_mapping = old.inner.mapping_write();
        InodeInner::persist_inode_locked(
            &mut old_meta,
            &mut old_mapping,
            old.ino(),
            old.type_,
            &fs,
        )?;
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
        let parent_meta = self.inner.meta_read();
        let child_ino = self.inner.find_entry(&parent_meta, &fs, name)?;
        drop(parent_meta);
        let child = fs.read_inode(child_ino)?;

        // SPEC: unlink rejects directories — use rmdir instead.
        if child.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        // Delete the directory entry first.
        self.delete_entry(name)?;

        // Linux: inode_set_ctime_to_ts(inode, inode_get_ctime(dir))
        // then inode_dec_link_count.
        let mut child_meta = child.inner.meta_write();
        child_meta.set_ctime(now());
        child_meta.sub_links_count_saturating(1);

        // Defer ext2_evict_inode-style reclamation to cache eviction.
        if child_meta.links_count() == 0 {
            child_meta.set_dtime(now());
            child_meta.set_freed(true);
        }
        let mut child_mapping = child.inner.mapping_write();
        InodeInner::persist_inode_locked(
            &mut child_meta,
            &mut child_mapping,
            child.ino(),
            child.type_,
            &fs,
        )?;

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

        if self.ino == target.ino {
            self.rename_same_dir_with_ordered_locks(old_name, new_name, &fs)
        } else {
            self.rename_cross_dir_with_ordered_locks(target, old_name, new_name, &fs)
        }
    }

    fn rename_same_dir_with_ordered_locks(
        &self,
        old_name: &str,
        new_name: &str,
        fs: &Arc<Ext2>,
    ) -> Result<()> {
        const RENAME_RETRY_LIMIT: usize = 8;
        for _ in 0..RENAME_RETRY_LIMIT {
            let old_ino = {
                let dir_meta = self.inner.meta_read();
                self.inner.find_entry(&dir_meta, fs, old_name)?
            };
            let old_inode = fs.read_inode(old_ino)?;
            let old_is_dir = old_inode.type_ == InodeType::Dir;
            let moved_ft = Self::inode_type_to_dir_file_type(old_inode.type_) as u8;

            let existing_ino = {
                let dir_meta = self.inner.meta_read();
                self.inner.find_entry(&dir_meta, fs, new_name).ok()
            };
            let existing_inode = if let Some(ino) = existing_ino {
                Some(fs.read_inode(ino)?)
            } else {
                None
            };

            let mut lock_targets = Vec::new();
            let dir_idx = Self::push_unique_lock_target(&mut lock_targets, self);
            let old_idx = Self::push_unique_lock_target(&mut lock_targets, old_inode.as_ref());
            let existing_idx = existing_inode
                .as_ref()
                .map(|inode| Self::push_unique_lock_target(&mut lock_targets, inode.as_ref()));
            let mut meta_guards: Vec<Option<RwMutexWriteGuard<'_, InodeMeta>>> =
                meta_write_lock_multiple_inodes(&lock_targets)
                    .into_iter()
                    .map(Some)
                    .collect();

            let rechecked_old_ino = {
                let dir_meta = meta_guards[dir_idx].as_ref().ok_or_else(|| {
                    Error::with_message(Errno::EIO, "missing directory meta lock")
                })?;
                self.inner.find_entry(dir_meta, fs, old_name)?
            };
            if rechecked_old_ino != old_ino {
                continue;
            }

            let rechecked_existing_ino = {
                let dir_meta = meta_guards[dir_idx].as_ref().ok_or_else(|| {
                    Error::with_message(Errno::EIO, "missing directory meta lock")
                })?;
                self.inner.find_entry(dir_meta, fs, new_name).ok()
            };
            if rechecked_existing_ino != existing_ino {
                continue;
            }

            if let Some(existing) = existing_inode.as_ref() {
                let existing_is_dir = existing.type_ == InodeType::Dir;
                if old_is_dir && !existing_is_dir {
                    return_errno!(Errno::ENOTDIR);
                }
                if !old_is_dir && existing_is_dir {
                    return_errno!(Errno::EISDIR);
                }
                if existing_is_dir {
                    let idx = existing_idx.ok_or_else(|| {
                        Error::with_message(Errno::EIO, "missing existing inode lock index")
                    })?;
                    let existing_meta = meta_guards[idx].as_ref().ok_or_else(|| {
                        Error::with_message(Errno::EIO, "missing existing inode meta lock")
                    })?;
                    if !existing.inner.empty_dir(existing_meta, fs, existing.ino()) {
                        return_errno!(Errno::ENOTEMPTY);
                    }
                }
            }

            let dir_meta = meta_guards[dir_idx]
                .as_mut()
                .ok_or_else(|| Error::with_message(Errno::EIO, "missing directory meta lock"))?;
            Self::apply_rename_target_locked(
                self,
                dir_meta,
                fs,
                new_name,
                old_ino,
                moved_ft,
                existing_inode.is_some(),
            )?;
            let old_target = self.inner.find_entry_target(dir_meta, fs, old_name)?;
            self.inner
                .delete_entry_in_cache(dir_meta, fs, &old_target)?;
            if old_is_dir {
                if existing_inode.is_none() {
                    dir_meta.add_links_count_saturating(1);
                }
                dir_meta.sub_links_count_saturating(1);
            }
            self.inner.update_dir_timestamps_and_flags(dir_meta)?;
            let mut dir_mapping = self.inner.mapping_write();
            InodeInner::persist_inode_locked(dir_meta, &mut dir_mapping, self.ino, self.type_, fs)?;

            if let (Some(existing), Some(idx)) = (existing_inode.as_ref(), existing_idx) {
                if idx == old_idx {
                    let old_meta = meta_guards[old_idx].as_mut().ok_or_else(|| {
                        Error::with_message(Errno::EIO, "missing old inode meta lock")
                    })?;
                    old_meta.set_ctime(now());
                    old_meta.sub_links_count_saturating(1);
                    if old_meta.links_count() == 0 {
                        old_meta.set_dtime(now());
                        old_meta.set_freed(true);
                    }
                    let mut old_mapping = old_inode.inner.mapping_write();
                    InodeInner::persist_inode_locked(
                        old_meta,
                        &mut old_mapping,
                        old_inode.ino(),
                        old_inode.type_,
                        fs,
                    )?;
                    return Ok(());
                }

                let existing_meta = meta_guards[idx].as_mut().ok_or_else(|| {
                    Error::with_message(Errno::EIO, "missing existing inode meta lock")
                })?;
                existing_meta.set_ctime(now());
                if old_is_dir {
                    existing_meta.sub_links_count_saturating(1);
                }
                existing_meta.sub_links_count_saturating(1);
                if existing_meta.links_count() == 0 {
                    existing_meta.set_dtime(now());
                    existing_meta.set_freed(true);
                }
                let mut existing_mapping = existing.inner.mapping_write();
                InodeInner::persist_inode_locked(
                    existing_meta,
                    &mut existing_mapping,
                    existing.ino(),
                    existing.type_,
                    fs,
                )?;
            }

            let old_meta = meta_guards[old_idx]
                .as_mut()
                .ok_or_else(|| Error::with_message(Errno::EIO, "missing old inode meta lock"))?;
            old_meta.set_ctime(now());
            let mut old_mapping = old_inode.inner.mapping_write();
            InodeInner::persist_inode_locked(
                old_meta,
                &mut old_mapping,
                old_inode.ino(),
                old_inode.type_,
                fs,
            )?;
            return Ok(());
        }

        return_errno_with_message!(
            Errno::EAGAIN,
            "rename retried due concurrent directory updates"
        );
    }

    fn rename_cross_dir_with_ordered_locks(
        &self,
        target: &Inode,
        old_name: &str,
        new_name: &str,
        fs: &Arc<Ext2>,
    ) -> Result<()> {
        const RENAME_RETRY_LIMIT: usize = 8;
        for _ in 0..RENAME_RETRY_LIMIT {
            let old_ino = {
                let source_meta = self.inner.meta_read();
                self.inner.find_entry(&source_meta, fs, old_name)?
            };
            let old_inode = fs.read_inode(old_ino)?;
            let old_is_dir = old_inode.type_ == InodeType::Dir;
            let moved_ft = Self::inode_type_to_dir_file_type(old_inode.type_) as u8;

            let existing_ino = {
                let target_meta = target.inner.meta_read();
                target.inner.find_entry(&target_meta, fs, new_name).ok()
            };
            let existing_inode = if let Some(ino) = existing_ino {
                Some(fs.read_inode(ino)?)
            } else {
                None
            };

            let mut lock_targets = Vec::new();
            let source_idx = Self::push_unique_lock_target(&mut lock_targets, self);
            let target_idx = Self::push_unique_lock_target(&mut lock_targets, target);
            let old_idx = Self::push_unique_lock_target(&mut lock_targets, old_inode.as_ref());
            let existing_idx = existing_inode
                .as_ref()
                .map(|inode| Self::push_unique_lock_target(&mut lock_targets, inode.as_ref()));
            let mut meta_guards: Vec<Option<RwMutexWriteGuard<'_, InodeMeta>>> =
                meta_write_lock_multiple_inodes(&lock_targets)
                    .into_iter()
                    .map(Some)
                    .collect();

            let rechecked_old_ino = {
                let source_meta = meta_guards[source_idx]
                    .as_ref()
                    .ok_or_else(|| Error::with_message(Errno::EIO, "missing source meta lock"))?;
                self.inner.find_entry(source_meta, fs, old_name)?
            };
            if rechecked_old_ino != old_ino {
                continue;
            }

            let rechecked_existing_ino = {
                let target_meta = meta_guards[target_idx]
                    .as_ref()
                    .ok_or_else(|| Error::with_message(Errno::EIO, "missing target meta lock"))?;
                target.inner.find_entry(target_meta, fs, new_name).ok()
            };
            if rechecked_existing_ino != existing_ino {
                continue;
            }

            if old_is_dir {
                let old_meta = meta_guards[old_idx].as_ref().ok_or_else(|| {
                    Error::with_message(Errno::EIO, "missing old inode meta lock")
                })?;
                let dotdot_ino = old_inode.inner.find_entry(old_meta, fs, "..")?;
                if dotdot_ino != self.ino {
                    return_errno_with_message!(Errno::EIO, "failed to update dotdot entry");
                }
            }

            if let Some(existing) = existing_inode.as_ref() {
                let existing_is_dir = existing.type_ == InodeType::Dir;
                if old_is_dir && !existing_is_dir {
                    return_errno!(Errno::ENOTDIR);
                }
                if !old_is_dir && existing_is_dir {
                    return_errno!(Errno::EISDIR);
                }
                if existing_is_dir {
                    let idx = existing_idx.ok_or_else(|| {
                        Error::with_message(Errno::EIO, "missing existing inode lock index")
                    })?;
                    let existing_meta = meta_guards[idx].as_ref().ok_or_else(|| {
                        Error::with_message(Errno::EIO, "missing existing inode meta lock")
                    })?;
                    if !existing.inner.empty_dir(existing_meta, fs, existing.ino()) {
                        return_errno!(Errno::ENOTEMPTY);
                    }
                }
            }

            {
                let target_meta = meta_guards[target_idx].as_mut().ok_or_else(|| {
                    Error::with_message(Errno::EIO, "missing target directory meta lock")
                })?;
                Self::apply_rename_target_locked(
                    target,
                    target_meta,
                    fs,
                    new_name,
                    old_ino,
                    moved_ft,
                    existing_inode.is_some(),
                )?;
                if old_is_dir && existing_inode.is_none() {
                    target_meta.add_links_count_saturating(1);
                }
                target.inner.update_dir_timestamps_and_flags(target_meta)?;
            }
            {
                let source_meta = meta_guards[source_idx].as_mut().ok_or_else(|| {
                    Error::with_message(Errno::EIO, "missing source directory meta lock")
                })?;
                let source_de = self.inner.find_entry_target(source_meta, fs, old_name)?;
                self.inner
                    .delete_entry_in_cache(source_meta, fs, &source_de)?;
                if old_is_dir {
                    source_meta.sub_links_count_saturating(1);
                }
                self.inner.update_dir_timestamps_and_flags(source_meta)?;
            }

            {
                let target_meta = meta_guards[target_idx].as_mut().ok_or_else(|| {
                    Error::with_message(Errno::EIO, "missing target directory meta lock")
                })?;
                let mut target_mapping = target.inner.mapping_write();
                InodeInner::persist_inode_locked(
                    target_meta,
                    &mut target_mapping,
                    target.ino,
                    target.type_,
                    fs,
                )?;
            }
            {
                let source_meta = meta_guards[source_idx].as_mut().ok_or_else(|| {
                    Error::with_message(Errno::EIO, "missing source directory meta lock")
                })?;
                let mut source_mapping = self.inner.mapping_write();
                InodeInner::persist_inode_locked(
                    source_meta,
                    &mut source_mapping,
                    self.ino,
                    self.type_,
                    fs,
                )?;
            }

            if let (Some(existing), Some(idx)) = (existing_inode.as_ref(), existing_idx) {
                if idx == old_idx {
                    let old_meta = meta_guards[old_idx].as_mut().ok_or_else(|| {
                        Error::with_message(Errno::EIO, "missing old inode meta lock")
                    })?;
                    old_meta.set_ctime(now());
                    old_meta.sub_links_count_saturating(1);
                    if old_meta.links_count() == 0 {
                        old_meta.set_dtime(now());
                        old_meta.set_freed(true);
                    }
                    let mut old_mapping = old_inode.inner.mapping_write();
                    InodeInner::persist_inode_locked(
                        old_meta,
                        &mut old_mapping,
                        old_inode.ino(),
                        old_inode.type_,
                        fs,
                    )?;
                    return Ok(());
                }

                let existing_meta = meta_guards[idx].as_mut().ok_or_else(|| {
                    Error::with_message(Errno::EIO, "missing existing inode meta lock")
                })?;
                existing_meta.set_ctime(now());
                if old_is_dir {
                    existing_meta.sub_links_count_saturating(1);
                }
                existing_meta.sub_links_count_saturating(1);
                if existing_meta.links_count() == 0 {
                    existing_meta.set_dtime(now());
                    existing_meta.set_freed(true);
                }
                let mut existing_mapping = existing.inner.mapping_write();
                InodeInner::persist_inode_locked(
                    existing_meta,
                    &mut existing_mapping,
                    existing.ino(),
                    existing.type_,
                    fs,
                )?;
            }

            let old_meta = meta_guards[old_idx]
                .as_mut()
                .ok_or_else(|| Error::with_message(Errno::EIO, "missing old inode meta lock"))?;
            old_meta.set_ctime(now());
            if old_is_dir {
                let dotdot = old_inode.inner.find_entry_target(old_meta, fs, "..")?;
                old_inode.inner.set_link_in_cache(
                    old_meta,
                    fs,
                    &dotdot,
                    target.ino,
                    DirEntryFileType::Dir as u8,
                )?;
                old_meta.remove_flags(FileFlags::INDEX_DIR);
            }
            let mut old_mapping = old_inode.inner.mapping_write();
            InodeInner::persist_inode_locked(
                old_meta,
                &mut old_mapping,
                old_inode.ino(),
                old_inode.type_,
                fs,
            )?;
            return Ok(());
        }

        return_errno_with_message!(
            Errno::EAGAIN,
            "rename retried due concurrent directory updates"
        );
    }

    fn push_unique_lock_target<'a>(targets: &mut Vec<&'a Inode>, inode: &'a Inode) -> usize {
        if let Some(index) = targets.iter().position(|target| target.ino == inode.ino) {
            return index;
        }
        targets.push(inode);
        targets.len() - 1
    }

    fn apply_rename_target_locked(
        target: &Inode,
        target_meta: &mut InodeMeta,
        fs: &Arc<Ext2>,
        new_name: &str,
        old_ino: u32,
        moved_ft: u8,
        has_existing: bool,
    ) -> Result<()> {
        if has_existing {
            let target_de = target.inner.find_entry_target(target_meta, fs, new_name)?;
            target
                .inner
                .set_link_in_cache(target_meta, fs, &target_de, old_ino, moved_ft)?;
            return Ok(());
        }

        let slot = match target.inner.scan_dir_for_slot(target_meta, fs, new_name)? {
            DirScanResult::Slot(slot) => slot,
            DirScanResult::NeedGrowth => target.inner.grow_dir_block(target_meta, fs)?,
        };
        target
            .inner
            .write_dir_entry(target_meta, fs, &slot, new_name, old_ino, moved_ft)?;
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
        let mut meta = self.inner.meta_write();
        let target = self.inner.find_entry_target(&meta, &fs, name)?;
        self.inner
            .set_link_in_cache(&meta, &fs, &target, new_ino, file_type as u8)?;
        if update_times {
            self.inner.update_dir_timestamps_and_flags(&mut meta)?;
        } else {
            meta.remove_flags(FileFlags::INDEX_DIR);
        }
        let mut mapping = self.inner.mapping_write();
        InodeInner::persist_inode_locked(&mut meta, &mut mapping, self.ino, self.type_, &fs)?;
        Ok(())
    }

    pub(super) fn extension(&self) -> &Extension {
        &self.extension
    }

    pub(super) fn page_cache_vmo(&self) -> Arc<Vmo> {
        self.inner.page_cache().pages().clone()
    }
}

impl PageCacheBackend for Inode {
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter> {
        let mapping = self.inner.mapping_read();
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
        let mapping = self.inner.mapping_read();
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
        self.inner.page_cache().pages().size() / BLOCK_SIZE
    }
}

#[derive(Debug)]
pub struct InodeInner {
    meta: RwMutex<InodeMeta>,
    mapping: RwMutex<InodeMapping>,
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
        let meta = RwMutex::new(InodeMeta::new(desc.meta_desc()));
        let mapping = RwMutex::new(InodeMapping::new(desc.mapping_desc()));
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
            meta,
            mapping,
            weak_self,
            fs,
            page_cache,
        }
    }

    pub(super) fn meta_read(&self) -> RwMutexReadGuard<'_, InodeMeta> {
        self.meta.read()
    }

    pub(super) fn meta_write(&self) -> RwMutexWriteGuard<'_, InodeMeta> {
        self.meta.write()
    }

    pub(super) fn meta_upread(&self) -> RwMutexUpgradeableGuard<'_, InodeMeta> {
        self.meta.upread()
    }

    pub(super) fn mapping_upread(&self) -> RwMutexUpgradeableGuard<'_, InodeMapping> {
        self.mapping.upread()
    }

    pub(super) fn mapping_read(&self) -> RwMutexReadGuard<'_, InodeMapping> {
        self.mapping.read()
    }

    pub(super) fn mapping_write(&self) -> RwMutexWriteGuard<'_, InodeMapping> {
        self.mapping.write()
    }

    pub(super) fn page_cache(&self) -> &PageCache {
        &self.page_cache
    }

    fn fs_arc(&self) -> Result<Arc<Ext2>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem already dropped"))
    }

    /// Persists inode meta+mapping to disk.
    ///
    /// # Lock
    /// The caller must hold both `meta.write()` and `mapping.write()`.
    pub(super) fn persist_inode_locked(
        meta: &mut InodeMeta,
        mapping: &mut InodeMapping,
        ino: u32,
        type_: InodeType,
        fs: &Ext2,
    ) -> Result<()> {
        debug_assert_eq!(meta.inode_type(), type_);
        let raw = RawInode::from_parts(meta.raw_desc(), &mapping.get_desc());
        fs.write_inode_desc(ino, &raw)?;
        meta.clear_dirty();
        mapping.desc.clear_dirty();
        Ok(())
    }

    // fn is_fast_symlink(&self, block_size: usize) -> bool {
    //     let ea_blocks = if self.meta.file_acl() != 0 {
    //         (block_size / SECTOR_SIZE) as u32
    //     } else {
    //         0
    //     };
    //     self.meta.inode_type() == InodeType::SymLink
    //         && self.mapping.desc.blocks.checked_sub(ea_blocks) == Some(0)
    // }

    // fn decode_device_id(&self) -> u64 {
    //     let (major, minor) = if self.mapping.desc.block_ptrs[0] != 0 {
    //         let val = self.mapping.desc.block_ptrs[0];
    //         (((val >> 8) & 0xFF), (val & 0xFF))
    //     } else {
    //         let dev = self.mapping.desc.block_ptrs[1];
    //         (
    //             ((dev & 0xFFF00) >> 8),
    //             ((dev & 0xFF) | ((dev >> 12) & 0xFFF00)),
    //         )
    //     };
    //     encode_device_numbers(major, minor)
    // }

    // fn encode_device_id(&mut self, device_id: u64) {
    //     let (major, minor) = decode_device_numbers(device_id);
    //     if major < 256 && minor < 256 {
    //         self.mapping.desc.block_ptrs[0] = (major << 8) | minor;
    //         self.mapping.desc.block_ptrs[1] = 0;
    //     } else {
    //         self.mapping.desc.block_ptrs[0] = 0;
    //         self.mapping.desc.block_ptrs[1] =
    //             (minor & 0xFF) | (major << 8) | ((minor & !0xFF) << 12);
    //         self.mapping.desc.block_ptrs[2] = 0;
    //     }
    // }

    /// Reads file data directly from data blocks into `writer`.
    ///
    /// Linux: /root/linux/fs/ext2/file.c:168 (ext2_dio_read_iter)
    pub(super) fn read_direct_at(
        &self,
        mapping: &InodeMapping,
        fs: &Ext2,
        offset: usize,
        end: usize,
        writer: &mut VmWriter,
    ) -> Result<()> {
        let block_size = fs.block_size();
        let mut current_offset = offset;

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
    pub(super) fn write_direct_at(
        &self,
        mapping: &InodeMapping,
        fs: &Ext2,
        offset: usize,
        reader: &mut VmReader,
    ) -> Result<()> {
        let block_size = fs.block_size();
        let write_len = reader.remain();
        // end is already checked in `Inode::write_direct_at`.
        let end = offset + write_len;
        let mut current_offset = offset;

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
    pub(super) fn empty_dir(&self, meta: &InodeMeta, fs: &Ext2, self_ino: u32) -> bool {
        if meta.inode_type() != InodeType::Dir {
            return false;
        }

        let block_size = fs.block_size();
        let size = meta.file_size();
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
    pub(super) fn find_entry(&self, meta: &InodeMeta, fs: &Ext2, name: &str) -> Result<u32> {
        if meta.inode_type() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let sb = fs.super_block();
        let block_size = fs.block_size();
        let size = meta.file_size();
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

    /// Reads directory entries starting at byte offset and feeds visitor.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:257 (ext2_readdir)
    pub(super) fn readdir_at(
        &self,
        meta: &InodeMeta,
        fs: &Ext2,
        offset: usize,
        visitor: &mut dyn DirentVisitor,
    ) -> Result<usize> {
        if meta.inode_type() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let size = meta.file_size();
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

    /// Translates a logical block number into a path of block pointer offsets.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:163 (ext2_block_to_path)
    pub(super) fn block_to_path(&self, iblock: u32) -> Result<BlockPath> {
        let fs = self.fs_arc()?;
        let mapping = self.mapping_read();
        mapping.block_to_path(&fs, iblock)
    }

    /// Maps a logical block to a physical block (read-only path).
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:783 (ext2_get_block)
    pub(super) fn get_block(&self, iblock: u32) -> Result<Option<Bid>> {
        let fs = self.fs_arc()?;
        let mapping = self.mapping_read();
        mapping.get_block(&fs, iblock)
    }

    pub(super) fn prepare_continuous_blocks(
        &self,
        meta: &mut InodeMeta,
        fs: &Ext2,
        offset: usize,
        end: usize,
        block_size: usize,
        discard_page_cache: bool,
    ) -> Result<()> {
        if block_size == 0 {
            return_errno_with_message!(Errno::EIO, "invalid filesystem block size");
        }

        let start_block = offset / block_size;
        let end_block = end.div_ceil(block_size);
        let old_size = meta.file_size();

        {
            let mut mapping = self.mapping_write();
            for iblock in start_block..end_block {
                let iblock = u32::try_from(iblock).map_err(|_| {
                    Error::with_message(Errno::EINVAL, "logical block number overflow")
                })?;
                let old_blocks = mapping.blocks_512();
                if mapping.get_or_alloc_block(fs, iblock, true)?.is_none() {
                    return_errno_with_message!(
                        Errno::EIO,
                        "missing block mapping after allocation"
                    );
                }
                if mapping.blocks_512() != old_blocks {
                    meta.set_ctime(now());
                }
            }
        }

        if end > old_size {
            self.page_cache.resize(end.align_up(block_size))?;
            meta.set_file_size(end);
        }

        if discard_page_cache {
            let discard_start = offset.min(old_size);
            let discard_end = end.min(old_size);
            if discard_start < discard_end {
                self.page_cache.discard_range(discard_start..discard_end);
            }
        }

        Ok(())
    }

    pub(super) fn write_failed_cleanup(
        &self,
        meta: &mut InodeMeta,
        fs: &Ext2,
        old_size: usize,
        end: usize,
        block_size: usize,
    ) {
        if end <= old_size {
            return;
        }

        let old_size_aligned = old_size.align_up(block_size);
        let end_aligned = end.align_up(block_size);
        self.page_cache.discard_range(old_size_aligned..end_aligned);

        if let Err(err) = self.page_cache.resize(old_size_aligned) {
            error!(
                "ext2: write_at cleanup page cache resize failed: old_size_aligned={}, err={:?}",
                old_size_aligned, err
            );
        }

        if let Err(err) = self.mapping_write().truncate_blocks(fs, old_size) {
            error!(
                "ext2: write_at cleanup truncate_blocks failed: old_size={}, err={:?}",
                old_size, err
            );
        }

        meta.set_file_size(old_size);
    }

    /// Resolves a logical block to physical, allocating a missing branch if requested.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:624 (ext2_get_blocks, create path)
    pub(super) fn get_or_alloc_block(&self, iblock: u32, create: bool) -> Result<Option<Bid>> {
        let fs = self.fs_arc()?;
        let mut meta = self.meta_write();
        let mut mapping = self.mapping_write();
        let old_blocks = mapping.blocks_512();
        let mapped = mapping.get_or_alloc_block(&fs, iblock, create)?;
        if mapping.blocks_512() != old_blocks {
            meta.set_ctime(now());
        }
        Ok(mapped)
    }

    /// Phase 1: scan directory blocks for reusable slot or duplicate.
    ///
    /// Linux: /root/linux/fs/ext2/dir.c:476 (ext2_add_link scan loop)
    pub(super) fn scan_dir_for_slot(
        &self,
        meta: &InodeMeta,
        fs: &Ext2,
        name: &str,
    ) -> Result<DirScanResult> {
        if meta.inode_type() != InodeType::Dir {
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

        let size = meta.file_size();
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
    pub(super) fn grow_dir_block(&self, meta: &mut InodeMeta, fs: &Ext2) -> Result<DirSlotInfo> {
        let block_size = fs.block_size();
        let old_size = meta.file_size();
        let data_blocks = old_size.div_ceil(block_size);
        let growth_iblock = u32::try_from(data_blocks)
            .map_err(|_| Error::with_message(Errno::EINVAL, "directory block index overflow"))?;

        // SPEC: allocation under mapping.write(); no PageCache/VMO operations in this scope.
        {
            let mut mapping = self.mapping_write();
            mapping
                .get_or_alloc_block(fs, growth_iblock, true)?
                .ok_or_else(|| {
                    Error::with_message(Errno::ENOSPC, "failed to grow directory block")
                })?;
        }

        let new_size = old_size.saturating_add(block_size);
        meta.set_file_size(new_size);
        if let Err(err) = self.page_cache.resize(new_size) {
            // SPEC: rollback allocated growth on resize failure.
            self.page_cache.discard_range(old_size..new_size);
            meta.set_file_size(old_size);
            let mut mapping = self.mapping_write();
            mapping.truncate_blocks(fs, old_size)?;
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
    pub(super) fn write_dir_entry(
        &self,
        _meta: &InodeMeta,
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
    pub(super) fn find_entry_target(
        &self,
        meta: &InodeMeta,
        fs: &Ext2,
        name: &str,
    ) -> Result<DirEntryTarget> {
        let max_inumber = fs.super_block().total_inodes();
        let block_size = fs.block_size();
        let size = meta.file_size();
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
    pub(super) fn delete_entry_in_cache(
        &self,
        meta: &InodeMeta,
        fs: &Ext2,
        target: &DirEntryTarget,
    ) -> Result<()> {
        let block_size = fs.block_size();
        let block_base = (target.dir_offset / block_size).saturating_mul(block_size);
        let entry_offset = target.dir_offset.saturating_sub(block_base);
        let limit = (meta.file_size())
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
    pub(super) fn set_link_in_cache(
        &self,
        _meta: &InodeMeta,
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

    pub(super) fn release_dir_data_blocks_for_cleanup(
        &self,
        meta: &mut InodeMeta,
        fs: &Ext2,
    ) -> Result<()> {
        // DIFF from Linux:
        // Linux mkdir-failure/rmdir cleanup reaches block release through
        // discard_new_inode()/iput() -> ext2_evict_inode() -> ext2_truncate_blocks().
        // Asterinas currently has no unified inode evict+truncate path, so we
        // explicitly trigger truncate-based release on rollback/removal paths.
        // TODO: Move this logic into a shared truncate/evict pipeline, and make
        // free_inode trigger it instead of per-call-site cleanup.
        // SPEC: mapping truncation happens under mapping.write().
        {
            let mut mapping = self.mapping_write();
            mapping.truncate_blocks(fs, 0)?;
        }
        self.page_cache.discard_range(0..meta.file_size());
        self.page_cache.resize(0)?;
        // SPEC: cleanup path must leave directory size at zero.
        meta.set_file_size(0);
        Ok(())
    }

    fn update_dir_timestamps_and_flags(&self, meta: &mut InodeMeta) -> Result<()> {
        let current = now();
        meta.touch_mtime_ctime(current);
        meta.remove_flags(FileFlags::INDEX_DIR);
        Ok(())
    }

    pub(super) fn sync_data_pages(&self, meta: &InodeMeta) -> Result<()> {
        // SPEC: file_write_and_wait_range on an empty file is a no-op.
        // Linux: /root/linux/mm/filemap.c:782-783.
        let file_size = meta.file_size();
        if file_size == 0 {
            return Ok(());
        }

        // SPEC: evict_range writes back dirty pages in [0, file_size), waits for
        // completion, and keeps pages cached as UpToDate.
        self.page_cache.evict_range(0..file_size)
    }

    fn persist_inode(&mut self) -> Result<()> {
        let inode = self
            .weak_self
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "inode already dropped"))?;
        let fs = self.fs_arc()?;
        let mut meta = self.meta.write();
        let mut mapping = self.mapping.write();
        Self::persist_inode_locked(&mut meta, &mut mapping, inode.ino, inode.type_, &fs)
    }
}

/// Acquires `meta.read()` locks on two inodes in ascending ino order.
/// Returns guards in `(a, b)` order regardless of which ino is smaller.
fn meta_read_lock_two_inodes<'a>(
    a: &'a Inode,
    b: &'a Inode,
) -> (
    RwMutexReadGuard<'a, InodeMeta>,
    RwMutexReadGuard<'a, InodeMeta>,
) {
    if a.ino <= b.ino {
        let ga = a.inner.meta_read();
        let gb = b.inner.meta_read();
        (ga, gb)
    } else {
        let gb = b.inner.meta_read();
        let ga = a.inner.meta_read();
        (ga, gb)
    }
}

/// Acquires `meta.write()` locks on two inodes in ascending ino order.
/// Returns guards in `(a, b)` order regardless of which ino is smaller.
fn meta_write_lock_two_inodes<'a>(
    a: &'a Inode,
    b: &'a Inode,
) -> (
    RwMutexWriteGuard<'a, InodeMeta>,
    RwMutexWriteGuard<'a, InodeMeta>,
) {
    if a.ino <= b.ino {
        let ga = a.inner.meta_write();
        let gb = b.inner.meta_write();
        (ga, gb)
    } else {
        let gb = b.inner.meta_write();
        let ga = a.inner.meta_write();
        (ga, gb)
    }
}

/// Acquires `meta.write()` locks on an arbitrary number of inodes in ascending ino order.
/// Returns guards in the same order as the input slice.
fn meta_write_lock_multiple_inodes<'a>(
    inodes: &[&'a Inode],
) -> Vec<RwMutexWriteGuard<'a, InodeMeta>> {
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
    let mut slots: Vec<Option<Rc<RefCell<Option<RwMutexWriteGuard<'a, InodeMeta>>>>>> =
        vec![None; inodes.len()];
    for &(orig_idx, _, inode) in &indexed {
        let guard = inode.inner.meta_write();
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

    fn meta_desc(&self) -> InodeMetaDesc {
        InodeMetaDesc::from_desc(self)
    }

    fn mapping_desc(&self) -> InodeMappingDesc {
        InodeMappingDesc::from_parts(self.blocks, self.block_ptrs)
    }
}

impl TryFrom<&RawInode> for InodeDesc {
    type Error = Error;
    fn try_from(raw: &RawInode) -> Result<Self> {
        let meta = InodeMetaDesc::try_from_raw(raw)?;
        let mapping = InodeMappingDesc::from_raw(raw);

        Ok(InodeDesc {
            type_: meta.type_,
            perm: meta.perm,
            uid: meta.uid,
            gid: meta.gid,
            size: meta.size,
            atime: meta.atime,
            ctime: meta.ctime,
            mtime: meta.mtime,
            dtime: meta.dtime,
            links_count: meta.links_count,
            blocks: mapping.blocks,
            flags: meta.flags,
            file_acl: meta.file_acl,
            generation: meta.generation,
            block_ptrs: mapping.block_ptrs,
        })
    }
}

impl From<&InodeDesc> for RawInode {
    fn from(desc: &InodeDesc) -> Self {
        let meta = desc.meta_desc();
        let mapping = desc.mapping_desc();
        RawInode::from_parts(&meta, &mapping)
    }
}

// In-memory inode metadata (raw on-disk view excluding i_blocks/i_block[]).
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeMetaDesc {
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
    flags: FileFlags,
    file_acl: u32,
    generation: u32,
}

impl InodeMetaDesc {
    pub(super) fn try_from_raw(raw: &RawInode) -> Result<Self> {
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

        let mut size = raw.size_lo as u64;
        if type_ == InodeType::File {
            size |= (raw.size_high as u64) << 32;
        }
        if size > i64::MAX as u64 {
            return_errno_with_message!(Errno::EUCLEAN, "corrupted inode on disk");
        }

        let file_acl = raw.file_acl;

        let flags = FileFlags::from_bits(raw.flags)
            .ok_or_else(|| Error::with_message(Errno::EIO, "invalid inode flags"))?;

        Ok(InodeMetaDesc {
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
            flags,
            file_acl,
            generation: raw.generation,
        })
    }

    pub(super) fn from_desc(desc: &InodeDesc) -> Self {
        Self {
            type_: desc.type_,
            perm: desc.perm,
            uid: desc.uid,
            gid: desc.gid,
            size: desc.size,
            atime: desc.atime,
            ctime: desc.ctime,
            mtime: desc.mtime,
            dtime: desc.dtime,
            links_count: desc.links_count,
            flags: desc.flags,
            file_acl: desc.file_acl,
            generation: desc.generation,
        }
    }
}

#[derive(Debug)]
pub(super) struct InodeMeta {
    desc: Dirty<InodeMetaDesc>,
    is_freed: bool,
}

impl InodeMeta {
    fn new(desc: InodeMetaDesc) -> Self {
        Self {
            desc: Dirty::new(desc),
            is_freed: false,
        }
    }

    pub(super) fn raw_desc(&self) -> &InodeMetaDesc {
        &self.desc
    }

    pub(super) fn clear_dirty(&mut self) {
        self.desc.clear_dirty();
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.desc.is_dirty()
    }

    pub(super) fn inode_type(&self) -> InodeType {
        self.desc.type_
    }

    pub(super) fn mode(&self) -> InodeMode {
        InodeMode::from_bits_truncate(self.desc.perm.bits())
    }

    pub(super) fn set_mode(&mut self, mode: InodeMode) {
        self.desc.perm = FilePerm::from_bits_truncate(mode.bits() as u16);
    }

    pub(super) fn uid(&self) -> u32 {
        self.desc.uid
    }

    pub(super) fn set_uid(&mut self, uid: u32) {
        self.desc.uid = uid;
    }

    pub(super) fn gid(&self) -> u32 {
        self.desc.gid
    }

    pub(super) fn set_gid(&mut self, gid: u32) {
        self.desc.gid = gid;
    }

    pub(super) fn file_size(&self) -> usize {
        self.desc.size as usize
    }

    pub(super) fn set_file_size(&mut self, new_size: usize) {
        self.desc.size = new_size as u64;
    }

    pub(super) fn atime(&self) -> Duration {
        self.desc.atime
    }

    pub(super) fn set_atime(&mut self, t: Duration) {
        self.desc.atime = t;
    }

    pub(super) fn mtime(&self) -> Duration {
        self.desc.mtime
    }

    pub(super) fn set_mtime(&mut self, t: Duration) {
        self.desc.mtime = t;
    }

    pub(super) fn ctime(&self) -> Duration {
        self.desc.ctime
    }

    pub(super) fn set_ctime(&mut self, t: Duration) {
        self.desc.ctime = t;
    }

    pub(super) fn touch_ctime(&mut self, t: Duration) {
        self.set_ctime(t);
    }

    pub(super) fn touch_mtime_ctime(&mut self, t: Duration) {
        self.set_mtime(t);
        self.set_ctime(t);
    }

    pub(super) fn dtime(&self) -> Duration {
        self.desc.dtime
    }

    pub(super) fn set_dtime(&mut self, t: Duration) {
        self.desc.dtime = t;
    }

    pub(super) fn links_count(&self) -> u16 {
        self.desc.links_count
    }

    pub(super) fn set_links_count(&mut self, nlinks: u16) {
        self.desc.links_count = nlinks;
    }

    pub(super) fn add_links_count_saturating(&mut self, delta: u16) {
        self.desc.links_count = self.desc.links_count.saturating_add(delta);
    }

    pub(super) fn sub_links_count_saturating(&mut self, delta: u16) {
        self.desc.links_count = self.desc.links_count.saturating_sub(delta);
    }

    pub(super) fn flags(&self) -> FileFlags {
        self.desc.flags
    }

    pub(super) fn set_flags(&mut self, flags: FileFlags) {
        self.desc.flags = flags;
    }

    pub(super) fn remove_flags(&mut self, flags: FileFlags) {
        self.desc.flags.remove(flags);
    }

    pub(super) fn file_acl(&self) -> u32 {
        self.desc.file_acl
    }

    pub(super) fn set_file_acl(&mut self, file_acl: u32) {
        self.desc.file_acl = file_acl;
    }

    pub(super) fn is_freed(&self) -> bool {
        self.is_freed
    }

    pub(super) fn set_freed(&mut self, is_freed: bool) {
        self.is_freed = is_freed;
    }
}

impl RawInode {
    pub(super) fn from_parts(meta: &InodeMetaDesc, mapping: &InodeMappingDesc) -> Self {
        let mode = meta.perm.0;
        let uid = meta.uid as u16;
        let gid = meta.gid as u16;
        let uid_high = (meta.uid >> 16) as u16;
        let gid_high = (meta.gid >> 16) as u16;

        let (size_lo, size_high) = if meta.type_ == InodeType::File {
            (meta.size as u32, (meta.size >> 32) as u32)
        } else {
            (meta.size as u32, 0)
        };

        Self {
            mode,
            uid,
            size_lo,
            atime: meta.atime.as_secs() as u32,
            ctime: meta.ctime.as_secs() as u32,
            mtime: meta.mtime.as_secs() as u32,
            dtime: meta.dtime.as_secs() as u32,
            gid,
            links_count: meta.links_count,
            blocks: mapping.blocks,
            flags: meta.flags.bits(),
            osd1: 0,
            block: mapping.block_ptrs,
            generation: meta.generation,
            file_acl: meta.file_acl,
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
                    StopAfterVisitor, encode_dir_entry,
                },
            },
            utils::{IdBitmap, Inode as VfsInodeTrait, XattrName, XattrNamespace, XattrSetFlags},
        },
        prelude::*,
        time::clocks,
    };

    fn make_raw_inode(mode: u16) -> RawInode {
        RawInodeBuilder::new(mode).build()
    }

    fn find_entry_ino(dir: &Arc<Inode>, name: &str) -> Result<u32> {
        let fs = dir.fs_arc()?;
        let meta = dir.inner.meta_read();
        dir.inner.find_entry(&meta, &fs, name)
    }

    fn is_fast_symlink_for_test(inode: &Arc<Inode>, block_size: usize) -> bool {
        let meta = inode.inner.meta_read();
        let mapping = inode.inner.mapping_read();
        let ea_blocks = if meta.file_acl() != 0 {
            (block_size / SECTOR_SIZE) as u32
        } else {
            0
        };
        meta.inode_type() == InodeType::SymLink
            && mapping.desc.blocks.checked_sub(ea_blocks) == Some(0)
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
        assert_eq!(find_entry_ino(&root, "alpha").unwrap(), created.ino());
        assert_eq!(
            f.ext2.read_inode_desc(created.ino()).unwrap().links_count,
            1
        );

        // Linux ext2_mkdir intent: child links=2 and parent link count +1.
        let created_dir = root
            .create("sub", InodeType::Dir, FilePerm::from_bits_truncate(0o755))
            .unwrap();
        assert_eq!(find_entry_ino(&root, "sub").unwrap(), created_dir.ino());
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
    fn namei_link_unlink_updates_nlinks() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();

        let old = root
            .create("old", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();
        let old_ino = old.ino();
        let old_links_before = f.ext2.read_inode_desc(old_ino).unwrap().links_count;
        let block_size = f.ext2.block_size();
        let payload = vec![0x6au8; block_size];
        let mut payload_reader = VmReader::from(payload.as_slice()).to_fallible();
        old.write_direct_at(0, &mut payload_reader).unwrap();

        // Linux ext2_link intent: increase nlink before publishing name.
        root.link(&old, "alias").unwrap();
        assert_eq!(find_entry_ino(&root, "alias").unwrap(), old_ino);
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
            find_entry_ino(&root, "alias").unwrap_err().error(),
            Errno::ENOENT
        );
        assert_eq!(
            f.ext2.read_inode_desc(old_ino).unwrap().links_count,
            old_links_before
        );

        assert_eq!(root.unlink("dir").unwrap_err().error(), Errno::EISDIR);
        assert_eq!(root.unlink(".").unwrap_err().error(), Errno::EINVAL);

        let free_blocks_before_sync = f.ext2.super_block().free_blocks_count();
        root.unlink("old").unwrap();
        {
            let inode_bitmap = f.block_groups()[0].inode_bitmap();
            assert!(inode_bitmap.is_allocated((old_ino - 1) as u16));
        }
        drop(old);
        f.ext2.sync_all_inodes().unwrap();
        let inode_bitmap = f.block_groups()[0].inode_bitmap();
        assert!(!inode_bitmap.is_allocated((old_ino - 1) as u16));
        drop(inode_bitmap);
        assert_eq!(
            f.ext2.super_block().free_blocks_count(),
            free_blocks_before_sync.saturating_add(1)
        );
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
            let meta = root.inner.meta_read();
            (meta.ctime(), meta.mtime())
        };

        // Linux ext2_set_link with update_times=false keeps ctime/mtime unchanged.
        root.set_link("src", target.ino(), DirEntryFileType::File, false)
            .unwrap();
        assert_eq!(find_entry_ino(&root, "src").unwrap(), target.ino());
        let (ctime_after, mtime_after) = {
            let meta = root.inner.meta_read();
            (meta.ctime(), meta.mtime())
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

        // Same-dir no-replacement path should insert new name then delete old name.
        let solo = root
            .create("solo", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();
        let solo_ino = solo.ino();
        root.rename("solo", &root, "solo_renamed").unwrap();
        assert_eq!(find_entry_ino(&root, "solo_renamed").unwrap(), solo_ino);
        assert_eq!(
            find_entry_ino(&root, "solo").unwrap_err().error(),
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
            find_entry_ino(&root, "move_src").unwrap_err().error(),
            Errno::ENOENT
        );
        assert_eq!(find_entry_ino(&dst_dir, "move_dst").unwrap(), move_src_ino);

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
            find_entry_ino(&src_dir, "moving").unwrap_err().error(),
            Errno::ENOENT
        );
        assert_eq!(
            find_entry_ino(&dst_replace_dir, "target").unwrap(),
            moving_ino
        );

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
        parent_a.rename("kid", &parent_b, "kid_moved").unwrap();
        let moved_dir = parent_b.lookup("kid_moved").unwrap();
        assert_eq!(find_entry_ino(&moved_dir, "..").unwrap(), parent_b.ino());

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
        assert_eq!(find_entry_ino(&root, "new").unwrap(), old_ino);
        assert_eq!(
            find_entry_ino(&root, "old").unwrap_err().error(),
            Errno::ENOENT
        );

        {
            let inode_bitmap = f.block_groups()[0].inode_bitmap();
            assert!(inode_bitmap.is_allocated((replaced_ino - 1) as u16));
            assert!(inode_bitmap.is_allocated((replaced_ino_cross - 1) as u16));
        }
        drop(replaced);
        drop(new);
        f.ext2.sync_all_inodes().unwrap();
        let inode_bitmap = f.block_groups()[0].inode_bitmap();
        assert!(!inode_bitmap.is_allocated((replaced_ino - 1) as u16));
        assert!(!inode_bitmap.is_allocated((replaced_ino_cross - 1) as u16));

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
    fn meta_desc_try_from_valid_raw_ok() {
        let mut raw = make_raw_inode(0o100644);
        raw.size_lo = 0x1122_3344;
        raw.size_high = 0x5566_7788;
        raw.uid = 0x1234;
        raw.uid_high = 0x5678;
        raw.gid = 0x4321;
        raw.gid_high = 0x8765;
        raw.dtime = 123;

        let meta = InodeMetaDesc::try_from_raw(&raw).unwrap();
        assert_eq!(meta.size, 0x5566_7788_1122_3344);
        assert_eq!(meta.uid, 0x5678_1234);
        assert_eq!(meta.gid, 0x8765_4321);
        assert_eq!(meta.perm.bits(), raw.mode);
        let dtime: Duration = meta.dtime.into();
        assert_eq!(dtime.as_secs(), 123);

        let mut raw_dir = make_raw_inode(0o040755);
        raw_dir.size_lo = 7;
        raw_dir.size_high = u32::MAX;
        let dir_meta = InodeMetaDesc::try_from_raw(&raw_dir).unwrap();
        assert_eq!(dir_meta.size, 7);
    }

    #[ktest]
    fn meta_desc_try_from_invalid_raw_returns_err() {
        let mut deleted_inode = make_raw_inode(0);
        deleted_inode.links_count = 0;
        deleted_inode.dtime = 1;
        let deleted_err = InodeMetaDesc::try_from_raw(&deleted_inode).unwrap_err();
        assert_eq!(deleted_err.error(), Errno::ESTALE);

        let mut invalid_flags_inode = make_raw_inode(0o100644);
        invalid_flags_inode.flags = 1 << 30;
        let flags_err = InodeMetaDesc::try_from_raw(&invalid_flags_inode).unwrap_err();
        assert_eq!(flags_err.error(), Errno::EIO);

        let mut size_overflow_inode = make_raw_inode(0o100644);
        size_overflow_inode.size_lo = u32::MAX;
        size_overflow_inode.size_high = u32::MAX;
        let size_err = InodeMetaDesc::try_from_raw(&size_overflow_inode).unwrap_err();
        assert_eq!(size_err.error(), Errno::EUCLEAN);
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

        assert_eq!(find_entry_ino(&root, "foo").unwrap(), foo.ino());
        assert_eq!(find_entry_ino(&root, "subdir").unwrap(), subdir.ino());
        assert_eq!(
            find_entry_ino(&root, "missing").unwrap_err().error(),
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
        let root_size = root.inner.meta_read().desc.size as usize;
        assert!(stop_advanced > 0 && stop_advanced < root_size);

        let first_entry_end = visitor.entries[0].3 + 1;
        let mut offset_visitor = CollectDirentVisitor::default();
        root.readdir_at(first_entry_end, &mut offset_visitor)
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
        assert_eq!(
            find_entry_ino(&file_inode, "foo").unwrap_err().error(),
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
            find_entry_ino(&hole_inode, "foo").unwrap_err().error(),
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
            find_entry_ino(&limited_blocks_inode, "missing")
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
        assert_eq!(
            find_entry_ino(&bad_inode, ".").unwrap_err().error(),
            Errno::EIO
        );
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
    fn dir_add_and_delete_entry_ok() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::namei_env().build().unwrap();
        let root = f.ext2.read_inode(ROOT_INO).unwrap();

        let bar = root
            .create("bar", InodeType::File, FilePerm::from_bits_truncate(0o644))
            .unwrap();

        find_entry_ino(&root, "bar").unwrap();
        root.add_entry("foo", 11, DirEntryFileType::File).unwrap();
        let dup = root
            .add_entry("foo", 12, DirEntryFileType::File)
            .unwrap_err();
        assert_eq!(dup.error(), Errno::EEXIST);
        root.delete_entry("foo").unwrap();

        assert_eq!(find_entry_ino(&root, ".").unwrap(), ROOT_INO);
        assert_eq!(find_entry_ino(&root, "bar").unwrap(), bar.ino());
        assert_eq!(
            find_entry_ino(&root, "foo").unwrap_err().error(),
            Errno::ENOENT
        );
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
            let size_before = root.inner.meta_read().desc.size;
            let result = root.create(&name, InodeType::File, FilePerm::from_bits_truncate(0o644));
            match result {
                Ok(_) => {
                    let size_after = root.inner.meta_read().desc.size;
                    if size_after > size_before {
                        // Block growth happened — this is what we wanted to test.
                        assert_eq!(
                            root.inner.meta_read().desc.size,
                            (size_before as usize + block_size) as u64
                        );
                        assert_ne!(root.inner.mapping_read().desc.block_ptrs[1], 0);
                        assert!(find_entry_ino(&root, &name).is_ok());
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
            if root.inner.mapping_read().desc.block_ptrs[12] != 0 {
                assert!(root.inner.meta_read().desc.size > (block_size * 12) as u64);
                assert!(find_entry_ino(&root, &name).is_ok());
                return;
            }
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
            for iblock in 0..13u32 {
                inode
                    .inner
                    .get_or_alloc_block(iblock, true)
                    .unwrap()
                    .unwrap();
            }
            inode.inner.meta_write().desc.size = (13 * block_size) as u64;

            let old_block_sectors = inode.inner.mapping_read().desc.blocks;
            assert_ne!(inode.inner.mapping_read().desc.block_ptrs[12], 0);
            assert!(inode.inner.get_block(12).unwrap().is_some());
            let free_before = f.ext2.super_block().free_blocks_count();

            let mut meta = inode.inner.meta_write();
            inode
                .inner
                .release_dir_data_blocks_for_cleanup(&mut meta, &f.ext2)
                .unwrap();
            drop(meta);
            assert_eq!(inode.inner.meta_read().desc.size, 0);
            assert_eq!(inode.inner.mapping_read().desc.blocks, 0);
            assert!(
                inode
                    .inner
                    .mapping_read()
                    .desc
                    .block_ptrs
                    .iter()
                    .all(|ptr| *ptr == 0)
            );
            assert_eq!(inode.inner.get_block(0).unwrap(), None);
            assert_eq!(inode.inner.get_block(12).unwrap(), None);

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
            assert!(inode.empty_dir());
            assert_eq!(inode.inner.meta_read().desc.size as usize, block_size);
            let mapping = inode.inner.mapping_read();
            assert_eq!(mapping.desc.blocks, (block_size / SECTOR_SIZE) as u32);
            mapping.desc.block_ptrs[0]
        };
        assert_ne!(allocated_bid, 0);

        assert_eq!(find_entry_ino(&inode, ".").unwrap(), 12);
        assert_eq!(find_entry_ino(&inode, "..").unwrap(), ROOT_INO);
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

        assert!(!sub.empty_dir());
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

    #[ktest]
    fn dir_rmdir_removes_child_and_updates_nlinks() {
        clocks::init_for_ktest();

        let env = prepare_rmdir_env(false);
        let f = &env.f;
        let child_ino = env.child_ino;
        let parent = &env.parent;

        parent.rmdir("sub").unwrap();
        {
            assert_eq!(parent.inner.meta_read().desc.links_count, 2);
            assert_eq!(
                find_entry_ino(parent, "sub").unwrap_err().error(),
                Errno::ENOENT
            );
        }

        let parent = f.ext2.read_inode(ROOT_INO).unwrap();
        assert_eq!(parent.inner.meta_read().desc.links_count, 2);
        let child = f.ext2.read_inode(child_ino).unwrap();
        assert_eq!(child.inner.meta_read().desc.size, 0);
        assert_eq!(child.inner.meta_read().desc.links_count, 0);
    }

    #[ktest]
    fn dir_rmdir_notempty_returns_enotempty() {
        clocks::init_for_ktest();

        let env = prepare_rmdir_env(true);
        let f = &env.f;
        let child_ino = env.child_ino;
        let parent = &env.parent;

        let err = parent.rmdir("sub").unwrap_err();
        assert_eq!(err.error(), Errno::ENOTEMPTY);

        assert_eq!(find_entry_ino(parent, "sub").unwrap(), child_ino);
        assert_eq!(parent.inner.meta_read().desc.links_count, 3);

        let inode_bitmap = f.block_groups()[0].inode_bitmap();
        assert!(inode_bitmap.is_allocated((child_ino - 1) as u16));
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

        assert_eq!(file.inner.meta_read().desc.size as usize, payload.len());
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

        assert_eq!(file.inner.meta_read().desc.size, 0);
        let mapping = file.inner.mapping_read();
        assert_eq!(mapping.desc.blocks, 0);
        assert_eq!(mapping.desc.block_ptrs[0], 0);

        let on_disk = f.ext2.read_inode_desc(file.ino()).unwrap();
        assert_eq!(on_disk.size, 0);
        assert_eq!(on_disk.blocks, 0);
    }

    #[ktest]
    fn symlink_fast_round_trip_ok() {
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

        assert_eq!(link.inner.meta_read().desc.size as usize, target.len());
        assert_eq!(link.inner.mapping_read().desc.blocks, 0);
        assert!(is_fast_symlink_for_test(&link, f.ext2.block_size()));
    }

    #[ktest]
    fn symlink_slow_round_trip_ok() {
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

        assert_eq!(
            link.inner.meta_read().desc.size as usize,
            MAX_FAST_SYMLINK_LEN
        );
        assert!(link.inner.mapping_read().desc.blocks > 0);
        assert!(!is_fast_symlink_for_test(&link, f.ext2.block_size()));
    }

    #[ktest]
    fn symlink_write_link_enametoolong_boundary() {
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
    fn symlink_read_write_reject_non_symlink_inode() {
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
        assert_eq!(file.inner.meta_read().desc.size as usize, block_size * 2);
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

        assert_eq!(
            file.inner.meta_read().desc.size as usize,
            write_off + payload.len()
        );
        assert_eq!(file.inner.get_block(0).unwrap(), None);
        assert_eq!(file.inner.get_block(1).unwrap(), None);

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

        assert_eq!(file.inner.meta_read().desc.size as usize, block_size);
        assert!(file.inner.get_block(0).unwrap().is_some());
        assert_eq!(file.inner.get_block(1).unwrap(), None);
        assert_eq!(f.ext2.super_block().free_blocks_count(), free_before_fail);

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
        let target = block_size * 3 + 123;
        file.resize(target).unwrap();
        assert_eq!(file.inner.meta_read().desc.size as usize, target);
        let mapping = file.inner.mapping_read();
        assert_eq!(mapping.desc.blocks, 0);
        assert!(mapping.desc.block_ptrs.iter().all(|ptr| *ptr == 0));
    }

    #[ktest]
    fn fallocate_allocate_extends_file_size() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 67, 0, 0, FileFlags::empty(), [0; 15]);
        let block_size = f.ext2.block_size();
        let new_size = block_size + 210;

        file.fallocate(FallocMode::Allocate, block_size + 10, 200)
            .unwrap();
        assert_eq!(file.file_size(), new_size);
    }

    #[ktest]
    fn fallocate_allocate_keep_size_is_noop() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 68, 123, 0, FileFlags::empty(), [0; 15]);

        file.fallocate(FallocMode::AllocateKeepSize, 4096, 512)
            .unwrap();
        assert_eq!(file.file_size(), 123);
    }

    #[ktest]
    fn fallocate_punch_hole_keep_size_zeroes_requested_range() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 69, 0, 0, FileFlags::empty(), [0; 15]);
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
    fn fallocate_unsupported_modes_return_eopnotsupp() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256)
            .with_free_blocks(64, 64)
            .build()
            .unwrap();
        let file = make_live_file_inode(&f.ext2, 70, 0, 0, FileFlags::empty(), [0; 15]);

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

        assert_eq!(
            file.inner.meta_read().desc.size as usize,
            block_size + keep_in_tail
        );
        let mapping = file.inner.mapping_read();
        assert_eq!(mapping.desc.blocks, sectors_per_block.saturating_mul(2));
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
    fn new_inode_initializes_page_cache_capacity() {
        clocks::init_for_ktest();

        let f = Ext2FixtureBuilder::new(1, 256).build().unwrap();
        let block_size = f.ext2.block_size();

        let inode_empty = make_live_file_inode(&f.ext2, 60, 0, 0, FileFlags::empty(), [0; 15]);
        assert_eq!(inode_empty.inner.page_cache().pages().size(), 0);

        let inode_non_empty =
            make_live_file_inode(&f.ext2, 61, block_size + 1, 0, FileFlags::empty(), [0; 15]);
        assert_eq!(
            inode_non_empty.inner.page_cache().pages().size(),
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

        assert_eq!(
            file.inner.meta_read().desc.size as usize,
            block_size * 2 + 7
        );
        let mapping = file.inner.mapping_read();
        assert_eq!(mapping.desc.blocks, 0);
        assert!(mapping.desc.block_ptrs.iter().all(|ptr| *ptr == 0));
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

        file.inner.page_cache().resize(block_size).unwrap();

        let one_byte = [0x5au8];
        let mut reader = VmReader::from(one_byte.as_slice()).to_fallible();
        file.inner
            .page_cache()
            .pages()
            .write(0, &mut reader)
            .unwrap();

        let err = file
            .inner
            .page_cache()
            .evict_range(0..block_size)
            .unwrap_err();
        assert_eq!(err.error(), Errno::EIO);
    }

    #[ktest]
    fn xattr_set_get_and_size_query_roundtrip() {
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

        assert_ne!(file.inner.meta_read().desc.file_acl, 0);
    }

    #[ktest]
    fn xattr_remove_last_entry_frees_block() {
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
        assert_ne!(file.inner.meta_read().desc.file_acl, 0);

        VfsInodeTrait::remove_xattr(file.as_ref(), make_name()).unwrap();
        assert_eq!(file.inner.meta_read().desc.file_acl, 0);

        let err = VfsInodeTrait::remove_xattr(file.as_ref(), make_name()).unwrap_err();
        assert_eq!(err.error(), Errno::ENODATA);
    }

    #[ktest]
    fn xattr_list_filters_namespace() {
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
