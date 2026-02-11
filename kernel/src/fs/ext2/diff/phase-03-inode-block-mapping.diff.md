# Phase 03 Diff Report: Inode Read Path & Block Mapping

## Scope

- Rust: `kernel/src/fs/ext2/inode.rs`, `kernel/src/fs/ext2/fs.rs`
- Linux references:
  - `/root/linux/fs/ext2/inode.c:1314` (`ext2_get_inode`)
  - `/root/linux/fs/ext2/inode.c:1387` (`ext2_iget`)
  - `/root/linux/fs/ext2/inode.c:163` (`ext2_block_to_path`)
  - `/root/linux/fs/ext2/inode.c:783` (`ext2_get_block`)

---

## 1) No Inode Cache

- **Linux**: Inodes are managed by the VFS inode cache (`iget_locked` / `iput`). `ext2_iget()` calls `iget_locked()` which either returns a cached inode or allocates a new one. The inode hash table is global across the filesystem.
  - Reference: `/root/linux/fs/ext2/inode.c:1387`
- **Asterinas**: `read_inode()` always reads from disk and constructs a new `Arc<Inode>`. There is no cache lookup. Every call to `read_inode(ino)` creates a fresh object.
  - Rust: `fs.rs:143` (`read_inode`)
- **Impact**:
  - Same inode number can have multiple in-memory representations simultaneously.
  - No deduplication — two `read_inode(2)` calls return different `Arc<Inode>` objects.
  - Modifications to one copy are invisible to the other.
  - The roadmap notes this: "inode cache deferred; will use `BTreeMap<u32, Weak<Inode>>` when added."

## 2) Inode Read/Write: No buffer_head, Full Block Read-Modify-Write

- **Linux**: `ext2_get_inode()` uses `sb_bread()` to read the inode table block into a `buffer_head`. The returned pointer points directly into the cached block. Writes use `mark_inode_dirty()` → `__ext2_write_inode()` which modifies the buffer_head in-place and calls `mark_buffer_dirty(bh)`.
  - Reference: `/root/linux/fs/ext2/inode.c:1314`
- **Asterinas**: `read_inode_desc()` reads the entire inode table block into a `Vec<u8>`, then parses the `RawInode` at the correct offset. `write_inode_desc()` reads the full block, patches the inode bytes, then writes the full block back.
  - Rust: `fs.rs:182` (`read_inode_desc`), `fs.rs:224` (`write_inode_desc`)
- **Impact**: Every inode write requires a full block read + write cycle. Linux only dirties the buffer_head and defers the actual I/O.

## 3) block_to_path: Aligned with Linux

- **Linux**: `ext2_block_to_path()` computes offsets array and boundary value. Returns depth (0 on error).
  - Reference: `/root/linux/fs/ext2/inode.c:163`
- **Asterinas**: `block_to_path()` faithfully replicates the same logic with identical offset calculations. Returns `Result<BlockPath>` instead of depth integer.
  - Rust: `inode.rs:530`
- **Status**: Logic is aligned. The `if {} else if { block -= ...; }` pattern matches Linux's mutable `i_block` approach using `saturating_sub`.

## 4) get_block: Read-Only Path Only, No Chain Verification

- **Linux**: `ext2_get_branch()` reads the indirect block chain and performs `verify_chain()` — a re-read check that detects concurrent modifications (returns `-EAGAIN` if chain changed). Uses `read_lock(&EXT2_I(inode)->i_meta_lock)` for concurrency safety.
  - Reference: `/root/linux/fs/ext2/inode.c:234`
- **Asterinas**: `get_block()` reads indirect blocks sequentially without any chain verification or locking. No `-EAGAIN` retry loop.
  - Rust: `inode.rs:595`
- **Impact**: No protection against concurrent block tree modifications. If another thread modifies the indirect block tree while `get_block` is reading, stale data may be returned.

## 5) No PageCache Integration for Indirect Blocks

- **Linux**: Indirect blocks are read via `sb_bread()` which caches them in the buffer cache. Repeated accesses to the same indirect block hit the cache.
- **Asterinas**: Every `get_block()` call reads indirect blocks fresh from disk via `read_bytes()`.
- **Impact**: Repeated lookups in the same indirect block range cause redundant I/O.

## 6) InodeDesc Parsing: Mode-to-Type Differences

- **Linux**: `ext2_iget()` uses `inode->i_mode` directly from the raw inode. The VFS `inode_init_owner()` handles mode interpretation.
  - Reference: `/root/linux/fs/ext2/inode.c:1387`
- **Asterinas**: `InodeDesc::try_from(&RawInode)` calls `InodeType::from_raw_mode(mode)` which may reject unknown mode types with `Err(EINVAL)`.
  - Rust: `inode.rs:1268` (comment: "TODO: Different from Linux")
- **Impact**: Linux accepts any mode bits and lets VFS handle unknown types. Asterinas rejects inodes with unrecognized type bits.

## 7) No `i_block_alloc_info` / Reservation Window

- **Linux**: Each inode has `i_block_alloc_info` containing a reservation window (`ext2_reserve_window_node`) used for goal-directed block allocation. This is initialized in `ext2_iget()`.
  - Reference: `/root/linux/fs/ext2/inode.c:1387`
- **Asterinas**: No reservation window concept. Block allocation is purely bitmap-scan based.
- **Impact**: No locality optimization for sequential writes to the same file.

## 8) Inode Lifecycle: No evict_inode

- **Linux**: When an inode's reference count drops to zero, `ext2_evict_inode()` is called. If `nlink == 0`, it truncates data, frees the inode, and removes from orphan list.
  - Reference: `/root/linux/fs/ext2/inode.c:72`
- **Asterinas**: No eviction mechanism. When `Arc<Inode>` is dropped, the inode is simply deallocated from memory. No cleanup of on-disk resources occurs.
  - Rust: `Inode` has no `Drop` impl that triggers disk cleanup.
- **Impact**: Deleted-but-open files will leak disk resources (blocks and inode) when the last reference is dropped.

---

## Spec-vs-Linux Supplement (from spec review)

### S1) phase-03-inode-table-io.spec: Deleted Inode Detection

- **Spec**: `InodeDesc::try_from` checks `links_count == 0 && (mode == 0 || dtime != 0)` → `Err(ESTALE)`.
- **Linux**: `ext2_iget()` checks `inode->i_nlink == 0 && (inode->i_mode == 0 || !(EXT2_SB(inode->i_sb)->s_mount_state & EXT2_VALID_FS))`. The Linux check also considers mount state, not just dtime.
  - Reference: `/root/linux/fs/ext2/inode.c:1420-1430`
- **Diff**: Asterinas uses `dtime != 0` as the secondary condition; Linux uses mount state validity. Both detect stale inodes but via different heuristics.

### S2) phase-03-inode-table-io.spec: `inode_table_block` Address Arithmetic

- **Spec**: `inode_table_block(group_idx, table_block_index)` returns `base + table_block_index`.
- **Linux**: `ext2_get_inode()` computes `block = le32_to_cpu(gdp->bg_inode_table) + (offset >> EXT2_BLOCK_SIZE_BITS(sb))`.
  - Reference: `/root/linux/fs/ext2/inode.c:1330`
- **Status**: Aligned. Same arithmetic, different expression form.

### S3) phase-03-inode-cache.spec: Two-Layer Inode Abstraction

- **Spec**: `read_inode` constructs `Inode` (outer immutable handle) + `InodeInner` (mutable state under `RwMutex`). `InodeInner` has `is_freed` flag and `weak_self`/`fs` back-pointers.
- **Linux**: `ext2_iget()` fills a single `struct inode` with ext2 private data embedded via `EXT2_I(inode)`. No split handle.
  - Reference: `/root/linux/fs/ext2/inode.c:1387`
- **Diff**: Architectural difference. Asterinas separates identity (immutable) from state (mutable) for Rust ownership safety. Linux uses a single object with VFS-level locking.

### S4) phase-03-block-mapping.spec: `boundary` Value

- **Spec**: `block_to_path` computes `boundary` for direct blocks as `direct_blocks - 1 - iblock`. For indirect levels, boundary computation is not specified.
- **Linux**: `ext2_block_to_path` computes `boundary` at every level, used by `ext2_find_near()` for goal-directed allocation hints.
  - Reference: `/root/linux/fs/ext2/inode.c:163`
- **Diff**: Asterinas computes boundary but does not use it (no goal-based allocation). The value is carried in `BlockPath` but unused downstream.

### S5) phase-03-file-read.spec: `read_at` / `read_page` Not Implemented

- **Spec**: Defines `read_at` (byte-level file read) and `read_page` (block-sized page read for page cache). Both use `get_block` for block mapping and handle sparse regions as zeroes.
- **Linux**: File reads go through `generic_file_read_iter` → `ext2_read_folio` → `ext2_get_block`. The page cache handles caching and readahead.
  - Reference: `/root/linux/fs/ext2/file.c:283`, `/root/linux/fs/ext2/inode.c:917`
- **Diff**: Asterinas spec defines direct block-by-block reads without page cache integration. Linux uses the VFS page cache for all file data I/O. The spec's `read_page` is a callback stub for future page cache wiring, not a direct replacement for Linux's folio path.
