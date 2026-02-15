[PROMPT]
Provide modifications to `kernel/src/fs/ext2/block_group.rs` and `kernel/src/fs/ext2/fs.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
read_block_bitmap           → fs/ext2/balloc.c:129
read_inode_bitmap           → fs/ext2/ialloc.c:40
ext2_valid_block_bitmap     → fs/ext2/balloc.c:71
BlockGroup (ext2_old)       → kernel/src/fs/ext2_old/block_group.rs:16
GroupMetadata (ext2_old)    → kernel/src/fs/ext2_old/block_group.rs:360

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::fs::Ext2;
```

```rust
use super::super_block::SuperBlock;
```

```rust
use crate::fs::utils::IdBitmap;
```

```rust
/// On-disk block group descriptor (32 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct RawGroupDesc { /* ... */ }
```

```rust
/// In-memory block group descriptor.
#[derive(Clone, Copy, Debug)]
pub(super) struct GroupDesc {
    pub block_bitmap: Bid,
    pub inode_bitmap: Bid,
    pub inode_table: Bid,
    pub free_blocks_count: u16,
    pub free_inodes_count: u16,
    pub used_dirs_count: u16,
}
```

```rust
/// Current BlockGroup structure (before this phase).
#[derive(Debug)]
pub struct BlockGroup {
    idx: usize,
    desc: RwMutex<Dirty<GroupDesc>>,
}
```

[NEW STRUCT]
```rust
/// Extended BlockGroup with persistent bitmap caches.
///
/// Bitmaps are loaded once during mount (BlockGroup::load) and kept in memory.
/// Alloc/free operations modify them in-place. Dirty bitmaps are written back
/// on sync_metadata.
///
/// Linux equivalent: buffer_head cache for bitmap blocks, loaded by
/// read_block_bitmap() and read_inode_bitmap().
/// Asterinas equivalent: ext2_old GroupMetadata (kernel/src/fs/ext2_old/block_group.rs:360).
#[derive(Debug)]
pub struct BlockGroup {
    /// Zero-based block group index.
    idx: usize,
    /// In-memory group descriptor with dirty tracking.
    desc: RwMutex<Dirty<GroupDesc>>,
    /// Cached block allocation bitmap. Loaded at mount, modified in-place.
    /// Dirty flag tracks whether it needs writeback.
    block_bitmap: RwMutex<Dirty<IdBitmap>>,
    /// Cached inode allocation bitmap. Loaded at mount, modified in-place.
    /// Dirty flag tracks whether it needs writeback.
    inode_bitmap: RwMutex<Dirty<IdBitmap>>,
}
```

[GUARANTEE]
```rust
impl BlockGroup {
    /// Loads a block group with persistent bitmap caches.
    ///
    /// Reads both block and inode bitmaps from disk during mount.
    /// Validates block bitmap per Linux ext2_valid_block_bitmap.
    ///
    /// # Arguments
    /// * `group_descs` - The group descriptor table segment.
    /// * `idx` - Zero-based block group index.
    /// * `sb` - Validated superblock.
    /// * `fs` - Reference to the Ext2 filesystem (for block device access).
    ///
    /// # Returns
    /// * `Ok(BlockGroup)` with loaded and validated bitmaps.
    /// * `Err(EIO)` if bitmap read fails.
    /// * `Err(EINVAL)` if bitmap validation fails.
    pub fn load(group_descs: &USegment, idx: usize, sb: &SuperBlock, fs: &Ext2) -> Result<Self>;

    /// Returns a read guard to the block bitmap.
    pub fn block_bitmap(&self) -> RwMutexReadGuard<'_, Dirty<IdBitmap>>;

    /// Returns a write guard to the block bitmap.
    pub fn block_bitmap_mut(&self) -> RwMutexWriteGuard<'_, Dirty<IdBitmap>>;

    /// Returns a read guard to the inode bitmap.
    pub fn inode_bitmap(&self) -> RwMutexReadGuard<'_, Dirty<IdBitmap>>;

    /// Returns a write guard to the inode bitmap.
    pub fn inode_bitmap_mut(&self) -> RwMutexWriteGuard<'_, Dirty<IdBitmap>>;

    /// Writes dirty bitmaps back to disk.
    ///
    /// # Arguments
    /// * `fs` - Reference to the Ext2 filesystem (for block device access).
    ///
    /// # Returns
    /// * `Ok(())` - Bitmaps synced (or were clean).
    /// * `Err(EIO)` - Disk write failure.
    pub fn sync_bitmaps(&self, fs: &Ext2) -> Result<()>;
}
```

[SPECIFICATION]
Pre (BlockGroup::load):
- `group_descs` contains valid on-disk group descriptors.
- `idx` is within `0..sb.block_groups_count()`.
- `sb` has been validated (magic, block_size == 4096).
- `fs.block_device()` is readable.

Post (BlockGroup::load: success):
- Reads `RawGroupDesc` at offset `idx * size_of::<RawGroupDesc>()` from `group_descs`.
- Converts to `GroupDesc`.
- Loads block bitmap:
  - Reads BLOCK_SIZE bytes from `desc.block_bitmap` offset on disk.
  - Computes capacity: `last_block - first_block + 1` for this group.
  - Validates per `ext2_valid_block_bitmap` (same as phase-02-block-bitmap-read).
  - Wraps in `Dirty::new(IdBitmap::from_buf(...))`.
- Loads inode bitmap:
  - Reads BLOCK_SIZE bytes from `desc.inode_bitmap` offset on disk.
  - Capacity = `sb.inodes_per_group()`.
  - Wraps in `Dirty::new(IdBitmap::from_buf(...))`.
- Returns `BlockGroup` with all fields initialized.

Post (BlockGroup::load: failure):
- `Err(EIO)` if block device read fails for either bitmap.
- `Err(EINVAL)` if block bitmap validation fails.
- `Err(EINVAL)` if bitmap capacity exceeds `IdBitmap::capacity()`.

Pre (sync_bitmaps):
- `self` is a valid BlockGroup with loaded bitmaps.
- `fs.block_device()` is writable.

Post (sync_bitmaps: success):
- For block bitmap:
  - If `self.block_bitmap` is dirty, writes its raw bytes to
    `desc.block_bitmap` offset on disk via `fs.block_device().write_bytes()`.
  - Clears dirty flag after successful write.
- For inode bitmap:
  - If `self.inode_bitmap` is dirty, writes its raw bytes to
    `desc.inode_bitmap` offset on disk via `fs.block_device().write_bytes()`.
  - Clears dirty flag after successful write.
- If neither bitmap is dirty, returns `Ok(())` immediately.

Post (sync_bitmaps: failure):
- `Err(EIO)` if block device write fails.
- Dirty flag remains set on failure (retry possible).

Invariant:
- Bitmaps are loaded once at mount time and kept in memory for the filesystem's lifetime.
- All alloc/free operations modify the in-memory bitmap directly (via write guard).
- Dirty tracking ensures only modified bitmaps are written back on sync.
- Bitmap bit numbering uses little-endian (Lsb0) order, matching Linux `ext2_test_bit`.
- `block_bitmap` and `inode_bitmap` are protected by separate `RwMutex` locks,
  allowing concurrent read access from different allocation paths.

[DIFF]
Linux: Bitmaps are loaded on-demand via `read_block_bitmap()` / `read_inode_bitmap()`
  into buffer_head cache. The buffer_head may be evicted and re-read later.
  → Asterinas: Bitmaps are loaded eagerly at mount time and kept in memory permanently.
  Reason: Simplifies allocation paths; no need for on-demand loading or eviction handling.
  Matches ext2_old pattern (GroupMetadata holds bitmaps for filesystem lifetime).

Linux: Bitmap modifications use `ext2_set_bit` / `ext2_clear_bit` on buffer_head data,
  then `mark_buffer_dirty(bh)` for deferred writeback.
  → Asterinas: Bitmap modifications use `IdBitmap` methods under `RwMutex` write guard.
  `Dirty<IdBitmap>` wrapper tracks modification state for writeback.
  Reason: Rust ownership model; `Dirty` wrapper replaces buffer_head dirty flag.

Linux: Current new ext2 `load_block_bitmap` / `load_inode_bitmap` are stateless methods
  that re-read from disk on every call.
  → Asterinas (this phase): Bitmaps are loaded once in `BlockGroup::load` and cached
  as fields. Callers access them via `block_bitmap()` / `inode_bitmap()` guards.
  Reason: Eliminates redundant disk I/O on every alloc/free operation.
