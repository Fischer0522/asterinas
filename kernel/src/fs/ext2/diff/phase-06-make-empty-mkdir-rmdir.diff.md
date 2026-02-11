# Phase 06 Diff Report: make_empty / mkdir / rmdir

## Scope

- Spec: `kernel/src/fs/ext2/spec/phase-06-make-empty-mkdir-rmdir.spec`
- Rust:
  - `kernel/src/fs/ext2/fs.rs`
  - `kernel/src/fs/ext2/inode.rs`
- Linux references:
  - `/root/linux/fs/ext2/ialloc.c:419` (`ext2_new_inode`)
  - `/root/linux/fs/ext2/dir.c:617` (`ext2_make_empty`)
  - `/root/linux/fs/ext2/dir.c:659` (`ext2_empty_dir`)
  - `/root/linux/fs/ext2/namei.c:228` (`ext2_mkdir`)
  - `/root/linux/fs/ext2/namei.c:302` (`ext2_rmdir`)

## Verification Summary

- Functional logic is aligned with Linux intent for:
  - inode creation entry point (`create_inode` as `ext2_new_inode` mapping)
  - directory first-chunk initialization (`make_empty`)
  - empty-directory check (`empty_dir`)
  - directory create/remove state machine (`mkdir` / `rmdir`)
- Current differences are intentional phase-level adaptations, not correctness bugs.

## Intentional DIFFs

### 1) Inode allocation policy is simplified

- Linux: `ext2_new_inode` uses Orlov/quadratic group selection and additional heuristics.
  - Reference: `/root/linux/fs/ext2/ialloc.c:419`
- Asterinas: `alloc_inode` scans groups cyclically from parent group with free-inode counters.
  - Rust: `kernel/src/fs/ext2/fs.rs:521`
- Reason: bring-up phase keeps allocator simple while preserving allocation correctness.

### 2) `create_inode` initializes a reduced field set

- Linux: initializes uid/gid, timestamps, flags, generation, ACL/security-related fields.
  - Reference: `/root/linux/fs/ext2/ialloc.c:539`
- Asterinas: initializes mode/links/size/blocks/block pointers and persists descriptor.
  - Rust: `kernel/src/fs/ext2/fs.rs:593`
- Reason: phase scope focuses on directory mutation path; non-critical metadata fields deferred.

### 3) No `i_dir_start_lookup` lookup hint

- Linux: `ext2_find_entry` starts from hint and wraps.
  - Reference: `/root/linux/fs/ext2/dir.c:356`
- Asterinas: linear scan from block 0.
  - Rust: `kernel/src/fs/ext2/inode.rs:374`
- Reason: optimization deferred; lookup semantics remain correct.

### 4) Timestamp/dirsync behavior deferred

- Linux: `ext2_add_link` / `ext2_delete_entry` update ctime/mtime and run dirsync path.
  - References:
    - `/root/linux/fs/ext2/dir.c:554`
    - `/root/linux/fs/ext2/dir.c:608`
- Asterinas: timestamp updates are TODO; current path persists metadata directly.
  - Rust: `kernel/src/fs/ext2/inode.rs:1102`
- Reason: `SystemTime` initialization constraints in current Ext2 path.

### 5) Explicit directory-block cleanup on rollback/removal

- Linux: block release is reached via inode lifecycle chain
  - `discard_new_inode()/iput() -> ext2_evict_inode() -> ext2_truncate_blocks()`
  - References:
    - `/root/linux/fs/inode.c:1220`
    - `/root/linux/fs/ext2/inode.c:72`
- Asterinas: explicit cleanup helper is called in `mkdir` rollback and `rmdir` success paths.
  - Rust:
    - `kernel/src/fs/ext2/inode.rs:1082` (`release_dir_data_blocks_for_cleanup`)
    - `kernel/src/fs/ext2/inode.rs:298`
    - `kernel/src/fs/ext2/inode.rs:362`
- Reason: current Asterinas Ext2 has no unified evict+truncate pipeline.
- TODO: move cleanup into a shared truncate/evict path and make inode free path trigger it.

---

## Spec-vs-Linux Supplement (from spec review)

### S1) phase-06-dir-mutation.spec: `add_entry` Directory Expansion Model

- **Spec**: When no slot exists in existing blocks, allocates one new data block via `alloc_blocks(1)`, initializes one free chunk, links into directory block pointer set, updates inode size/blocks.
- **Linux**: `ext2_add_link()` iterates `n = 0` to `npages` (inclusive — one past current page count). When `n == npages`, it hits `i_size` boundary and initializes a new chunk via `ext2_prepare_chunk` + page cache expansion.
  - Reference: `/root/linux/fs/ext2/dir.c:496-513`
- **Diff**: Linux expands via page cache (folio allocation + `ext2_prepare_chunk`). Asterinas expands via explicit `alloc_blocks(1)` + manual block linking. Both achieve the same result but through different I/O paths.

### S2) phase-06-dir-mutation.spec: `delete_entry` Merge Strategy

- **Spec**: Finds previous entry, merges space by extending `prev.rec_len` across removed entry span, sets removed entry `inode = 0`.
- **Linux**: `ext2_delete_entry()` does the same: walks from chunk start to find `pde` (previous entry), extends `pde->rec_len` to cover deleted entry, sets `dir->inode = 0`.
  - Reference: `/root/linux/fs/ext2/dir.c:571-611`
- **Status**: Aligned.

### S3) phase-06-make-empty-mkdir-rmdir.spec: `make_empty` Block Allocation Model

- **Spec**: Allocates exactly one new data block for logical block 0 via `alloc_blocks(1)`. Zero-fills the whole chunk before writing `.` and `..` entries. Updates `size = chunk_size` and `blocks += chunk_size / SECTOR_SIZE`.
- **Linux**: `ext2_make_empty()` uses `filemap_grab_folio(inode->i_mapping, 0)` + `ext2_prepare_chunk(folio, 0, chunk_size)` which triggers `ext2_get_block` with `create=1` to allocate the block through the page cache path.
  - Reference: `/root/linux/fs/ext2/dir.c:617-655`
- **Diff**: Linux allocates via page cache + `ext2_get_block(create=1)`. Asterinas allocates via explicit `alloc_blocks(1)` + manual block pointer assignment. The `.`/`..` entry layout is identical.

### S4) phase-06-make-empty-mkdir-rmdir.spec: `mkdir` State Machine Ordering

- **Spec**: 1) Increment parent link count, 2) `create_inode`, 3) `make_empty`, 4) `add_entry`, 5) persist metadata.
- **Linux**: `ext2_mkdir()` follows: 1) `dquot_initialize`, 2) `inode_inc_link_count(dir)`, 3) `ext2_new_inode`, 4) `ext2_make_empty`, 5) `ext2_add_link`, 6) `d_instantiate_new`.
  - Reference: `/root/linux/fs/ext2/namei.c:228-270`
- **Status**: Aligned in ordering. Asterinas omits quota and dentry instantiation steps.

### S5) phase-06-make-empty-mkdir-rmdir.spec: `mkdir` Rollback Completeness

- **Spec**: Rollback requires: decrement parent link count, free child inode via `free_inode`, release child data block, remove parent entry if already inserted. "No leaked inode bits, data blocks, or directory entries."
- **Linux**: `ext2_mkdir()` rollback uses `discard_new_inode()` which triggers `ext2_evict_inode()` → `ext2_truncate_blocks()` → bitmap free. The VFS inode lifecycle handles cleanup automatically.
  - Reference: `/root/linux/fs/ext2/namei.c:260-268`
- **Diff**: Linux relies on VFS inode eviction for cleanup. Asterinas must manually unwind each step. The spec's rollback is more explicit but also more error-prone — any missed step leaks resources.

### S6) phase-06-make-empty-mkdir-rmdir.spec: `rmdir` Link Count Handling

- **Spec**: Child inode: `links_count -= 2` (for `.` and `..`). Parent inode: `links_count -= 1`.
- **Linux**: `ext2_rmdir()` calls `inode_dec_link_count(inode)` twice (for `.` and the dir entry) and `inode_dec_link_count(dir)` once.
  - Reference: `/root/linux/fs/ext2/namei.c:316-320`
- **Status**: Aligned. Both reduce child by 2 and parent by 1.

### S7) phase-06-make-empty-mkdir-rmdir.spec: `empty_dir` Corruption Handling

- **Spec**: Returns `false` on any block read/parsing failure, corrupt stream (`rec_len == 0`), or unexpected live entries. `.` entry inode must equal this inode number.
- **Linux**: `ext2_empty_dir()` returns 0 (not empty) on folio read failure or corrupt entries. Also checks `.` inode matches `dir->i_ino`.
  - Reference: `/root/linux/fs/ext2/dir.c:659-710`
- **Status**: Aligned. Both treat corruption as "not empty" for `rmdir` gating.

### S8) phase-06-make-empty-mkdir-rmdir.spec: Lock Ordering

- **Spec**: Parent inode number first, then child inode number (ascending order). Global ordering: `SuperBlock -> BlockGroup -> Inode`.
- **Linux**: VFS provides `inode_lock(dir)` before calling `ext2_mkdir`/`ext2_rmdir`. Child inode lock is not explicitly taken by ext2 — VFS handles it via `d_instantiate_new` / `iput`.
  - Reference: `/root/linux/fs/ext2/namei.c:228`
- **Diff**: Asterinas uses explicit ascending inode-number lock ordering. Linux relies on VFS-level `i_rwsem` ordering conventions.

## Notes

- This diff document tracks both **intentional** divergences and **spec-vs-Linux** analysis.
- If phase scope expands (timestamps, full inode lifecycle, Orlov allocator), this file should be updated accordingly.
