// SPDX-License-Identifier: MPL-2.0

use core::time::Duration;

use crate::{
    fs::{
        ext2::{FilePerm, Inode},
        inode_handle::FileIo,
        utils::{
            AccessMode, DirentVisitor, Extension, FallocMode, FileSystem, Inode as VfsInode,
            InodeIo, InodeMode, InodeType, Metadata, MknodType, StatusFlags, SymbolicLink,
            XattrName, XattrNamespace, XattrSetFlags,
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
        // DIFF from Linux direct-io split path: current Ext2 read path is block-device direct,
        // so O_DIRECT and non-O_DIRECT share the same implementation.
        let _ = status_flags;
        Inode::read_at(self, offset, writer)
    }

    fn write_at(
        &self,
        offset: usize,
        reader: &mut VmReader,
        status_flags: StatusFlags,
    ) -> Result<usize> {
        // DIFF from Linux direct-io split path: current Ext2 write path is block-device direct,
        // so O_DIRECT and non-O_DIRECT share the same implementation.
        let _ = status_flags;
        Inode::write_at(self, offset, reader)
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
        // DIFF from Linux: page cache integration is pending in current Ext2 phase.
        None
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

    fn mknod(&self, _name: &str, _mode: InodeMode, _type_: MknodType) -> Result<Arc<dyn VfsInode>> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "mknod is not supported yet");
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
        return_errno_with_message!(Errno::EOPNOTSUPP, "symlink read is not supported yet");
    }

    fn write_link(&self, _target: &str) -> Result<()> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "symlink write is not supported yet");
    }

    fn sync_all(&self) -> Result<()> {
        Inode::sync_all(self)
    }

    fn sync_data(&self) -> Result<()> {
        Inode::sync_data(self)
    }

    fn fallocate(&self, _mode: FallocMode, _offset: usize, _len: usize) -> Result<()> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "fallocate is not supported yet");
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
        _name: XattrName,
        _value_reader: &mut VmReader,
        _flags: XattrSetFlags,
    ) -> Result<()> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "xattr is not supported yet");
    }

    fn get_xattr(&self, _name: XattrName, _value_writer: &mut VmWriter) -> Result<usize> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "xattr is not supported yet");
    }

    fn list_xattr(
        &self,
        _namespace: XattrNamespace,
        _list_writer: &mut VmWriter,
    ) -> Result<usize> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "xattr is not supported yet");
    }

    fn remove_xattr(&self, _name: XattrName) -> Result<()> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "xattr is not supported yet");
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
