[PROMPT]
Provide additions to `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_get_blocks (write path) → fs/ext2/inode.c:624
ext2_get_branch              → fs/ext2/inode.c:234
ext2_alloc_branch            → fs/ext2/inode.c:479
ext2_splice_branch           → fs/ext2/inode.c:561
ext2_blks_to_allocate        → fs/ext2/inode.c:361
ext2_find_goal               → fs/ext2/inode.c:330
ext2_find_near               → fs/ext2/inode.c:294

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::fs::Ext2;
```

```rust
use super::utils::Dirty;
```

```rust
#[derive(Debug)]
pub struct Inode {
    ino: u32,
    type_: InodeType,
    inner: RwMutex<InodeInner>,
    block_group_idx: usize,
    fs: Weak<Ext2>,
}
```

```rust
#[derive(Debug)]
pub struct InodeInner {
    desc: Dirty<InodeDesc>,
    is_freed: bool,
    weak_self: Weak<Inode>,
    fs: Weak<Ext2>,
}
```

```rust
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeDesc {
    type_: InodeType,
    perm: FilePerm,
    uid: u32,
    gid: u32,
    size: u64,
    atime: Duration,
    ctime: Duration,
    mtime: Duration,
    dtime: Duration,
    links_count: u16,
    blocks: u32,
    flags: FileFlags,
    file_acl: u32,
    block_ptrs: [u32; 15],
}
```

```rust
pub(super) struct BlockPath {
    pub depth: usize,
    pub offsets: [u32; 4],
    pub boundary: u32,
}
```

```rust
impl InodeInner {
    pub(super) fn block_to_path(&self, iblock: u32) -> Result<BlockPath>;
    pub(super) fn get_block(&self, iblock: u32) -> Result<Option<Bid>>;
    fn persist_inode_and_sync(&self, fs: &Ext2) -> Result<()>;
}
```

```rust
impl Ext2 {
    pub fn block_device(&self) -> &dyn BlockDevice;
    pub fn block_size(&self) -> usize;
    pub(super) fn alloc_blocks(&self, count: u32) -> Result<Range<u32>>;
    pub(super) fn free_blocks(&self, start: u32, count: u32) -> Result<()>;
}
```

[GUARANTEE]
```rust
/// Represents one level in the indirect block chain traversal.
/// Asterinas equivalent of Linux `Indirect` (inode.c:114).
///
/// - `key`: The physical block number read from this level's pointer slot.
///          0 means the chain is broken (hole) at this level.
/// - `bh`:  The indirect block buffer that contains the pointer slot.
///          `None` for level 0 (pointer lives in `inode.block_ptrs`).
struct IndirectEntry {
    key: u32,
    bh: Option<Vec<u8>>,
}
```

```rust
/// Result of `get_branch`: describes how far the existing block pointer
/// chain extends for a given `BlockPath`.
///
/// - `partial_level`: The level index where the chain broke (pointer was 0).
///   If the chain is complete (all levels resolved), `partial_level == depth`
///   and `chain[depth-1].key` is the physical data block.
/// - `chain`: One `IndirectEntry` per level traversed (length == `partial_level + 1`
///   for incomplete chains — includes the zero-key entry — or `depth` for
///   complete chains).
struct BranchResult {
    partial_level: usize,
    chain: Vec<IndirectEntry>,
}
```

```rust
impl InodeInner {
    /// Traverses the existing block pointer chain for a given block path.
    ///
    /// Linux: fs/ext2/inode.c:234 (ext2_get_branch)
    ///
    /// # Arguments
    /// * `path` - The `BlockPath` produced by `block_to_path`.
    /// * `fs` - Reference to the live `Ext2` filesystem.
    ///
    /// # Returns
    /// A `BranchResult` describing how far the chain extends.
    fn get_branch(&self, path: &BlockPath, fs: &Ext2) -> Result<BranchResult>;

    /// Counts how many blocks (indirect + data) need to be allocated for a
    /// branch that broke at `partial_level`.
    ///
    /// Linux: fs/ext2/inode.c:361 (ext2_blks_to_allocate)
    ///
    /// # Arguments
    /// * `branch` - The `BranchResult` from `get_branch` (incomplete chain).
    /// * `path` - The `BlockPath` for the target logical block.
    ///
    /// # Returns
    /// `(indirect_blks, data_blks)` — number of indirect metadata blocks and
    /// data blocks to allocate respectively.
    fn blks_to_allocate(&self, branch: &BranchResult, path: &BlockPath) -> (u32, u32);

    /// Allocates blocks, builds the new indirect chain, connects it to the
    /// inode's block pointer tree, and updates inode metadata.
    ///
    /// Combines the logic of Linux `ext2_alloc_branch` (inode.c:479) and
    /// `ext2_splice_branch` (inode.c:561). In Linux these are separate to
    /// minimize the critical section under `truncate_mutex`; in Asterinas
    /// the caller already holds `&mut self` (exclusive write lock on
    /// `InodeInner`), so the split provides no concurrency benefit.
    ///
    /// On success, returns the physical block number of the allocated data block.
    /// On failure, all partially allocated blocks are freed before returning.
    ///
    /// # Arguments
    /// * `fs` - Reference to the live `Ext2` filesystem.
    /// * `indirect_blks` - Number of indirect metadata blocks to allocate.
    /// * `data_blks` - Number of data blocks to allocate.
    /// * `path` - The `BlockPath` (provides offsets for pointer placement).
    /// * `branch` - The `BranchResult` (provides `partial_level` and parent buffer).
    ///
    /// # Returns
    /// * `Ok(Bid)` - Physical block ID of the newly allocated data block.
    /// * `Err(ENOSPC)` - Allocation failed.
    /// * `Err(EIO)` - Indirect block I/O failed.
    fn alloc_and_splice_branch(
        &mut self,
        fs: &Ext2,
        indirect_blks: u32,
        data_blks: u32,
        path: &BlockPath,
        branch: &BranchResult,
    ) -> Result<Bid>;

    /// Resolves a logical block to a physical block, allocating if necessary.
    ///
    /// Orchestrates `get_branch` → `blks_to_allocate` → `alloc_and_splice_branch`
    /// to implement the full block mapping with allocation.
    ///
    /// Linux: fs/ext2/inode.c:624 (ext2_get_blocks)
    ///
    /// # Arguments
    /// * `iblock` - Logical block number within the file.
    /// * `create` - If true, allocate blocks for holes; if false, behave like `get_block`.
    ///
    /// # Returns
    /// * `Ok(Some(bid))` - Physical block ID for the logical block.
    /// * `Ok(None)` - Block not mapped and `create` is false (sparse hole).
    /// * `Err(ENOSPC)` - No space for allocation.
    /// * `Err(EIO)` - I/O or metadata error.
    pub(super) fn get_or_alloc_block(&mut self, iblock: u32, create: bool) -> Result<Option<Bid>>;
}
```

[SPECIFICATION]

## 7.1.0  get_branch — Indirect Block Chain Traversal

Pre (get_branch):
- `self` refers to a valid, non-freed `InodeInner`.
- `path` is a valid `BlockPath` with `depth >= 1`, produced by `block_to_path`.
- `fs` is a live reference to the `Ext2` filesystem.

Post (get_branch: success):
- Mirrors Linux `ext2_get_branch` (inode.c:234), minus the `verify_chain` /
  `-EAGAIN` retry logic (see [DIFF] for rationale).
- Algorithm:
  1. Reads the first pointer: `key = self.desc.block_ptrs[path.offsets[0] as usize]`.
     Pushes `IndirectEntry { key, bh: None }` into `chain`.
     (Linux: `add_chain(chain, NULL, EXT2_I(inode)->i_data + *offsets)`)
  2. If `key == 0`, the chain is broken at level 0.
     Returns `BranchResult { partial_level: 0, chain }`.
     (Linux: `if (!p->key) goto no_block`)
  3. For each subsequent level `1..path.depth`:
     a. Reads the indirect block at physical block `chain[level-1].key` from disk
        via `fs.block_device().read_bytes(Bid::new(key as u64).to_offset(), &mut buf)`.
        (Linux: `bh = sb_bread(sb, le32_to_cpu(p->key))`)
     b. On read failure, returns `Err(EIO)`.
        (Linux: `goto failure` → `*err = -EIO`)
     c. Extracts the next pointer at byte offset `path.offsets[level] * 4` from
        the buffer as a little-endian u32.
        (Linux: `add_chain(++p, bh, (__le32*)bh->b_data + *++offsets)`)
     d. Pushes `IndirectEntry { key: next_ptr, bh: Some(buf) }` into `chain`.
     e. If `next_ptr == 0`, the chain is broken at this level.
        Returns `BranchResult { partial_level: level, chain }`.
        (Linux: `if (!p->key) goto no_block`)
     - No `verify_chain` check between levels. The caller holds at least a read
       lock on `InodeInner`, so no concurrent truncation or allocation can modify
       the chain during traversal.
  4. If all levels traversed with non-zero pointers, the chain is complete.
     Returns `BranchResult { partial_level: path.depth, chain }`.
     (Linux: `return NULL` meaning "no partial — full chain found")

Post (get_branch: failure):
- `Err(EIO)` if any indirect block read fails.

Invariant:
- `chain.len() == partial_level + 1` when the chain is incomplete (broken at
  `partial_level`; the entry with `key == 0` is included), or
  `chain.len() == path.depth` when complete.
- `chain[i].bh` is `None` for `i == 0` (pointer from inode), `Some(buf)` for
  `i >= 1` (pointer from indirect block on disk).
- `chain[i].key` is the physical block number read from level `i`'s pointer slot.
  The last entry's `key` is 0 when the chain is incomplete.

Note (refactor get_block):
- The existing `get_block` method (inode.rs:612) currently inlines the same
  chain-traversal logic. After `get_branch` is implemented, `get_block` SHOULD
  be refactored to call `get_branch` internally:
    ```
    let branch = self.get_branch(&path, &fs)?;
    if branch.partial_level < path.depth {
        return Ok(None); // hole
    }
    Ok(Some(Bid::new(branch.chain[path.depth - 1].key as u64)))
    ```
  This eliminates duplicated traversal code between read and write paths.

---

## 7.1.1  blks_to_allocate — Count Blocks Needed for Allocation

Pre (blks_to_allocate):
- `branch` is a `BranchResult` from `get_branch` with an incomplete chain
  (`branch.partial_level < path.depth`).
- `path` is the `BlockPath` for the target logical block.

Post (blks_to_allocate: success):
- Mirrors Linux `ext2_blks_to_allocate` (inode.c:361), simplified to
  single-block data allocation.
- Algorithm:
  1. `indirect_blks = (path.depth - 1) - branch.partial_level`:
     The number of missing indirect metadata levels between the break point
     and the data block level.
     (Linux: `indirect_blks = (chain + depth) - partial - 1`, inode.c:724)
  2. `data_blks = 1`:
     Allocate exactly one data block.
     (Linux scans consecutive zero slots in the indirect block to allocate
     multiple contiguous data blocks up to `maxblocks`; Asterinas allocates
     one per call for simplicity.)
  3. Returns `(indirect_blks, data_blks)`.

Invariant:
- `indirect_blks + data_blks >= 1` (at minimum one data block).
- `indirect_blks` is in range `[0, 3]` (max depth is 4: direct/ind/dind/tind).

---

## 7.1.2  alloc_and_splice_branch — Allocate, Build, and Connect Chain

Pre (alloc_and_splice_branch):
- `self` refers to a valid, non-freed `InodeInner` (caller holds `&mut self`).
- `fs` is a live reference to the `Ext2` filesystem.
- `indirect_blks >= 0`, `data_blks >= 1`.
- `path` and `branch` describe the incomplete chain from `get_branch`.

Post (alloc_and_splice_branch: success):
- Combines Linux `ext2_alloc_branch` (inode.c:479) + `ext2_splice_branch`
  (inode.c:561). In Linux these are separate to minimize the critical section
  under `truncate_mutex`; in Asterinas the caller already holds the exclusive
  write lock on `InodeInner`, so the split provides no concurrency benefit.
- Algorithm:
  1. **Allocate all blocks** (Linux: ext2_alloc_blocks, inode.c:399):
     - `total = indirect_blks + data_blks`.
     - Calls `fs.alloc_blocks(total)` to obtain a contiguous range.
     - If the returned range length < `total`, retries in a loop for the
       remaining blocks (each call to `alloc_blocks` for the deficit).
       (Linux: `while(1) { count = target; current_block = ext2_new_blocks(...); ... }`)
     - On allocation failure, frees any partially allocated blocks via
       `fs.free_blocks()` and returns `Err(ENOSPC)`.
       (Linux: `failed_out:` label, inode.c:444)
     - Collects all allocated block numbers into `new_blocks: Vec<u32>`:
       indices `[0..indirect_blks)` are indirect metadata blocks,
       index `[indirect_blks]` is the data block.

  2. **Initialize indirect blocks** (Linux: ext2_alloc_branch body, inode.c:500):
     - Let `partial_level = branch.partial_level`.
     - For each `i` in `0..indirect_blks`:
       a. Create a zero-filled buffer of `block_size` bytes.
          (Linux: `memset(bh->b_data, 0, blocksize)`, inode.c:513)
       b. Write `new_blocks[i + 1]` as little-endian u32 at byte offset
          `path.offsets[partial_level + 1 + i] * 4` within the buffer.
          (Linux: `branch[n].p = (__le32*)bh->b_data + offsets[n]`,
           `*branch[n].p = branch[n].key`, inode.c:514-516)
       c. Write the buffer to disk at `Bid::new(new_blocks[i] as u64).to_offset()`.
          (Linux: `mark_buffer_dirty_inode(bh, inode)`, inode.c:529)
     - On write failure, frees all allocated blocks and returns `Err(EIO)`.
       (Linux: `failed:` label, inode.c:540-546)

  3. **Set the missing link** (Linux: ext2_splice_branch, `*where->p = where->key`, inode.c:573):
     - If `partial_level == 0` (break is at the inode's block pointer array):
       - `self.desc.block_ptrs[path.offsets[0] as usize] = new_blocks[0]`.
     - Else (break is inside an existing indirect block):
       - Takes the buffer from `branch.chain[partial_level - 1].bh`
         (the last valid indirect block).
       - Writes `new_blocks[0]` as little-endian u32 at byte offset
         `path.offsets[partial_level] * 4` within that buffer.
       - Writes the modified buffer back to disk at
         `Bid::new(branch.chain[partial_level - 1].key as u64).to_offset()`.
       (Linux: `if (where->bh) mark_buffer_dirty_inode(where->bh, inode)`,
        inode.c:599)
     - On write failure, frees all allocated blocks and returns `Err(EIO)`.

  4. **Update inode metadata**:
     - `self.desc.blocks += total * (block_size / 512) as u32`.
       (In Linux, `inode_add_bytes` is called inside `ext2_new_blocks` via
       `dquot_alloc_block` → `__dquot_alloc_space`. This is unconditional VFS
       block accounting, not a quota feature — executes even with
       `CONFIG_QUOTA` disabled.)
     - `self.desc.ctime = now()`.
       (Linux: `inode_set_ctime_current(inode)`, inode.c:602)
     - Mark `self.desc` dirty.
       (Linux: `mark_inode_dirty(inode)`, inode.c:603)

  5. Returns `Ok(Bid::new(new_blocks[indirect_blks] as u64))` — the data block.

Post (alloc_and_splice_branch: failure):
- `Err(ENOSPC)` if `fs.alloc_blocks` fails.
- `Err(EIO)` if writing an indirect block or the splice pointer to disk fails.
- On any failure, all blocks allocated so far are freed before returning.
- `self.desc` (block_ptrs, blocks, ctime) is not modified on any error path.

Invariant:
- On success, the block pointer tree is fully connected from
  `self.desc.block_ptrs` through any indirect blocks to the new data block.
- Each indirect block on disk is zero-filled except for a single le32 pointer
  at the appropriate offset linking to the next level.
- `self.desc.blocks` accurately reflects the total allocated blocks
  (data + indirect metadata) in 512-byte sector units.

---

## 7.1.3  get_or_alloc_block — Block Mapping with Allocation

Pre (get_or_alloc_block):
- `self` refers to a valid, non-freed `InodeInner`.
- `self.fs` can be upgraded to a live `Arc<Ext2>`.
- `self.desc.block_ptrs` reflects the current in-memory block pointer state.
- `iblock` is a logical block number (may be beyond current file extent).

Post (get_or_alloc_block: create=false):
- Behaves identically to `get_block`: returns `Ok(Some(bid))` if mapped,
  `Ok(None)` if the block is a sparse hole, or propagates errors.
- No allocation or mutation occurs.

Post (get_or_alloc_block: create=true, block already mapped):
- Returns `Ok(Some(bid))` with the existing physical block.
- No allocation or mutation occurs.

Post (get_or_alloc_block: create=true, block not mapped):
- Mirrors Linux `ext2_get_blocks` (inode.c:624) with `create=1`.
- Orchestrates the helper functions:
  1. `block_to_path(iblock)` → `path`.
     If `depth == 0`, returns `Err(EIO)`.
  2. `get_branch(&path, &fs)` → `branch`.
     If `branch.partial_level == path.depth`, block is already mapped —
     returns `Ok(Some(Bid::new(branch.chain[depth-1].key as u64)))`.
     If `!create`, returns `Ok(None)`.
  3. `blks_to_allocate(&branch, &path)` → `(indirect_blks, data_blks)`.
  4. `alloc_and_splice_branch(&fs, indirect_blks, data_blks, &path, &branch)` → `bid`.
     On error, propagates (alloc_and_splice_branch handles its own cleanup).
  5. Returns `Ok(Some(bid))`.

Post (get_or_alloc_block: failure):
- `Err(EIO)` if `block_to_path` returns depth 0, or if `get_branch` /
  `alloc_and_splice_branch` encounters I/O errors, or if `self.fs.upgrade()` fails.
- `Err(ENOSPC)` if `alloc_and_splice_branch` fails.
- `self.desc` is not modified on any error path.

[DIFF]
Linux: `ext2_get_branch` uses `i_meta_lock` (read lock) during traversal and
  calls `verify_chain` between each level to detect concurrent modification by
  truncate (which holds the write lock). If the chain changed, it returns
  `-EAGAIN` and the caller (`ext2_get_blocks`) retries under `truncate_mutex`.
  → Asterinas: `get_branch` performs no `verify_chain` and has no retry loop.
  The caller always holds at least a read lock on `RwMutex<InodeInner>`, which
  prevents any concurrent writer (truncate, alloc) from modifying `block_ptrs`
  or indirect blocks during traversal. The chain is guaranteed stable.
  Reason: Asterinas's coarse-grained `RwMutex<InodeInner>` makes the optimistic
  concurrency control (`verify_chain` + `-EAGAIN` + reread) unnecessary.

Linux: `ext2_get_blocks` with `create=1` uses `truncate_mutex` to serialize
  allocation against concurrent truncation, and `i_meta_lock` (rwlock) to
  protect the indirect block chain during reads.
  → Asterinas: `InodeInner` is accessed through `RwMutex<InodeInner>`, so
  `get_or_alloc_block` requires `&mut self` (exclusive write lock on InodeInner).
  This provides equivalent serialization without a separate truncate_mutex.
  Reason: Asterinas uses coarser-grained locking (whole InodeInner) vs Linux's
  fine-grained per-field locks.

Linux: `ext2_get_blocks` supports multi-block contiguous allocation via
  `ext2_blks_to_allocate` which counts consecutive zero slots in the indirect
  block, allocating up to `maxblocks` data blocks at once for readahead.
  → Asterinas: `get_or_alloc_block` allocates exactly one data block per call.
  Multi-block allocation is not implemented.
  Reason: Without page cache readahead, single-block allocation is sufficient.
  The indirect metadata blocks are still batch-allocated with the data block.

Linux: `ext2_find_goal` uses `i_block_alloc_info` to track the last allocated
  logical/physical block pair for sequential allocation heuristics, and
  `ext2_find_near` scans backward in the indirect block for locality.
  → Asterinas: No goal-based allocation. `fs.alloc_blocks(count)` uses the
  existing group-scanning allocator without placement hints.
  Reason: Goal-based allocation requires per-inode allocation state tracking
  not yet implemented. The group-scanning allocator provides basic locality.

Linux: `ext2_alloc_branch` (inode.c:479) and `ext2_splice_branch` (inode.c:561)
  are separate functions. `ext2_alloc_branch` allocates blocks and initializes
  indirect blocks but does NOT connect them to the inode tree. `ext2_splice_branch`
  then atomically sets the missing pointer. This split minimizes the critical
  section under `truncate_mutex`: the slow allocation + disk I/O happens outside
  the mutex, and only the fast pointer write happens inside.
  → Asterinas: Merged into a single `alloc_and_splice_branch`. The caller
  already holds `&mut self` (exclusive write lock on `InodeInner`) for the
  entire `get_or_alloc_block` call, so there is no concurrent truncation or
  read to race against. The two-phase split provides no concurrency benefit
  and only adds interface complexity (passing `new_blocks` between functions,
  split error handling responsibilities).
  Reason: Asterinas's coarse-grained `RwMutex<InodeInner>` eliminates the
  need for the fine-grained critical section optimization that motivated
  the split in Linux.

Linux: `ext2_alloc_branch` uses `buffer_head` for indirect blocks, marks them
  dirty, and syncs for directories via `sync_dirty_buffer`.
  → Asterinas: Indirect blocks are written directly to disk via
  `block_device().write_bytes()` immediately after initialization.
  Reason: No buffer_head layer; direct I/O is the current Asterinas model.

Linux: `ext2_splice_branch` updates `i_block_alloc_info` for next-allocation
  goal tracking, and handles multi-block direct allocation (`num == 0 && blks > 1`).
  → Asterinas: No allocation info tracking, and single data block per call.
  Reason: Deferred to a future optimization phase.
