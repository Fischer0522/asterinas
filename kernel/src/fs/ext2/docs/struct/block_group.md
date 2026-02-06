# Phase 1 - Core Struct Design (`block_group.rs`)

Target implementation file: `kernel/src/fs/ext2/block_group.rs`

## 1) Structure Definition

```rust
/// Ext2 block group runtime state.
///
/// # Linux Reference
/// - Source: `fs/ext2/ext2.h:191-201`
/// - Corresponds to: `struct ext2_group_desc`
/// - Validation flow: `fs/ext2/super.c:695-731` (`ext2_check_descriptors`)
///
/// # Concurrency
/// - Lock: `desc: RwMutex<Dirty<GroupDesc>>`
/// - Ordering: `SuperBlock -> BlockGroup -> inode_cache -> inode -> bitmap`
/// - Linux lock intent peer: per-group lock indirection via `sb_bgl_lock`.
///   Linux: `fs/ext2/ext2.h:102-124`
///
/// # Caching
/// - Cached fields: group free counters + descriptor block pointers
/// - Authoritative source: group descriptor table on disk
///   (`fs/ext2/ext2.h:191-201`)
#[derive(Debug)]
pub struct BlockGroup {
    /// Group index in filesystem.
    /// Linux: logical `block_group` index used across balloc/ialloc paths.
    /// Linux: `fs/ext2/balloc.c:82-83`, `fs/ext2/ialloc.c:126-129`
    idx: usize,

    /// Mutable descriptor with dirty tracking.
    /// Linux peer: gdp updates in alloc/free paths.
    /// Linux: `fs/ext2/balloc.c:525-535`, `fs/ext2/ialloc.c:137-144`
    desc: RwMutex<Dirty<GroupDesc>>,
}

/// On-disk block group descriptor layout (32 bytes).
///
/// # Linux Reference
/// - Source: `fs/ext2/ext2.h:191-201`
/// - Corresponds to: `struct ext2_group_desc`
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct RawGroupDesc {
    /// `bg_block_bitmap`
    /// Linux: `fs/ext2/ext2.h:193`
    pub block_bitmap: u32,
    /// `bg_inode_bitmap`
    /// Linux: `fs/ext2/ext2.h:194`
    pub inode_bitmap: u32,
    /// `bg_inode_table`
    /// Linux: `fs/ext2/ext2.h:195`
    pub inode_table: u32,
    /// `bg_free_blocks_count`
    /// Linux: `fs/ext2/ext2.h:196`
    pub free_blocks_count: u16,
    /// `bg_free_inodes_count`
    /// Linux: `fs/ext2/ext2.h:197`
    pub free_inodes_count: u16,
    /// `bg_used_dirs_count`
    /// Linux: `fs/ext2/ext2.h:198`
    pub used_dirs_count: u16,
    /// `bg_pad`
    /// Linux: `fs/ext2/ext2.h:199`
    pub pad: u16,
    /// `bg_reserved[3]`
    /// Linux: `fs/ext2/ext2.h:200`
    pub reserved: [u32; 3],
}

/// Decoded in-memory group descriptor.
#[derive(Clone, Copy, Debug)]
pub struct GroupDesc {
    /// Block-id of block bitmap.
    /// Linux: `bg_block_bitmap` (`fs/ext2/ext2.h:193`)
    pub block_bitmap: Bid,
    /// Block-id of inode bitmap.
    /// Linux: `bg_inode_bitmap` (`fs/ext2/ext2.h:194`)
    pub inode_bitmap: Bid,
    /// First block-id of inode table.
    /// Linux: `bg_inode_table` (`fs/ext2/ext2.h:195`)
    pub inode_table: Bid,
    /// Cached free blocks count.
    /// Linux: `bg_free_blocks_count` (`fs/ext2/ext2.h:196`)
    pub free_blocks_count: u16,
    /// Cached free inodes count.
    /// Linux: `bg_free_inodes_count` (`fs/ext2/ext2.h:197`)
    pub free_inodes_count: u16,
    /// Cached used directory count.
    /// Linux: `bg_used_dirs_count` (`fs/ext2/ext2.h:198`)
    pub used_dirs_count: u16,
}

/// Group bitmap cache (Asterinas adaptation).
/// Linux intent source: `read_block_bitmap` / `read_inode_bitmap` buffer-head cache.
/// Linux: `fs/ext2/balloc.c:129`, `fs/ext2/ialloc.c:47`
#[derive(Debug)]
pub struct GroupBitmapState {
    /// Data-block allocation bitmap.
    pub block_bitmap: IdBitmap,
    /// Inode allocation bitmap.
    pub inode_bitmap: IdBitmap,
}
```

## 2) Method Signatures (No Implementation Yet)

```rust
impl BlockGroup {
    /// Load and decode descriptor by index.
    /// Linux equivalent: descriptor lookup in `ext2_get_group_desc`.
    /// Linux: `fs/ext2/balloc.c:39`
    pub fn load(group_descs: &USegment, idx: usize) -> Result<Self>;

    /// Write back dirty group descriptor.
    pub fn sync_metadata(&self, group_descs: &USegment) -> Result<()>;

    /// Load and verify block bitmap against descriptor invariants.
    /// Linux equivalent: `read_block_bitmap()` + `ext2_valid_block_bitmap()`.
    /// Linux: `fs/ext2/balloc.c:71-160`
    pub fn load_block_bitmap(&self, fs: &Ext2, sb: &SuperBlock) -> Result<IdBitmap>;

    /// Load inode bitmap for this group.
    /// Linux equivalent: `read_inode_bitmap()`.
    /// Linux: `fs/ext2/ialloc.c:47-68`
    pub fn load_inode_bitmap(&self, fs: &Ext2, sb: &SuperBlock) -> Result<IdBitmap>;

    /// Decrement free-block counter after allocation.
    /// Linux equivalent: counter updates in `ext2_new_blocks`.
    /// Linux: `fs/ext2/balloc.c:1208-1410`
    pub fn dec_free_blocks(&self, count: u16);

    /// Increment free-block counter after free.
    /// Linux equivalent: `ext2_free_blocks`.
    /// Linux: `fs/ext2/balloc.c:482-612`
    pub fn inc_free_blocks(&self, count: u16);

    /// Decrement free-inode counter after allocation.
    /// Linux equivalent: `ext2_new_inode`.
    /// Linux: `fs/ext2/ialloc.c:419-591`
    pub fn dec_free_inodes(&self, count: u16);

    /// Increment free-inode counter after free.
    /// Linux equivalent: `ext2_free_inode`.
    /// Linux: `fs/ext2/ialloc.c:105-192`
    pub fn inc_free_inodes(&self, count: u16);
}

impl GroupDesc {
    /// Validate descriptor boundaries against group range.
    /// Linux equivalent: `ext2_check_descriptors`.
    /// Linux: `fs/ext2/super.c:695-731`
    pub fn validate_bounds(&self, sb: &SuperBlock, group_idx: usize) -> Result<()>;

    /// Validate bitmap bits for metadata-reserved blocks.
    /// Linux equivalent: `ext2_valid_block_bitmap`.
    /// Linux: `fs/ext2/balloc.c:71-124`
    pub fn validate_bitmap_consistency(&self, sb: &SuperBlock, bitmap: &IdBitmap) -> Result<()>;
}
```

## 3) Corruption Policy

- If descriptor block pointers are outside group range, treat as corruption (`EINVAL`/`EUCLEAN`) and abort mount or force readonly.
- If metadata-reserved blocks (block bitmap/inode bitmap/inode table) are not marked allocated in bitmap, treat bitmap as invalid and block allocation.
- Counter underflow/overflow is prevented by checked updates and escalated to filesystem error handler when invariants are violated.

## 4) Design Rationale

### Why `RwMutex<Dirty<GroupDesc>>`?
- Linux updates group descriptor counters under lock and writes them back; this directly maps to a guarded dirty object.

### Why split raw and decoded descriptor (`RawGroupDesc` vs `GroupDesc`)?
- Keeps on-disk ABI exact while allowing typed block-ids (`Bid`) and safer arithmetic in memory.

### Why bitmap validation at load time?
- Linux performs strict metadata-bit consistency checks (`ext2_valid_block_bitmap`); Stage 1 preserves this as non-negotiable logic truth.
