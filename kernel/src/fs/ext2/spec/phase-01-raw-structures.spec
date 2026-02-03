[PROMPT]
Provide additions to `kernel/src/fs/ext2/block_group.rs` and `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
struct ext2_group_desc   → fs/ext2/ext2.h:191
struct ext2_inode        → fs/ext2/ext2.h:290
struct ext2_dir_entry_2  → fs/ext2/ext2.h:615

[RELY]
```rust
use ostd::Pod;
```

```rust
// RawSuperBlock is defined in `super_block.rs` and must be reused.
```

```rust
/// On-disk block group descriptor (32 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct RawGroupDesc {
    pub block_bitmap: u32,         // bg_block_bitmap
    pub inode_bitmap: u32,         // bg_inode_bitmap
    pub inode_table: u32,          // bg_inode_table
    pub free_blocks_count: u16,    // bg_free_blocks_count
    pub free_inodes_count: u16,    // bg_free_inodes_count
    pub used_dirs_count: u16,      // bg_used_dirs_count
    pub pad: u16,                  // bg_pad
    pub reserved: [u32; 3],        // bg_reserved
}
```

```rust
/// On-disk inode structure (128 bytes for GOOD_OLD_REV).
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct RawInode {
    pub mode: u16,                 // i_mode
    pub uid: u16,                  // i_uid (low 16 bits)
    pub size_lo: u32,              // i_size
    pub atime: u32,                // i_atime
    pub ctime: u32,                // i_ctime
    pub mtime: u32,                // i_mtime
    pub dtime: u32,                // i_dtime
    pub gid: u16,                  // i_gid (low 16 bits)
    pub links_count: u16,          // i_links_count
    pub blocks: u32,               // i_blocks (512-byte sectors)
    pub flags: u32,                // i_flags
    pub osd1: u32,                 // osd1.linux1.l_i_reserved1
    pub block: [u32; 15],          // i_block
    pub generation: u32,           // i_generation
    pub file_acl: u32,             // i_file_acl
    pub size_high: u32,            // i_dir_acl (size high)
    pub faddr: u32,                // i_faddr
    pub frag: u8,                  // osd2.linux2.l_i_frag
    pub fsize: u8,                 // osd2.linux2.l_i_fsize
    pub pad1: u16,                 // osd2.linux2.i_pad1
    pub uid_high: u16,             // osd2.linux2.l_i_uid_high
    pub gid_high: u16,             // osd2.linux2.l_i_gid_high
    pub reserved2: u32,            // osd2.linux2.l_i_reserved2
}
```

```rust
/// On-disk directory entry with file_type.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct RawDirEntry {
    pub inode: u32,                // inode
    pub rec_len: u16,              // rec_len
    pub name_len: u8,              // name_len
    pub file_type: u8,             // file_type
}
```

[GUARANTEE]
// No functions in this module; data layout only.

[SPECIFICATION]
Pre:
- All structs are used to map on-disk data and must be `#[repr(C)]` + `Pod`.

Post:
- Field order and sizes exactly match Linux ext2 on-disk layout.
- `RawGroupDesc` is exactly 32 bytes.
- `RawInode` is exactly 128 bytes for GOOD_OLD_REV layout.
- `RawDirEntry` is exactly 8 bytes (header only; name follows on disk).

Invariant:
- These structs are plain data containers without methods or logic.
- `RawSuperBlock` remains defined in `super_block.rs` and is not redefined here.
