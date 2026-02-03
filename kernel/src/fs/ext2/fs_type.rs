// SPDX-License-Identifier: MPL-2.0

use super::prelude::*;
use crate::fs::registry::{FsProperties, FsType};
use crate::fs::utils::{FileSystem, FsFlags};
use aster_systree::SysNode;

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
        _args: Option<CString>,
        _disk: Option<Arc<dyn BlockDevice>>,
    ) -> Result<Arc<dyn FileSystem>> {
        return_errno!(Errno::ENOSYS);
    }

    fn sysnode(&self) -> Option<Arc<dyn SysNode>> {
        None
    }
}
