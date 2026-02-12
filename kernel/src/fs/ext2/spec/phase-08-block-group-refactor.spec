[PROMPT]
Refactor `kernel/src/fs/ext2/block_group.rs` and `kernel/src/fs/ext2/fs.rs`.
Move per-group operations (block alloc/free, inode alloc/free, inode desc read/write,
system zone checks) from `Ext2` into `BlockGroup`. `Ext2` retains cross-group
orchestration only. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_new_blocks             → /root/linux/fs/ext2/balloc.c:1208
ext2_free_blocks            → /root/linux/fs/ext2/balloc.c:482
ext2_try_to_allocate        → /root/linux/fs/ext2/balloc.c:682
ext2_new_inode              → /root/linux/fs/ext2/ialloc.c:419
ext2_free_inode             → /root/linux/fs/ext2/ialloc.c:79
ext2_get_inode              → /root/linux/fs/ext2/inode.c:1314
ext2_iget                   → /root/linux/fs/ext2/inode.c:1387
ext2_data_block_valid       → /root/linux/fs/ext2/balloc.c:1177
ext2_group_first_block_no   → /root/linux/fs/ext2/ext2.h:798
ext2_group_last_block_no    → /root/linux/fs/ext2/ext2.h:804
read_block_bitmap           → /root/linux/fs/ext2/balloc.c:129
read_inode_bitmap           → /root/linux/fs/ext2/ialloc.c:31

================================================================================
PART 1 — STRUCTURAL CHANGES
================================================================================

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::super_block::SuperBlock;
```

```rust
use super::inode::{InodeDesc, RawInode};
```

```rust
use crate::fs::utils::IdBitmap;
```

```rust
use crate::fs::utils::FsEventSubscriberStats;
```

[STRUCTURE: BlockGroup — BEFORE]
```rust
#[derive(Debug)]
pub struct BlockGroup {
    idx: usize,
    desc: RwMutex<Dirty<GroupDesc>>,
    inode_table_backend: Arc<InodeTableBackend>,
    inode_table_cache: PageCache,
}
```

```rust
struct InodeTableBackend {
    inode_table_bid: Bid,
    raw_inodes_size: usize,
    fs: Weak<Ext2>,
}
```

[STRUCTURE: BlockGroup — AFTER]
```rust
/// A single Ext2 block group.
///
/// Owns all per-group state: descriptor, bitmaps, inode table cache,
/// and the block device handle needed for I/O. Provides self-contained
/// operations for block/inode allocation, deallocation, and inode
/// descriptor read/write within this group.
#[derive(Debug)]
pub struct BlockGroup {
    /// Block group index (0-based).
    idx: usize,
    /// Group descriptor with dirty tracking.
    desc: RwMutex<Dirty<GroupDesc>>,
    /// Backing block device (shared with Ext2 and other groups).
    block_device: Arc<dyn BlockDevice>,
    /// Cached geometry: first filesystem-wide block number of this group.
    first_block: u32,
    /// Cached geometry: last filesystem-wide block number of this group.
    last_block: u32,
    /// Cached geometry: inode table blocks per group.
    itb_per_group: u32,
    /// Cached geometry: inodes per group.
    inodes_per_group: u32,
    /// Cached geometry: inode size in bytes.
    inode_size: usize,
    /// Inode table page cache backend.
    inode_table_backend: Arc<InodeTableBackend>,
    /// Inode table page cache.
    inode_table_cache: PageCache,
}
```

```rust
/// Backend of the inode table page cache in one block group.
///
/// Linux equivalent: `sb_bread()` buffer_head cache path in
/// `/root/linux/fs/ext2/inode.c:1314` (`ext2_get_inode`).
/// Asterinas equivalent: `PageCacheBackend` implementation.
#[derive(Debug)]
struct InodeTableBackend {
    /// Physical block ID of `bg_inode_table`.
    inode_table_bid: Bid,
    /// Total inode table size in bytes (`inodes_per_group * inode_size`).
    raw_inodes_size: usize,
    /// Block device handle for I/O (replaces `Weak<Ext2>`).
    block_device: Arc<dyn BlockDevice>,
}
```

[STRUCTURE: Ext2 — UNCHANGED]
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

[STRUCTURE: GroupDesc — UNCHANGED]
```rust
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

================================================================================
PART 2 — BlockGroup::load CHANGES
================================================================================

[GUARANTEE]
impl BlockGroup {
    /// Loads a block group from the descriptor table.
    ///
    /// Now takes `Arc<dyn BlockDevice>` directly instead of `Weak<Ext2>`.
    /// Caches per-group geometry from `SuperBlock` at load time.
    pub fn load(
        group_descs: &USegment,
        idx: usize,
        sb: &SuperBlock,
        block_device: Arc<dyn BlockDevice>,
    ) -> Result<Self>;
}

[SPECIFICATION]
Pre (BlockGroup::load):
- `group_descs` covers at least `(idx + 1) * size_of::<RawGroupDesc>()` bytes.
- `sb` is validated.
- `block_device` is the filesystem's block device.

Post (BlockGroup::load):
- Reads `RawGroupDesc` at offset `idx * size_of::<RawGroupDesc>()`.
- Converts to `GroupDesc`.
- Caches geometry:
  - `first_block = sb.group_first_block_no(idx)`.
  - `last_block = sb.group_last_block_no(idx)`.
  - `itb_per_group = sb.itb_per_group()`.
  - `inodes_per_group = sb.inodes_per_group()`.
  - `inode_size = sb.inode_size()`.
- Creates `InodeTableBackend` with `block_device.clone()` instead of `Weak<Ext2>`.
- Creates `PageCache` with capacity `inodes_per_group * inode_size`.
- Returns `Err(EIO)` if descriptor read fails.

================================================================================
PART 3 — METHODS MOVED INTO BlockGroup
================================================================================

[GUARANTEE]
impl BlockGroup {
    // ── Block allocation (moved from Ext2::try_alloc_in_group) ──

    /// Attempts to allocate up to `count` contiguous blocks within this group.
    ///
    /// Returns `Ok(Some(range))` with filesystem-wide block numbers on success,
    /// `Ok(None)` if this group has no allocatable blocks,
    /// or `Err` on I/O failure.
    ///
    /// The second element of the tuple indicates whether bitmap corruption
    /// was detected (counter says free but bitmap disagrees).
    ///
    /// Linux: /root/linux/fs/ext2/balloc.c:682 (ext2_try_to_allocate)
    pub(super) fn alloc_blocks(
        &self,
        count: u32,
        sb_free_blocks: u32,
    ) -> Result<(Option<Range<u32>>, bool)>;

    /// Frees a range of blocks within this group.
    ///
    /// `bit` is the group-relative start index, `group_count` is the number
    /// of blocks to free. Returns the number of blocks actually freed
    /// (bits that transitioned from allocated to free).
    ///
    /// Linux: /root/linux/fs/ext2/balloc.c:482 (ext2_free_blocks, per-group portion)
    pub(super) fn free_blocks(&self, bit: u32, group_count: u32) -> Result<u32>;

    // ── Inode allocation (moved from Ext2::alloc_inode inner loop) ──

    /// Attempts to allocate one inode within this group.
    ///
    /// Returns `Ok(Some(inode_idx))` with the group-relative inode index
    /// (0-based) on success, `Ok(None)` if no free inode in this group.
    ///
    /// Linux: /root/linux/fs/ext2/ialloc.c:419 (ext2_new_inode, per-group portion)
    pub(super) fn alloc_inode(&self) -> Result<Option<u16>>;

    /// Frees one inode within this group.
    ///
    /// `bit` is the group-relative inode index (0-based).
    /// Returns `true` if the bit transitioned allocated→free,
    /// `false` if it was already free (logs warning).
    ///
    /// Linux: /root/linux/fs/ext2/ialloc.c:79 (ext2_free_inode, per-group portion)
    pub(super) fn free_inode(&self, bit: u16) -> Result<bool>;

    // ── Inode descriptor I/O (moved from Ext2) ──

    /// Reads an inode descriptor from the inode table of this group.
    ///
    /// `index_in_group` is the 0-based inode index within this group.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1314 (ext2_get_inode)
    pub(super) fn read_inode_desc(&self, index_in_group: u32) -> Result<InodeDesc>;

    /// Writes an inode descriptor to the inode table of this group.
    ///
    /// `index_in_group` is the 0-based inode index within this group.
    ///
    /// Linux: /root/linux/fs/ext2/inode.c:1314 (ext2_get_inode)
    pub(super) fn write_inode_desc(&self, index_in_group: u32, raw: &RawInode) -> Result<()>;

    // ── System zone check (moved from Ext2) ──

    /// Checks whether a filesystem-wide block range overlaps this group's
    /// system zone (block bitmap, inode bitmap, inode table).
    ///
    /// Linux: /root/linux/fs/ext2/balloc.c:1177 (ext2_data_block_valid, system zone part)
    fn overlaps_system_zone(&self, start: u32, count: u32) -> bool;
}

================================================================================
PART 4 — Ext2 METHODS THAT CHANGE (become thin orchestrators)
================================================================================

[GUARANTEE]
impl Ext2 {
    /// Loads block groups — signature changes: takes `Arc<dyn BlockDevice>`
    /// instead of `Weak<Self>`.
    pub(super) fn load_block_groups(
        sb: &SuperBlock,
        group_descs: &USegment,
        block_device: Arc<dyn BlockDevice>,
    ) -> Result<Vec<BlockGroup>>;

    /// Allocates up to `count` contiguous blocks (cross-group orchestrator).
    ///
    /// Iterates groups, delegates to `BlockGroup::alloc_blocks()`,
    /// updates superblock counter on success.
    pub(super) fn alloc_blocks(&self, count: u32) -> Result<Range<u32>>;

    /// Frees a range of blocks (cross-group orchestrator).
    ///
    /// Splits range across group boundaries, delegates per-group portion
    /// to `BlockGroup::free_blocks()`, updates superblock counter.
    pub(super) fn free_blocks(&self, start: u32, count: u32) -> Result<()>;

    /// Allocates a new inode number (cross-group orchestrator).
    ///
    /// Cyclic group scan from parent group, delegates to
    /// `BlockGroup::alloc_inode()`, updates superblock/group counters.
    pub(super) fn alloc_inode(&self, parent_ino: u32, inode_type: InodeType) -> Result<u32>;

    /// Frees an inode by number (cross-group orchestrator).
    ///
    /// Locates group, delegates to `BlockGroup::free_inode()`,
    /// updates superblock/group counters.
    pub(super) fn free_inode(&self, ino: u32) -> Result<()>;

    /// Reads an inode descriptor — delegates to BlockGroup.
    pub(super) fn read_inode_desc(&self, ino: u32) -> Result<InodeDesc>;

    /// Writes an inode descriptor — delegates to BlockGroup.
    pub(super) fn write_inode_desc(&self, ino: u32, raw: &RawInode) -> Result<()>;

    /// Reads an inode — delegates descriptor read to BlockGroup.
    pub(super) fn read_inode(&self, ino: u32) -> Result<Arc<Inode>>;
}

================================================================================
PART 5 — DETAILED SPECIFICATIONS
================================================================================

[SPECIFICATION]

# ── BlockGroup::alloc_blocks ──

Pre:
- `count > 0`.
- `sb_free_blocks` is the current superblock free block count (passed by caller
  to avoid BlockGroup needing superblock access).

Post (success — Some(range)):
- Loads block bitmap via `self.load_block_bitmap()` (no longer needs `&Ext2` or
  `&SuperBlock` parameters — uses cached geometry).
- Performs corruption check: if `free_blocks_count > 0` but bitmap has no free
  bit, sets `saw_corruption = true`.
- Attempts consecutive allocation with halving fallback:
  - `req = min(count, group_size) as u16`.
  - Loop: `IdBitmap::alloc_consecutive(req)`, on failure `req /= 2`, until `req == 0`.
- For each candidate range, converts to filesystem-wide block numbers:
  `ret_block = self.first_block + range.start as u32`.
- Rejects ranges that overlap system zone via `self.overlaps_system_zone()`.
- Rejects ranges where `free_blocks_count < alloc_len` or `sb_free_blocks < alloc_len`.
- On valid range:
  - Restores any previously rejected ranges in bitmap.
  - Persists bitmap to disk via `self.block_device.write_bytes(block_bitmap_bid.to_offset(), ...)`.
  - Updates group counter: `self.dec_free_blocks(alloc_len as u16)`.
  - Returns `Ok((Some(ret_block..ret_block+alloc_len), saw_corruption))`.

Post (failure — None):
- Returns `Ok((None, saw_corruption))` if no allocatable range exists in this group.

Post (error):
- Returns `Err(EIO)` on bitmap I/O failure or invalid group geometry.

# ── BlockGroup::free_blocks ──

Pre:
- `bit` is the group-relative block index.
- `group_count > 0`.
- Caller has validated that the range is in the data zone.

Post (success):
- Loads block bitmap.
- Checks system zone overlap via `self.overlaps_system_zone()`; returns `Err(EIO)` if overlap.
- For each bit in `[bit, bit + group_count)`:
  - If already free, logs warning.
- Calls `bitmap.free_consecutive(bit..bit+group_count)`.
- Persists bitmap to disk.
- Updates group counter: `self.inc_free_blocks(group_count as u16)`.
- Returns `Ok(group_count)` — the number of blocks freed.

Post (error):
- Returns `Err(EIO)` on bitmap I/O failure or system zone overlap.

# ── BlockGroup::alloc_inode ──

Pre:
- Caller has checked `self.free_inodes_count() > 0` before calling.

Post (success — Some(inode_idx)):
- Loads inode bitmap via `self.load_inode_bitmap()` (no longer needs `&Ext2` or
  `&SuperBlock` — uses cached geometry).
- Allocates one bit with `IdBitmap::alloc()`.
- Persists bitmap to disk via `self.block_device.write_bytes(inode_bitmap_bid.to_offset(), ...)`.
- Returns `Ok(Some(inode_idx))` where `inode_idx` is the 0-based group-relative index.
- Does NOT update group/superblock counters (caller does this).

Post (failure — None):
- Returns `Ok(None)` if `IdBitmap::alloc()` returns `None`.

Post (error):
- Returns `Err(EIO)` on bitmap I/O failure.

# ── BlockGroup::free_inode ──

Pre:
- `bit` is a valid group-relative inode index.

Post (success):
- Loads inode bitmap.
- If bit is already free: logs warning, persists bitmap, returns `Ok(false)`.
- If bit is allocated: clears it, persists bitmap, returns `Ok(true)`.

Post (error):
- Returns `Err(EIO)` on bitmap I/O failure.

# ── BlockGroup::read_inode_desc ──

Pre:
- `index_in_group < self.inodes_per_group`.

Post:
- Computes `offset_bytes = index_in_group * self.inode_size`.
- Reads `RawInode` from `self.inode_table_cache` at `offset_bytes`.
- Converts via `InodeDesc::try_from(&raw)`.
- Returns `Err(EIO)` on page cache read failure.
- Returns `Err(ESTALE)` for deleted inode.

# ── BlockGroup::write_inode_desc ──

Pre:
- `index_in_group < self.inodes_per_group`.

Post:
- Computes `offset_bytes = index_in_group * self.inode_size`.
- Writes `raw` to `self.inode_table_cache` at `offset_bytes`.
- Returns `Err(EIO)` on page cache write failure.

# ── BlockGroup::overlaps_system_zone ──

Pre:
- `start` is a filesystem-wide block number.
- `count > 0`.

Post:
- Returns `true` if `[start, start+count-1]` overlaps any of:
  - Block bitmap block (`desc.block_bitmap`).
  - Inode bitmap block (`desc.inode_bitmap`).
  - Inode table range (`desc.inode_table .. desc.inode_table + itb_per_group - 1`).
- Uses `ranges_overlap` helper (stays as a free function or associated function).

# ── BlockGroup::load_block_bitmap (CHANGED SIGNATURE) ──

Pre:
- Previously: `fn load_block_bitmap(&self, fs: &Ext2, sb: &SuperBlock) -> Result<IdBitmap>`.
- After: `fn load_block_bitmap(&self) -> Result<IdBitmap>`.
- Uses `self.block_device` for I/O and `self.first_block`/`self.last_block` for geometry.

Post:
- Same validation logic as before but using cached fields instead of `&Ext2`/`&SuperBlock`.

# ── BlockGroup::load_inode_bitmap (CHANGED SIGNATURE) ──

Pre:
- Previously: `fn load_inode_bitmap(&self, fs: &Ext2, sb: &SuperBlock) -> Result<IdBitmap>`.
- After: `fn load_inode_bitmap(&self) -> Result<IdBitmap>`.
- Uses `self.block_device` for I/O and `self.inodes_per_group` for capacity.

Post:
- Same logic as before but using cached fields.

# ── Ext2::alloc_blocks (SIMPLIFIED) ──

Pre:
- `count > 0`.

Post (success):
- Reads superblock geometry once.
- Iterates groups 0..groups_count.
- Skips groups with `free_blocks_count == 0`.
- Calls `group.alloc_blocks(count, sb_free_blocks)`.
- On `Some(range)`: updates `sb.dec_free_blocks(range.len())`, returns `Ok(range)`.
- Tracks `saw_corruption` across groups.

Post (failure):
- `Err(ENOSPC)` if no group yields blocks.
- `Err(EIO)` if corruption detected and no allocation succeeded.

# ── Ext2::free_blocks (SIMPLIFIED) ──

Pre:
- If `count == 0`, returns `Ok(())`.

Post:
- Validates data range via `SuperBlock::data_block_valid`.
- Splits range across group boundaries.
- For each group portion: calls `group.free_blocks(bit, group_count)`.
- Accumulates freed count, updates `sb.inc_free_blocks(total_freed)`.

# ── Ext2::alloc_inode (SIMPLIFIED) ──

Pre:
- `parent_ino` in `[ROOT_INO, total_inodes]`.

Post:
- Cyclic group scan from parent group.
- Skips groups with `free_inodes_count == 0`.
- Calls `group.alloc_inode()`.
- On `Some(inode_idx)`: computes `ino`, validates range, updates counters
  (`group.dec_free_inodes(1)`, `sb.dec_free_inodes()`,
  if dir: `group.inc_used_dirs()`).

# ── Ext2::free_inode (SIMPLIFIED) ──

Pre:
- `ino` in `[first_ino, total_inodes]`.

Post:
- Reads inode desc to determine `is_dir` — now via `group.read_inode_desc(index_in_group)`.
- Calls `group.free_inode(bit)`.
- If returned `true` (bit was allocated): updates counters.

# ── Ext2::read_inode_desc (THIN WRAPPER) ──

Post:
- Validates ino range.
- Computes `group_idx = (ino - 1) / inodes_per_group`.
- Computes `index_in_group = (ino - 1) % inodes_per_group`.
- Delegates to `group.read_inode_desc(index_in_group)`.

# ── Ext2::write_inode_desc (THIN WRAPPER) ──

Post:
- Same ino validation and group lookup.
- Delegates to `group.write_inode_desc(index_in_group, raw)`.

# ── Ext2::load_block_groups (SIGNATURE CHANGE) ──

Post:
- Takes `block_device: Arc<dyn BlockDevice>` instead of `fs: Weak<Self>`.
- Passes `block_device.clone()` to each `BlockGroup::load()`.

# ── InodeTableBackend (CHANGED) ──

Post:
- Stores `block_device: Arc<dyn BlockDevice>` instead of `fs: Weak<Ext2>`.
- `read_page_async` / `write_page_async` use `self.block_device` directly
  instead of `self.fs.upgrade()?.block_device()`.
- Removes the `Err(EIO, "filesystem already dropped")` path since
  `Arc<dyn BlockDevice>` is always valid.

================================================================================
PART 6 — METHODS REMOVED FROM Ext2
================================================================================

The following private methods are removed from `Ext2` because their logic
is now encapsulated within `BlockGroup`:

- `Ext2::try_alloc_in_group` → replaced by `BlockGroup::alloc_blocks`
- `Ext2::range_overlaps_system_zone` → replaced by `BlockGroup::overlaps_system_zone`
- `Ext2::group_first_block_no` (static) → geometry cached in `BlockGroup::first_block`
- `Ext2::group_last_block_no` (static) → geometry cached in `BlockGroup::last_block`
- `Ext2::data_block_valid` (static) → moved to `SuperBlock::data_block_valid` (already exists)
- `Ext2::ranges_overlap` (static) → kept as a module-level helper or moved into `BlockGroup`

================================================================================
PART 7 — INVARIANTS
================================================================================

Invariant (BlockGroup self-containment):
- After construction, `BlockGroup` can perform all per-group operations
  (bitmap load, block/inode alloc/free, inode desc read/write) without
  any reference to `Ext2`.
- `BlockGroup` does NOT hold `Weak<Ext2>` or any back-pointer to the filesystem.
- `BlockGroup` does NOT modify superblock counters — that is the caller's
  responsibility (`Ext2` orchestrator methods).

Invariant (counter update protocol):
- Group-level counters (`free_blocks_count`, `free_inodes_count`, `used_dirs_count`)
  are updated by `BlockGroup` methods for operations fully within the group.
- Superblock-level counters (`free_blocks_count`, `free_inodes_count`) are
  updated only by `Ext2` orchestrator methods after successful group operations.
- Exception: `BlockGroup::alloc_blocks` updates its own `dec_free_blocks`
  because the bitmap persistence and counter update must be atomic within
  the group's lock scope. `Ext2` updates the superblock counter separately.
- `BlockGroup::free_blocks` updates its own `inc_free_blocks` for the same reason.
- `BlockGroup::alloc_inode` does NOT update counters — returns raw index only.
  Caller (`Ext2::alloc_inode`) updates both group and superblock counters
  after computing the filesystem-wide ino and validating the range.
- `BlockGroup::free_inode` does NOT update counters — returns bool only.
  Caller (`Ext2::free_inode`) updates counters based on the return value.

Invariant (bitmap I/O ownership):
- All bitmap reads and writes go through `self.block_device` within `BlockGroup`.
- `Ext2` never directly reads or writes bitmaps after this refactor.

Invariant (inode table I/O ownership):
- All inode table reads and writes go through `self.inode_table_cache` within `BlockGroup`.
- `Ext2` never directly accesses inode table pages after this refactor.

================================================================================
PART 8 — TEST IMPACT
================================================================================

[TEST]
Existing tests in `fs.rs::test` and `block_group.rs::test` must be updated:

1. `BlockGroup::load` call sites: change `Weak::new()` / `fs: Weak<Ext2>` to
   `Arc<dyn BlockDevice>` (use test disk directly).

2. `load_block_bitmap` / `load_inode_bitmap` call sites: remove `&Ext2` and
   `&SuperBlock` parameters.

3. `Ext2::load_block_groups` call sites: pass `Arc<dyn BlockDevice>` instead
   of `Weak<Self>`.

4. New unit tests for `BlockGroup`:
   - `block_group_alloc_blocks_ok`: allocate blocks within a single group.
   - `block_group_free_blocks_ok`: free blocks within a single group.
   - `block_group_alloc_inode_ok`: allocate inode within a single group.
   - `block_group_free_inode_ok`: free inode within a single group.
   - `block_group_read_write_inode_desc`: read/write inode descriptor.
   - `block_group_overlaps_system_zone`: system zone overlap detection.

5. Existing `Ext2`-level tests (`block_alloc_free_ok`, `inode_alloc_free_ok`, etc.)
   should continue to pass unchanged — they test the orchestrator layer.

[DIFF]
Linux: Per-group operations are spread across `balloc.c`, `ialloc.c`, `inode.c`
  with `struct ext2_sb_info` passed everywhere.
  → Asterinas: Per-group operations are encapsulated in `BlockGroup` with cached
  geometry and owned `Arc<dyn BlockDevice>`.
  Reason: Better encapsulation, no circular `Weak<Ext2>` dependency, each
  `BlockGroup` is self-contained for I/O.

Linux: `buffer_head` provides implicit caching and device access.
  → Asterinas: `Arc<dyn BlockDevice>` stored directly in `BlockGroup` and
  `InodeTableBackend`.
  Reason: No buffer_head layer; direct device reference is simpler and avoids
  weak pointer upgrade failures.

Linux: Superblock and group descriptor counters are updated in the same function
  that modifies bitmaps.
  → Asterinas: Group counters updated in `BlockGroup` methods, superblock counters
  updated in `Ext2` orchestrator methods.
  Reason: Clean separation of per-group vs. filesystem-wide state management.
