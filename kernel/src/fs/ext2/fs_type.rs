// SPDX-License-Identifier: MPL-2.0

use super::prelude::*;
use crate::fs::registry::{FsProperties, FsType};
use crate::fs::utils::{FileSystem, FsFlags};
use aster_systree::SysNode;

/// VFS-visible Ext2 filesystem type.
/// Linux: /root/linux/fs/ext2/super.c:1698 (ext2_fs_type)
pub(super) struct Ext2Type;

impl FsType for Ext2Type {
    fn name(&self) -> &'static str {
        // Linux: /root/linux/fs/ext2/super.c:1700 (.name = "ext2")
        "ext2"
    }

    fn properties(&self) -> FsProperties {
        // Linux: /root/linux/fs/ext2/super.c:1702 (.fs_flags = FS_REQUIRES_DEV)
        FsProperties::NEED_DISK
    }

    fn create(
        &self,
        _flags: FsFlags,
        _args: Option<CString>,
        _disk: Option<Arc<dyn BlockDevice>>,
    ) -> Result<Arc<dyn FileSystem>> {
        // Linux: /root/linux/fs/ext2/super.c:1703 (init_fs_context -> mount flow)
        return_errno!(Errno::ENOSYS);
    }

    fn sysnode(&self) -> Option<Arc<dyn SysNode>> {
        // Linux: /root/linux/fs/ext2/super.c:1698 (ext2_fs_type)
        None
    }
}
