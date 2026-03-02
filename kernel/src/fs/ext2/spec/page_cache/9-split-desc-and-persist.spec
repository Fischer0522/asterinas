[PROMPT]
Phase 1 (foundations): introduce split inode descriptors for metadata vs block
mapping, plus conversion helpers, so later phases can split locks without
maintaining dual-write copies of `i_block[15]`.

Provide modifications to `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
struct ext2_inode        → fs/ext2/ext2.h:290
ext2_iget                → fs/ext2/inode.c:1493-1500
__ext2_write_inode       → fs/ext2/inode.c:1589-1599
ext2_setsize             → fs/ext2/inode.c:1275

[RELY]
```rust
use super::prelude::*;
use super::utils::Dirty;
```

```rust
/// On-disk inode structure (GOOD_OLD_REV).
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct RawInode {
    pub mode: u16,        // i_mode
    pub uid: u16,         // i_uid (low 16 bits)
    pub size_lo: u32,     // i_size
    pub atime: u32,       // i_atime
    pub ctime: u32,       // i_ctime
    pub mtime: u32,       // i_mtime
    pub dtime: u32,       // i_dtime
    pub gid: u16,         // i_gid (low 16 bits)
    pub links_count: u16, // i_links_count
    pub blocks: u32,      // i_blocks (512-byte sectors)
    pub flags: u32,       // i_flags
    pub osd1: u32,        // osd1.linux1.l_i_reserved1
    pub block: [u32; 15], // i_block
    pub generation: u32,  // i_generation
    pub file_acl: u32,    // i_file_acl
    pub size_high: u32,   // i_dir_acl (size high)
    pub faddr: u32,       // i_faddr
    pub frag: u8,         // osd2.linux2.l_i_frag
    pub fsize: u8,        // osd2.linux2.l_i_fsize
    pub pad1: u16,        // osd2.linux2.i_pad1
    pub uid_high: u16,    // osd2.linux2.l_i_uid_high
    pub gid_high: u16,    // osd2.linux2.l_i_gid_high
    pub reserved2: u32,   // osd2.linux2.l_i_reserved2
}
```

```rust
/// Current combined in-memory descriptor (will be split over phases).
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeDesc {
    pub type_: InodeType,
    pub perm: FilePerm,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: Duration,
    pub ctime: Duration,
    pub mtime: Duration,
    pub dtime: Duration,
    pub links_count: u16,
    pub blocks: u32,
    pub flags: FileFlags,
    pub block_ptrs: [u32; 15],
    pub file_acl: u32,
    pub generation: u32,
}
```

[GUARANTEE]

```rust
/// In-memory inode metadata (raw on-disk view excluding i_blocks/i_block[]).
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeMetaDesc {
    pub type_: InodeType,      // Inode type decoded from i_mode
    pub perm: FilePerm,        // Permission bits (no file type)
    pub uid: u32,              // Owner uid
    pub gid: u32,              // Owner gid
    pub size: u64,             // File size in bytes
    pub atime: Duration,       // Access time
    pub ctime: Duration,       // Status change time
    pub mtime: Duration,       // Modification time
    pub dtime: Duration,       // Deletion time
    pub links_count: u16,      // Hard link count
    pub flags: FileFlags,      // Inode flags
    pub file_acl: u32,         // i_file_acl (xattr/acl block)
    pub generation: u32,       // Generation
}

/// In-memory inode block mapping (raw on-disk view of i_blocks + i_block[]).
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeMappingDesc {
    pub blocks: u32,           // i_blocks (512-byte sectors)
    pub block_ptrs: [u32; 15], // i_block[15]
}
```

```rust
impl InodeMetaDesc {
    /// Parses meta fields from an on-disk inode.
    ///
    /// # Returns
    /// - `Ok(meta)` on success.
    /// - `Err(ESTALE)` if inode is deleted/unlinked per ext2 on-disk markers.
    /// - `Err(EIO)` for invalid flags or unrecoverable format violations.
    /// - `Err(EUCLEAN)` for corrupted values (e.g., size overflow).
    pub(super) fn try_from_raw(raw: &RawInode) -> Result<InodeMetaDesc>;

    /// Splits a combined descriptor into meta-only descriptor.
    ///
    /// This is a temporary migration helper until `InodeDesc` is removed.
    pub(super) fn from_desc(desc: &InodeDesc) -> InodeMetaDesc;
}

impl InodeMappingDesc {
    /// Parses mapping fields from an on-disk inode.
    pub(super) fn from_raw(raw: &RawInode) -> InodeMappingDesc;

    /// Splits a combined descriptor into mapping-only descriptor.
    ///
    /// This is a temporary migration helper until `InodeDesc` is removed.
    pub(super) fn from_desc(desc: &InodeDesc) -> InodeMappingDesc;
}

impl RawInode {
    /// Assembles an on-disk inode from split descriptors.
    pub(super) fn from_parts(meta: &InodeMetaDesc, mapping: &InodeMappingDesc) -> RawInode;
}
```

[SPECIFICATION]

## Invariants

- The split descriptors must be a lossless decomposition of the on-disk inode:
  - `InodeMetaDesc` contains every persisted field except `i_blocks` and `i_block[15]`.
  - `InodeMappingDesc` contains exactly `i_blocks` and `i_block[15]`.
- `RawInode::from_parts(meta, mapping)` must preserve all fields represented by
  `meta` and `mapping` without implicit mutation.

## Error behavior

- `InodeMetaDesc::try_from_raw` must preserve the current ext2 inode loading
  behavior (same error codes and conditions as existing combined parsing).

[DIFF]

Linux: In-kernel inode caches separate in-memory `struct inode` fields from the
  on-disk `struct ext2_inode` and writes back via `__ext2_write_inode`.
  → Asterinas: Introduces explicit split descriptors as an intermediate step to
    enable split locks without dual-write mapping copies.

[TEST]

## InodeMetaDesc::try_from_raw
- Valid raw inode → returns Ok with correct uid/gid/perm/times/flags/size.
- Deleted inode markers → returns Err(ESTALE).
- Invalid flags → returns Err(EIO).
- Size overflow/corruption → returns Err(EUCLEAN).

## InodeMappingDesc::from_raw
- Copies blocks and block pointers exactly from RawInode.

## RawInode::from_parts
- Roundtrip: raw → (meta,mapping) → raw' produces raw' matching raw for all
  fields represented by descriptors.
