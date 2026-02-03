[PROMPT]
Provide `kernel/src/fs/ext2/fs_type.rs`. Output Rust code only. No unsafe. No panic/assert/unimplemented.
This module only provides VFS registration glue; no on-disk logic.

[RELY]
use super::prelude::*;
use crate::fs::registry::{FsProperties, FsType};
use crate::fs::utils::{FileSystem, FsFlags};
use aster_systree::SysNode;

/// VFS-visible Ext2 filesystem type.
pub(super) struct Ext2Type;

[GUARANTEE]
impl FsType for Ext2Type {
    fn name(&self) -> &'static str;
    fn properties(&self) -> FsProperties;
    fn create(
        &self,
        flags: FsFlags,
        args: Option<CString>,
        disk: Option<Arc<dyn BlockDevice>>,
    ) -> Result<Arc<dyn FileSystem>>;
    fn sysnode(&self) -> Option<Arc<dyn SysNode>>;
}

[SPECIFICATION]
Pre (name/properties/sysnode/create):
- `self` is a valid `Ext2Type` value.

Post (name):
- Returns the literal string "ext2".

Post (properties):
- Returns `FsProperties::NEED_DISK`.

Post (sysnode):
- Returns `None` (Ext2 does not appear under SysFS in this phase).

Post (create):
- Returns `Err(Errno::ENOSYS)`.
- Must not perform any I/O or access `disk`.
- Must not mutate global state.
