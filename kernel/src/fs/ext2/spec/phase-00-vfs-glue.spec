[PROMPT]
Provide `kernel/src/fs/ext2/fs_type.rs`. Output Rust code only. No unsafe. No panic/assert/unimplemented.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
Ext2Type (fs type registration) → fs/ext2/super.c:1698 (ext2_fs_type)
Ext2Type::name                  → fs/ext2/super.c:1700 (.name = "ext2")
Ext2Type::properties            → fs/ext2/super.c:1702 (.fs_flags = FS_REQUIRES_DEV)
Ext2Type::create                → fs/ext2/super.c:1703 (init_fs_context -> mount flow)
Ext2Type::sysnode               → fs/ext2/super.c:1698 (ext2_fs_type)

[RELY]
```rust
use super::prelude::*;
```

```rust
use crate::fs::registry::{FsProperties, FsType};
```

```rust
use crate::fs::utils::{FileSystem, FsFlags};
```

```rust
use aster_systree::SysNode;
```

```rust
/// VFS-visible Ext2 filesystem type.
pub(super) struct Ext2Type;
```

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
- In this skeleton phase, returns `Err(Errno::ENOSYS)`.
- Must not perform any I/O or access `disk`.
- Must not mutate global state.
