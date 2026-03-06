// SPDX-License-Identifier: MPL-2.0

use core::time::Duration;

use aster_block::bio::BioStatus;

use crate::{
    fs::{
        ext2::{FilePerm, Inode},
        inode_handle::FileIo,
        utils::{
            AccessMode, DirentVisitor, Extension, FallocMode, FileSystem, Inode as VfsInode,
            InodeIo, InodeMode, InodeType, Metadata, MknodType, Permission, StatusFlags,
            SymbolicLink, XattrName, XattrNamespace, XattrSetFlags,
        },
    },
    prelude::*,
    process::{Gid, Uid},
    vm::vmo::Vmo,
};

impl InodeIo for Inode {
    fn read_at(
        &self,
        offset: usize,
        writer: &mut VmWriter,
        status_flags: StatusFlags,
    ) -> Result<usize> {
        // Linux: /root/linux/fs/ext2/file.c:283 (ext2_file_read_iter)
        if status_flags.contains(StatusFlags::O_DIRECT) {
            Inode::read_direct_at(self, offset, writer)
        } else {
            Inode::read_at(self, offset, writer)
        }
    }

    fn write_at(
        &self,
        offset: usize,
        reader: &mut VmReader,
        status_flags: StatusFlags,
    ) -> Result<usize> {
        // Linux: /root/linux/fs/ext2/file.c:295 (ext2_file_write_iter)
        if status_flags.contains(StatusFlags::O_DIRECT) {
            Inode::write_direct_at(self, offset, reader)
        } else {
            Inode::write_at(self, offset, reader)
        }
    }
}

impl VfsInode for Inode {
    fn size(&self) -> usize {
        Inode::file_size(self)
    }

    fn resize(&self, new_size: usize) -> Result<()> {
        Inode::resize(self, new_size)
    }

    fn metadata(&self) -> Metadata {
        Inode::metadata(self)
    }

    fn ino(&self) -> u64 {
        Inode::ino(self) as u64
    }

    fn type_(&self) -> InodeType {
        Inode::inode_type(self)
    }

    fn mode(&self) -> Result<InodeMode> {
        Ok(Inode::mode(self))
    }

    fn set_mode(&self, mode: InodeMode) -> Result<()> {
        Inode::set_mode(self, mode)
    }

    fn owner(&self) -> Result<Uid> {
        Ok(Uid::new(Inode::uid(self)))
    }

    fn set_owner(&self, uid: Uid) -> Result<()> {
        Inode::set_uid(self, uid.into())
    }

    fn group(&self) -> Result<Gid> {
        Ok(Gid::new(Inode::gid(self)))
    }

    fn set_group(&self, gid: Gid) -> Result<()> {
        Inode::set_gid(self, gid.into())
    }

    fn atime(&self) -> Duration {
        Inode::atime(self)
    }

    fn set_atime(&self, time: Duration) {
        Inode::set_atime(self, time)
    }

    fn mtime(&self) -> Duration {
        Inode::mtime(self)
    }

    fn set_mtime(&self, time: Duration) {
        Inode::set_mtime(self, time)
    }

    fn ctime(&self) -> Duration {
        Inode::ctime(self)
    }

    fn set_ctime(&self, time: Duration) {
        Inode::set_ctime(self, time)
    }

    fn page_cache(&self) -> Option<Arc<Vmo>> {
        Some(Inode::page_cache_vmo(self))
    }

    fn open(
        &self,
        _access_mode: AccessMode,
        _status_flags: StatusFlags,
    ) -> Option<Result<Box<dyn FileIo>>> {
        None
    }

    fn create(&self, name: &str, type_: InodeType, mode: InodeMode) -> Result<Arc<dyn VfsInode>> {
        Ok(Inode::create(self, name, type_, mode.into())?)
    }

    fn mknod(&self, name: &str, mode: InodeMode, type_: MknodType) -> Result<Arc<dyn VfsInode>> {
        // Linux: /root/linux/fs/ext2/namei.c:136-155 (ext2_mknod)
        // SPEC: map mknod request to ext2 inode type plus optional encoded device id.
        let (inode_type, device_id) = match type_ {
            MknodType::CharDevice(dev_id) => (InodeType::CharDevice, Some(dev_id)),
            MknodType::BlockDevice(dev_id) => (InodeType::BlockDevice, Some(dev_id)),
            MknodType::NamedPipe => (InodeType::NamedPipe, None),
        };

        let new_inode = Inode::create(self, name, inode_type, mode.into())?;
        if let Some(device_id) = device_id {
            // SPEC: persist Linux-compatible i_block[0..2] device encoding.
            new_inode.set_device_id(device_id)?;
        }

        Ok(new_inode)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn VfsInode>> {
        Ok(Inode::lookup(self, name)?)
    }

    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        Inode::readdir_at(self, offset, visitor)
    }

    fn link(&self, old: &Arc<dyn VfsInode>, name: &str) -> Result<()> {
        let old = old
            .downcast_ref::<Inode>()
            .ok_or_else(|| Error::with_message(Errno::EXDEV, "not same fs"))?;
        Inode::link(self, old, name)
    }

    fn unlink(&self, name: &str) -> Result<()> {
        Inode::unlink(self, name)
    }

    fn rmdir(&self, name: &str) -> Result<()> {
        Inode::rmdir(self, name)
    }

    fn rename(&self, old_name: &str, target: &Arc<dyn VfsInode>, new_name: &str) -> Result<()> {
        let target = target
            .downcast_ref::<Inode>()
            .ok_or_else(|| Error::with_message(Errno::EXDEV, "not same fs"))?;
        Inode::rename(self, old_name, target, new_name)
    }

    fn read_link(&self) -> Result<SymbolicLink> {
        Inode::read_link(self).map(SymbolicLink::Plain)
    }

    fn write_link(&self, target: &str) -> Result<()> {
        Inode::write_link(self, target)
    }

    fn sync_all(&self) -> Result<()> {
        // Linux: /root/linux/fs/ext2/file.c:155 (ext2_fsync)
        Inode::sync_all(self)?;
        if Inode::fs_arc(self)?.block_device().sync()? != BioStatus::Complete {
            return_errno_with_message!(Errno::EIO, "failed to flush block device");
        }
        Ok(())
    }

    fn sync_data(&self) -> Result<()> {
        // Linux: /root/linux/fs/buffer.c:602 (generic_buffers_fsync_noflush)
        Inode::sync_data(self)?;
        if Inode::fs_arc(self)?.block_device().sync()? != BioStatus::Complete {
            return_errno_with_message!(Errno::EIO, "failed to flush block device");
        }
        Ok(())
    }

    fn fallocate(&self, mode: FallocMode, offset: usize, len: usize) -> Result<()> {
        // Linux ext2 has no `.fallocate` file operation
        // (/root/linux/fs/ext2/file.c:313-328), so delegate to the
        // Asterinas compatibility implementation.
        Inode::fallocate(self, mode, offset, len)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        // SPEC: the inode must belong to a live filesystem instance.
        Inode::fs_arc(self).unwrap()
    }

    fn extension(&self) -> &Extension {
        Inode::extension(self)
    }

    fn set_xattr(
        &self,
        name: XattrName,
        value_reader: &mut VmReader,
        flags: XattrSetFlags,
    ) -> Result<()> {
        self.check_permission(Permission::MAY_WRITE)?;
        Inode::set_xattr(self, name, value_reader, flags)
    }

    fn get_xattr(&self, name: XattrName, value_writer: &mut VmWriter) -> Result<usize> {
        self.check_permission(Permission::MAY_READ)?;
        Inode::get_xattr(self, name, value_writer)
    }

    fn list_xattr(&self, namespace: XattrNamespace, list_writer: &mut VmWriter) -> Result<usize> {
        self.check_permission(Permission::MAY_ACCESS)?;
        Inode::list_xattr(self, namespace, list_writer)
    }

    fn remove_xattr(&self, name: XattrName) -> Result<()> {
        self.check_permission(Permission::MAY_WRITE)?;
        Inode::remove_xattr(self, name)
    }
}

impl From<FilePerm> for InodeMode {
    fn from(perm: FilePerm) -> Self {
        Self::from_bits_truncate(perm.bits() as _)
    }
}

impl From<InodeMode> for FilePerm {
    fn from(mode: InodeMode) -> Self {
        Self::from_bits_truncate(mode.bits() as _)
    }
}
