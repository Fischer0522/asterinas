# Phase 1 - Supplementary Struct Design (`fs_type.rs`)

Target implementation file: `kernel/src/fs/ext2/fs_type.rs`

## 1) Structure Definition

```rust
/// VFS-visible Ext2 filesystem type registration object.
///
/// # Linux Reference
/// - Source: `fs/ext2/super.c:1698-1705`
/// - Corresponds to: `static struct file_system_type ext2_fs_type`
///
/// # Concurrency
/// - Stateless singleton; no mutable interior state in the type object itself.
/// - Mount-time mutable state is carried by per-mount context object.
#[derive(Debug)]
pub struct Ext2Type;

/// Per-mount Ext2 context for parsing mount options before `Ext2::open`.
///
/// # Linux Reference
/// - Source: `fs/ext2/super.c:476-486`
/// - Corresponds to: `struct ext2_fs_context`
#[derive(Clone, Debug)]
pub struct Ext2MountContext {
    /// Pending VFS super flags delta.
    /// Linux: `vals_s_flags`, `mask_s_flags`
    /// Linux: `fs/ext2/super.c:477-478`
    vals_s_flags: u64,
    mask_s_flags: u64,

    /// Pending ext2 mount option delta.
    /// Linux: `vals_s_mount_opt`, `mask_s_mount_opt`
    /// Linux: `fs/ext2/super.c:479-480`
    vals_mount_opt: u32,
    mask_mount_opt: u32,

    /// Reserved uid/gid overrides.
    /// Linux: `s_resuid`, `s_resgid`
    /// Linux: `fs/ext2/super.c:481-482`
    resuid: u32,
    resgid: u32,

    /// Superblock physical block index.
    /// Linux: `s_sb_block`
    /// Linux: `fs/ext2/super.c:483`
    sb_block: u64,

    /// Parsed-option presence bitmap.
    /// Linux: `spec` + `EXT2_SPEC_s_resuid/s_resgid`
    /// Linux: `fs/ext2/super.c:473-474`, `fs/ext2/super.c:484`
    spec: MountSpec,
}

bitflags! {
    /// Parsed mount-spec markers.
    /// Linux: `EXT2_SPEC_s_resuid`, `EXT2_SPEC_s_resgid`
    /// Linux: `fs/ext2/super.c:473-474`
    pub struct MountSpec: u32 {
        const RESUID = 1 << 0;
        const RESGID = 1 << 1;
    }
}

/// Supported mount option update op.
/// Linux intent peer: `ctx_set_mount_opt` / `ctx_clear_mount_opt`.
/// Linux: `fs/ext2/super.c:488-500`
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountOptOp {
    Set,
    Clear,
}
```

## 2) Method Signatures (No Implementation Yet)

```rust
impl FsType for Ext2Type {
    /// Filesystem type name.
    /// Linux: `.name = "ext2"`
    /// Linux: `fs/ext2/super.c:1700`
    fn name(&self) -> &'static str;

    /// Filesystem type properties.
    /// Linux: `.fs_flags = FS_REQUIRES_DEV`
    /// Linux: `fs/ext2/super.c:1702`
    fn properties(&self) -> FsProperties;

    /// Create/mount filesystem instance from parsed args and optional disk.
    /// Linux mount flow: `init_fs_context -> parse -> get_tree -> ext2_fill_super`
    /// Linux: `fs/ext2/super.c:1669-1705`, `fs/ext2/super.c:877-980`
    fn create(
        &self,
        flags: FsFlags,
        args: Option<CString>,
        disk: Option<Arc<dyn BlockDevice>>,
    ) -> Result<Arc<dyn FileSystem>>;
}

impl Ext2MountContext {
    /// Create default context for fresh mount.
    /// Linux equivalent: `ext2_init_fs_context` non-reconfigure branch.
    /// Linux: `fs/ext2/super.c:1687-1690`
    pub fn new_mount() -> Self;

    /// Create context from existing mounted fs (remount/reconfigure).
    /// Linux equivalent: `ext2_init_fs_context` reconfigure branch.
    /// Linux: `fs/ext2/super.c:1677-1686`
    pub fn from_reconfigure(sb: &SuperBlockMem) -> Self;

    /// Apply mount option set/clear updates.
    /// Linux equivalent: `ctx_set_mount_opt` / `ctx_clear_mount_opt`.
    /// Linux: `fs/ext2/super.c:488-500`
    pub fn apply_mount_opt(&mut self, flag: u32, op: MountOptOp);

    /// Parse one key-value mount parameter.
    /// Linux equivalent: `ext2_parse_param`.
    /// Linux: `fs/ext2/super.c:519-560`
    pub fn parse_param(&mut self, key: &str, value: Option<&str>) -> Result<()>;

    /// Finalize parsed options into superblock runtime mount configuration.
    /// Linux equivalent: `ext2_set_options`.
    /// Linux: `fs/ext2/super.c:822-875`
    pub fn materialize(self, sb: &mut SuperBlockMem) -> Result<()>;
}
```

## 3) Error Handling and Fallback

- Missing disk when `FsProperties::NEED_DISK` is required returns mount failure.
- Unsupported mount options fail early in parse stage.
- Mount context application never bypasses superblock feature compatibility checks.

## 4) Design Rationale

### Why keep `Ext2Type` stateless?
- Mirrors Linux `file_system_type` behavior: registration object is immutable and shared globally.

### Why explicit `Ext2MountContext` struct?
- Linux uses a separate `ext2_fs_context` staging object; preserving that separation avoids directly mutating mounted fs state during argument parsing.

### Why represent mount option operations as enum?
- Encodes set/clear intent at type level, reducing bitmask misuse and matching Linux set/clear helpers.
