[PROMPT]
Provide modifications to `kernel/src/fs/ext2/block_group.rs` and `kernel/src/fs/ext2/fs.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_get_inode       → fs/ext2/inode.c:1314
__ext2_write_inode   → fs/ext2/inode.c:1512
sb_bread             → (block cache read, replaced by PageCache)
mark_buffer_dirty    → (block cache dirty, replaced by PageCache dirty tracking)
BlockGroupImpl (ext2_old) → kernel/src/fs/ext2_old/block_group.rs:16

[RELY]
```rust
use core::mem::size_of;
```

```rust
use super::prelude::*;
```

```rust
use super::fs::Ext2;
```

```rust
use super::inode::{InodeDesc, RawInode};
```

```rust
use super::super_block::SuperBlock;
```

```rust
use super::utils::Dirty;
```

```rust
use crate::fs::utils::IdBitmap;
```

```rust
/// On-disk block group descriptor (32 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct RawGroupDesc {
    pub block_bitmap: u32,
    pub inode_bitmap: u32,
    pub inode_table: u32,
    pub free_blocks_count: u16,
    pub free_inodes_count: u16,
    pub used_dirs_count: u16,
    pub pad: u16,
    pub reserved: [u32; 3],
}
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
/// PageCache infrastructure.
pub struct PageCache { /* ... */ }
pub trait PageCacheBackend: Sync + Send {
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;
    fn write_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;
    fn npages(&self) -> usize;
}
```

```rust
/// The Ext2 filesystem (core state holder).
#[derive(Debug)]
pub struct Ext2 {
    block_device: Arc<dyn BlockDevice>,
    super_block: RwMutex<Dirty<SuperBlock>>,
    block_groups: Vec<BlockGroup>,
    inodes_per_group: u32,
    blocks_per_group: u32,
    inode_size: usize,
    block_size: usize,
    group_descriptors_segment: USegment,
    fs_event_subscriber_stats: FsEventSubscriberStats,
    self_ref: Weak<Ext2>,
}
```

[NEW STRUCT]
```rust
/// Internal implementation backing the inode table PageCache for a block group.
///
/// Holds the physical inode table location and size so that PageCacheBackend
/// can translate page indices to block device I/O. Shared via Arc between
/// BlockGroup and its PageCache.
///
/// Linux equivalent: the buffer_head layer used by sb_bread() in ext2_get_inode().
/// Asterinas equivalent: ext2_old BlockGroupImpl (kernel/src/fs/ext2_old/block_group.rs:22).
struct InodeTableBackend {
    /// Physical block ID of the first block of this group's inode table.
    /// Corresponds to bg_inode_table in the group descriptor.
    inode_table_bid: Bid,
    /// Total size in bytes of the inode table for this group.
    /// Equals inodes_per_group * inode_size.
    raw_inodes_size: usize,
    /// Weak reference to the filesystem for block device access.
    fs: Weak<Ext2>,
}
```

[GUARANTEE]
```rust
impl BlockGroup {
    /// Loads a block group with its inode table PageCache.
    ///
    /// # Arguments
    /// * `group_descs` - The group descriptor table segment.
    /// * `idx` - Zero-based block group index.
    /// * `sb` - Validated superblock.
    /// * `fs` - Weak reference to the owning Ext2 filesystem.
    ///
    /// # Returns
    /// * `Ok(BlockGroup)` with initialized inode table cache.
    /// * `Err(EIO)` if group descriptor cannot be read.
    pub fn load(group_descs: &USegment, idx: usize, sb: &SuperBlock, fs: Weak<Ext2>) -> Result<Self>;

    /// Returns the inode table PageCache for this group.
    pub fn inode_table_cache(&self) -> &PageCache;
}
```

```rust
impl PageCacheBackend for InodeTableBackend {
    /// Reads one page (= one block, since block_size == PAGE_SIZE) from the
    /// inode table into the provided cache page frame.
    ///
    /// # Arguments
    /// * `idx` - Page index within the inode table (0-based).
    /// * `frame` - Target CachePage to fill with data from disk.
    ///
    /// Translates: page idx → physical block = inode_table_bid + idx.
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;

    /// Writes one page from the cache page frame back to the inode table on disk.
    ///
    /// # Arguments
    /// * `idx` - Page index within the inode table (0-based).
    /// * `frame` - Source CachePage containing dirty data to write.
    fn write_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter>;

    /// Returns the number of pages (blocks) in this group's inode table.
    fn npages(&self) -> usize;
}
```

```rust
impl Ext2 {
    /// Reads an inode descriptor from the inode table via PageCache.
    ///
    /// Replaces direct block device I/O with cached page access.
    /// Linux equivalent: ext2_get_inode() using sb_bread().
    ///
    /// # Arguments
    /// * `ino` - 1-based inode number.
    ///
    /// # Returns
    /// * `Ok(InodeDesc)` - Parsed inode descriptor.
    /// * `Err(EINVAL)` - Invalid inode number.
    /// * `Err(EIO)` - I/O failure reading inode table page.
    /// * `Err(ESTALE)` - Inode is deleted.
    pub(super) fn read_inode_desc(&self, ino: u32) -> Result<InodeDesc>;

    /// Writes an inode descriptor back to the inode table via PageCache.
    ///
    /// Reads the cached page, modifies the RawInode at the correct offset,
    /// and marks the page dirty. The actual disk write happens on cache eviction/sync.
    /// Linux equivalent: __ext2_write_inode() using ext2_get_inode() + mark_buffer_dirty().
    ///
    /// # Arguments
    /// * `ino` - 1-based inode number.
    /// * `raw` - The RawInode to write.
    ///
    /// # Returns
    /// * `Ok(())` - Inode written to cache successfully.
    /// * `Err(EINVAL)` - Invalid inode number.
    /// * `Err(EIO)` - I/O failure accessing inode table page.
    pub(super) fn write_inode_desc(&self, ino: u32, raw: &RawInode) -> Result<()>;
}
```

[SPECIFICATION]
Pre (BlockGroup::load):
- `group_descs` contains valid on-disk group descriptors.
- `idx` is within `0..sb.block_groups_count()`.
- `sb` has been validated (magic, block_size == 4096).
- `fs` is a valid weak reference to the Ext2 instance being constructed.

Post (BlockGroup::load: success):
- Reads `RawGroupDesc` at offset `idx * size_of::<RawGroupDesc>()` from `group_descs`.
- Converts to `GroupDesc`.
- Creates `InodeTableBackend` with:
  - `inode_table_bid = desc.inode_table` (as Bid).
  - `raw_inodes_size = sb.inodes_per_group() * sb.inode_size()`.
  - `fs` = cloned weak reference.
- Wraps backend in `Arc`, creates `PageCache::with_capacity(raw_inodes_size, backend)`.
- Returns `BlockGroup` with desc, idx, and inode_table_cache.

Post (BlockGroup::load: failure):
- `Err(EIO)` if descriptor read fails.

Pre (InodeTableBackend::read_page_async):
- `idx < self.npages()`.
- `frame` is a valid allocated CachePage.

Post (read_page_async: success):
- Computes physical block: `bid = self.inode_table_bid + idx as u64`.
- Creates `BioSegment` from frame with `BioDirection::FromDevice`.
- Submits async read via `fs.block_device().read_blocks_async(bid, bio_segment)`.
- Returns the `BioWaiter`.

Post (read_page_async: failure):
- `Err(EIO)` if filesystem reference is dropped or block device read fails.

Pre (InodeTableBackend::write_page_async):
- `idx < self.npages()`.
- `frame` contains dirty inode table data.

Post (write_page_async: success):
- Same physical block computation as read.
- Creates `BioSegment` from frame with `BioDirection::ToDevice`.
- Submits async write via `fs.block_device().write_blocks_async(bid, bio_segment)`.

Post (write_page_async: failure):
- `Err(EIO)` if filesystem reference is dropped or block device write fails.

Pre (npages):
- (none)

Post (npages):
- Returns `self.raw_inodes_size.div_ceil(BLOCK_SIZE)`.

Pre (read_inode_desc):
- `self.super_block` has been validated.
- `ino` is a 1-based inode number.

Post (read_inode_desc: success):
- Validates inode number (same as phase-03-inode-table-io):
  - `(ino != ROOT_INO && ino < sb.first_ino())` → `Err(EINVAL)`.
  - `ino > sb.total_inodes()` → `Err(EINVAL)`.
- Computes location:
  - `group_idx = (ino - 1) / sb.inodes_per_group()`.
  - `index_in_group = (ino - 1) % sb.inodes_per_group()`.
  - `offset_bytes = index_in_group * sb.inode_size()`.
- Reads `RawInode` via PageCache:
  - `self.block_groups[group_idx].inode_table_cache().pages().read_val::<RawInode>(offset_bytes)`.
  - PageCache automatically triggers `read_page_async` on cache miss.
- Returns `InodeDesc::try_from(&raw)`.

Post (read_inode_desc: failure):
- `Err(EINVAL)` on invalid inode number.
- `Err(EIO)` on PageCache read failure.
- Propagates `Err(ESTALE)` from `InodeDesc::try_from` for deleted inodes.

Pre (write_inode_desc):
- `ino` is a valid 1-based inode number.
- `raw` contains the updated RawInode to persist.

Post (write_inode_desc: success):
- Same inode number validation and offset computation as `read_inode_desc`.
- Writes `RawInode` via PageCache:
  - `self.block_groups[group_idx].inode_table_cache().pages().write_val(offset_bytes, raw)`.
  - PageCache marks the page dirty; actual disk write deferred to eviction/sync.
- Returns `Ok(())`.

Post (write_inode_desc: failure):
- `Err(EINVAL)` on invalid inode number.
- `Err(EIO)` on PageCache write failure.

Invariant:
- Inode table I/O always goes through PageCache, never direct block device access.
- Multiple inodes sharing the same inode table block share the same cached page.
- Dirty pages are written back on eviction or explicit sync.
- Address arithmetic matches Linux `ext2_get_inode` exactly.

[DIFF]
Linux: Uses `sb_bread(sb, block)` to read inode table blocks into buffer_head cache.
  `ext2_get_inode` returns a pointer into the buffer_head data.
  → Asterinas: Uses per-BlockGroup `PageCache` with `InodeTableBackend`.
  `read_inode_desc` reads via `PageCache::pages().read_val()`.
  Reason: No buffer_head in Asterinas; PageCache provides equivalent block caching.

Linux: `__ext2_write_inode` modifies raw_inode in-place in buffer_head, then calls
  `mark_buffer_dirty(bh)` for deferred writeback.
  → Asterinas: `write_inode_desc` writes via `PageCache::pages().write_val()`,
  which triggers `update_page` → marks CachePage as Dirty.
  Reason: PageCache dirty tracking replaces buffer_head dirty mechanism.

Linux: `brelse(bh)` releases buffer_head reference after use.
  → Asterinas: PageCache manages page lifetime via LRU eviction.
  Reason: Rust ownership model; no manual reference counting needed.

Linux: BlockGroup::load in ext2_old takes `block_device` and `fs: Weak<Ext2>` parameters
  to construct PageCache during filesystem mount.
  → Asterinas (new): BlockGroup::load takes `sb` and `fs` parameters.
  The `InodeTableBackend` accesses block device through `fs.block_device()`.
  Reason: Cleaner dependency; block device access is centralized through Ext2.
