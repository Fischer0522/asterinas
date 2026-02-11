# Phase 04 Diff Report: Directory Read Path

## Scope

- Rust: `kernel/src/fs/ext2/dir.rs`, `kernel/src/fs/ext2/inode.rs`
- Linux references:
  - `/root/linux/fs/ext2/dir.c:99` (`ext2_check_folio`)
  - `/root/linux/fs/ext2/dir.c:342` (`ext2_find_entry`)
  - `/root/linux/fs/ext2/dir.c:257` (`ext2_readdir`)

---

## 1) No `i_dir_start_lookup` Hint

- **Linux**: `ext2_find_entry()` starts scanning from `EXT2_I(dir)->i_dir_start_lookup` and wraps around. After a successful find, the hint is updated to the found page index for future lookups.
  - Reference: `/root/linux/fs/ext2/dir.c:356`
- **Asterinas**: `find_entry()` always scans linearly from block 0.
  - Rust: `inode.rs:388`
- **Impact**: Linear scan on every lookup. For large directories, this is O(n) instead of amortized O(1) for sequential operations.

## 2) No PageCache / Folio-Based Directory I/O

- **Linux**: Directory data is read via `ext2_get_folio()` which uses the page cache. Directory blocks are cached as folios and only read from disk on cache miss. Writes go through `folio_mark_dirty()`.
  - Reference: `/root/linux/fs/ext2/dir.c:220` (`ext2_get_folio`)
- **Asterinas**: Directory blocks are read via raw `read_bytes()` into `Vec<u8>` buffers. Every `find_entry`, `readdir_at`, `add_entry`, `delete_entry` call reads blocks fresh from disk.
  - Rust: `inode.rs:401-411`
- **Impact**: No caching of directory data. Repeated lookups in the same directory re-read all blocks.

## 3) Directory Only Supports Direct Blocks

- **Linux**: Directory data can span direct, indirect, double-indirect, and triple-indirect blocks. `ext2_get_folio` uses the standard block mapping path.
- **Asterinas**: `link_new_data_block()` explicitly rejects `iblock >= 12` with `ENOSPC`. Directory growth is limited to 12 direct blocks.
  - Rust: `inode.rs:872-885`
- **Impact**: Maximum directory size is 12 * 4096 = 48KB. Large directories (>~3000 entries) cannot be created.

## 4) `ext2_check_folio` vs DirEntry::validate

- **Linux**: `ext2_check_folio()` validates an entire page of directory entries at once, checking alignment, rec_len chain continuity, and that the last entry reaches the page boundary.
  - Reference: `/root/linux/fs/ext2/dir.c:99`
- **Asterinas**: `DirEntry::validate()` checks individual entries. `DirEntryIter` validates entries one at a time during iteration.
  - Rust: `dir.rs:36` (`validate`), `dir.rs:135` (`next_entry`)
- **Status**: Functionally equivalent per-entry validation. Missing the whole-page pre-validation pass.

## 5) `need_revalidate` / `inode_query_iversion` / `ext2_validate_entry`

- **Linux**: `ext2_readdir()` checks `need_revalidate` flag and calls `ext2_validate_entry()` to re-sync the position after directory modification. Uses `inode_query_iversion()` to detect changes.
  - Reference: `/root/linux/fs/ext2/dir.c:280`
- **Asterinas**: No revalidation mechanism. `readdir_at()` trusts the offset parameter directly.
  - Rust: `inode.rs:436`
- **Impact**: If the directory is modified between `readdir` calls, the offset may point to the middle of an entry, causing incorrect results or errors.

---

## Spec-vs-Linux Supplement (from spec review)

### S1) phase-04-dir-entry-parse.spec: `rec_len_from_disk` 64K Handling

- **Spec**: `rec_len_from_disk` returns `rec_len` as-is since `PAGE_SIZE < 65536`, skipping the 64K special case.
- **Linux**: `ext2_rec_len_from_disk()` has a `#if (PAGE_SIZE >= 65536)` guard that maps `EXT2_MAX_REC_LEN` to `1 << 16`.
  - Reference: `/root/linux/fs/ext2/dir.c:38`
- **Status**: Aligned for 4K page size. The 64K path is correctly omitted.

### S2) phase-04-dir-entry-parse.spec: `dir_rec_len` Formula

- **Spec**: `dir_rec_len(name_len) = ((name_len + 8 + 3) & !3)` — matches Linux `EXT2_DIR_REC_LEN`.
- **Linux**: `#define EXT2_DIR_REC_LEN(name_len) (((name_len) + 8 + EXT2_DIR_ROUND) & ~EXT2_DIR_ROUND)` where `EXT2_DIR_ROUND = 3`.
  - Reference: `/root/linux/include/linux/ext2_fs.h:295`
- **Status**: Aligned.

### S3) phase-04-dir-entry-parse.spec: Validation Scope Difference

- **Spec**: `DirEntry::validate` checks per-entry: `rec_len >= EXT2_DIR_REC_LEN(1)`, 4-byte alignment, `rec_len >= EXT2_DIR_REC_LEN(name_len)`, `offset + rec_len <= limit`, `name_len <= NAME_MAX`, `inode <= max_inumber`.
- **Linux**: `ext2_check_folio` performs the same per-entry checks but also validates the **whole-page chain**: the last entry's `rec_len` must reach exactly the page boundary. If `limit` is not chunk-aligned, the page is rejected.
  - Reference: `/root/linux/fs/ext2/dir.c:99-150`
- **Diff**: Asterinas validates entries individually during iteration. Linux pre-validates the entire page before any entry access. Missing the chain-continuity check means a truncated last entry (where `offset + rec_len < limit`) would not be caught until iteration reaches it.

### S4) phase-04-dir-lookup-readdir.spec: `find_entry` Block Bound Check

- **Spec**: `find_entry` computes `max_blocks = self.desc.blocks >> 3` (512-byte sectors to fs-block bound) and returns `Err(ENOENT)` if `block_idx > max_blocks`.
- **Linux**: `ext2_find_entry` uses `npages = dir_pages(dir)` which computes `(i_size + PAGE_SIZE - 1) >> PAGE_SHIFT`. It iterates pages, not blocks, and uses `i_size` as the bound.
  - Reference: `/root/linux/fs/ext2/dir.c:349`
- **Diff**: Asterinas uses `i_blocks`-derived bound; Linux uses `i_size`-derived bound. These can diverge if `i_blocks` is inconsistent with `i_size` (e.g., after corruption). Linux's approach is more robust since `i_size` is the authoritative directory length.

### S5) phase-04-dir-lookup-readdir.spec: `readdir_at` File Type Mapping

- **Spec**: Converts ext2 `file_type` (0..7) to `InodeType` mapping. Uses `DirentVisitor` callback.
- **Linux**: `ext2_readdir` checks `has_filetype` flag (from `EXT2_FEATURE_INCOMPAT_FILETYPE`) before using `file_type`. If the feature is absent, reports `DT_UNKNOWN`.
  - Reference: `/root/linux/fs/ext2/dir.c:306-309`
- **Diff**: Asterinas spec does not condition file_type usage on the FILETYPE feature flag. Since the superblock spec requires FILETYPE as the only allowed incompat feature, this is safe in practice but not explicitly guarded.
