// SPDX-License-Identifier: MPL-2.0

use aster_systree::SysNode;

use super::{fs::Ext2, prelude::*};
use crate::fs::vfs::{
    file_system::{FileSystem, FsFlags},
    registry::{FsProperties, FsType},
};

/// VFS-visible Ext2 filesystem type.
pub(super) struct Ext2Type;

impl FsType for Ext2Type {
    fn name(&self) -> &'static str {
        "ext2"
    }

    fn properties(&self) -> FsProperties {
        FsProperties::NEED_DISK
    }

    fn create(
        &self,
        _flags: FsFlags,
        args: Option<CString>,
        disk: Option<Arc<dyn BlockDevice>>,
    ) -> Result<Arc<dyn FileSystem>> {
        let disk = disk.ok_or_else(|| {
            Error::with_message(Errno::EINVAL, "the ext2 filesystem requires a block device")
        })?;
        Ext2::open(disk, args.as_deref()).map(|fs| fs as Arc<dyn FileSystem>)
    }

    fn sysnode(&self) -> Option<Arc<dyn SysNode>> {
        None
    }
}
