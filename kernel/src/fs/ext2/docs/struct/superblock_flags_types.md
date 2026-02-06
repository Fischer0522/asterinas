# Phase 1 - Supplementary Struct Design (Superblock flags/types)

Target implementation file: `kernel/src/fs/ext2/super_block.rs`

## 1) Structure Definition

```rust
bitflags! {
    /// Compatible feature set from superblock.
    /// Linux: `EXT2_FEATURE_COMPAT_*`
    /// Linux: `fs/ext2/ext2.h:526-533`
    pub struct FeatureCompatSet: u32 {
        const DIR_PREALLOC = 1 << 0;
        const IMAGIC_INODES = 1 << 1;
        const HAS_JOURNAL = 1 << 2;
        const EXT_ATTR = 1 << 3;
        const RESIZE_INO = 1 << 4;
        const DIR_INDEX = 1 << 5;
    }
}

bitflags! {
    /// Incompatible feature set (must be fully supported to mount).
    /// Linux: `EXT2_FEATURE_INCOMPAT_*`
    /// Linux: `fs/ext2/ext2.h:539-544`
    pub struct FeatureInCompatSet: u32 {
        const COMPRESSION = 1 << 0;
        const FILETYPE = 1 << 1;
        const RECOVER = 1 << 2;
        const JOURNAL_DEV = 1 << 3;
        const META_BG = 1 << 4;
    }
}

bitflags! {
    /// Readonly-compatible feature set.
    /// Linux: `EXT2_FEATURE_RO_COMPAT_*`
    /// Linux: `fs/ext2/ext2.h:534-537`
    pub struct FeatureRoCompatSet: u32 {
        const SPARSE_SUPER = 1 << 0;
        const LARGE_FILE = 1 << 1;
        const BTREE_DIR = 1 << 2;
    }
}

bitflags! {
    /// Filesystem state flags from `s_state`.
    /// Linux: `EXT2_VALID_FS`, `EXT2_ERROR_FS`
    /// Linux: `fs/ext2/ext2.h:358-360`
    pub struct FsState: u16 {
        const VALID = 1 << 0;
        const ERROR = 1 << 1;
    }
}

/// Error policy from `s_errors`.
/// Linux: `s_errors` behavior wiring in `ext2_error`.
/// Linux: `fs/ext2/ext2.h:429`, `fs/ext2/super.c:75-81`
#[repr(u16)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ErrorsBehaviour {
    Continue = 1,
    RemountReadonly = 2,
    Panic = 3,
}

/// Creator OS id from superblock.
/// Linux: `EXT2_OS_*`
/// Linux: `fs/ext2/ext2.h:486-490`
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum OsId {
    Linux = 0,
    Hurd = 1,
    Masix = 2,
    FreeBSD = 3,
    Lites = 4,
}

/// On-disk revision level.
/// Linux: `EXT2_GOOD_OLD_REV`, `EXT2_DYNAMIC_REV`
/// Linux: `fs/ext2/ext2.h:495-500`
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RevLevel {
    GoodOld = 0,
    Dynamic = 1,
}
```

## 2) Method Signatures (No Implementation Yet)

```rust
impl SuperBlock {
    /// Validate incompatible features for mount acceptance.
    /// Linux equivalent: incompat feature rejection in `ext2_fill_super`.
    /// Linux: `fs/ext2/super.c:971-977`
    pub fn validate_incompat_features(&self) -> Result<()>;

    /// Validate readonly-compatible features for rw mount.
    /// Linux equivalent: ro-compat check in `ext2_fill_super`.
    /// Linux: `fs/ext2/super.c:978-980`
    pub fn validate_ro_compat_for_rw(&self, readonly_mount: bool) -> Result<()>;

    /// Apply filesystem error policy transition.
    /// Linux equivalent: `ext2_error` handling.
    /// Linux: `fs/ext2/super.c:49-82`
    pub fn on_fs_error(&mut self, policy: ErrorsBehaviour) -> Result<()>;

    /// Determine whether sparse-super policy is active.
    /// Linux equivalent: `EXT2_HAS_RO_COMPAT_FEATURE(...SPARSE_SUPER)`.
    /// Linux: `fs/ext2/balloc.c:1516-1518`
    pub fn has_sparse_super(&self) -> bool;
}
```

## 3) Policy Notes

- Unsupported `FeatureInCompatSet` bits must fail mount immediately.
- Unsupported `FeatureRoCompatSet` bits may allow readonly mount but must reject read-write mount.
- `ErrorsBehaviour::RemountReadonly` transitions mount mode without dropping read-only service.

## 4) Design Rationale

### Why keep feature sets as dedicated bitflags instead of raw integers?
- Makes compatibility policy explicit and statically type-checked.

### Why separate `ErrorsBehaviour` from `FsState`?
- `FsState` reflects current filesystem health bits; `ErrorsBehaviour` controls transition policy when errors occur.
