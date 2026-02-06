# Phase 1 - Supplementary Struct Design (Inode/Dir flags & type maps)

Target implementation files:
- `kernel/src/fs/ext2/inode.rs`
- `kernel/src/fs/ext2/dir.rs`

## 1) Structure Definition

```rust
/// File permission mode wrapper.
/// Linux source field: `ext2_inode.i_mode`.
/// Linux: `fs/ext2/ext2.h:291`
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilePerm(pub u16);

bitflags! {
    /// Inode flags (`i_flags`) mirroring ext2 semantics.
    /// Linux: `EXT2_*_FL` definitions.
    /// Linux: `fs/ext2/ext2.h:223-244`
    pub struct FileFlags: u32 {
        const SECURE_DEL = 1 << 0;
        const UNDELETE = 1 << 1;
        const COMPRESS = 1 << 2;
        const SYNC_UPDATE = 1 << 3;
        const IMMUTABLE = 1 << 4;
        const APPEND_ONLY = 1 << 5;
        const NO_DUMP = 1 << 6;
        const NO_ATIME = 1 << 7;
        const DIRTY = 1 << 8;
        const COMPRESS_BLK = 1 << 9;
        const NO_COMPRESS = 1 << 10;
        const ENCRYPT = 1 << 11;
        const INDEX_DIR = 1 << 12;
        const IMAGIC = 1 << 13;
        const JOURNAL_DATA = 1 << 14;
        const NO_TAIL = 1 << 15;
        const DIR_SYNC = 1 << 16;
        const TOP_DIR = 1 << 17;
        const RESERVED = 1 << 31;
    }
}

/// Directory entry file-type byte mapping.
/// Linux source field: `ext2_dir_entry_2.file_type`.
/// Linux: `fs/ext2/ext2.h:592-597`
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DirEntryFileType {
    Unknown = 0,
    File = 1,
    Dir = 2,
    Char = 3,
    Block = 4,
    Fifo = 5,
    Socket = 6,
    Symlink = 7,
}

/// Inode flag mask category for type-dependent filtering.
/// Linux equivalent: `ext2_mask_flags` masks.
/// Linux: `fs/ext2/ext2.h:256-270`
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileFlagMaskKind {
    Directory,
    Regular,
    Other,
}
```

## 2) Method Signatures (No Implementation Yet)

```rust
impl FilePerm {
    /// Construct from raw mode bits.
    pub fn from_bits_truncate(bits: u16) -> Self;

    /// Return raw mode bits.
    pub fn bits(self) -> u16;
}

impl FileFlags {
    /// Mask flags according to inode type.
    /// Linux equivalent: `ext2_mask_flags`.
    /// Linux: `fs/ext2/ext2.h:263-270`
    pub fn mask_for_type(self, kind: FileFlagMaskKind) -> Self;

    /// Return flags inherited by newly created child inode.
    /// Linux equivalent: `EXT2_FL_INHERITED`.
    /// Linux: `fs/ext2/ext2.h:249-255`
    pub fn inherited_parent_flags(self) -> Self;

    /// Validate user-visible/modifiable constraints.
    /// Linux equivalent intent: `EXT2_FL_USER_VISIBLE`/`EXT2_FL_USER_MODIFIABLE`.
    /// Linux: `fs/ext2/ext2.h:246-247`
    pub fn validate_user_update(old: Self, new: Self) -> Result<Self>;
}

impl DirEntryFileType {
    /// Decode ext2 directory file_type byte.
    pub fn from_u8(value: u8) -> Self;

    /// Convert to VFS inode type.
    pub fn to_inode_type(self) -> InodeType;
}

impl Inode {
    /// Apply inode flag bits onto VFS inode runtime flags.
    /// Linux equivalent: `ext2_set_inode_flags`.
    /// Linux: `fs/ext2/inode.c:1357-1375`
    pub fn apply_runtime_flags(&self) -> Result<()>;
}
```

## 3) Compatibility Notes

- `DirEntryFileType` is only authoritative when FILETYPE incompat feature is enabled; otherwise file type must be inferred from inode.
- Flag inheritance and masking must remain type-aware (dir vs regular vs other) to match Linux behavior.

## 4) Design Rationale

### Why preserve explicit file-type enum for directory entries?
- Keeps on-disk encoding stable and makes parser behavior explicit under feature gating.

### Why have mask-kind abstraction?
- Encodes Linux’s three-branch masking logic as a typed rule, reducing subtle flag bugs.
