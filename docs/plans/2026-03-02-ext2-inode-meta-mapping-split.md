# Ext2 Inode Meta/Mapping Split Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Split Ext2 inode state into independent metadata vs block-mapping domains to remove PageCache callback self-deadlocks, simplify lock choreography (especially direntry ops), and keep Linux ext2 semantics.

**Architecture:** Replace the monolithic `RwMutex<InodeInner>` with `InodeInner { meta: RwMutex<InodeMeta>, mapping: RwMutex<InodeMapping>, page_cache: PageCache }` and split `InodeDesc` into `InodeMetaDesc + InodeMappingDesc`. Persist by assembling `RawInode` from both parts (no dual-write). PageCache backend callbacks must only take `mapping.read()`.

**Tech Stack:** Rust (no_std), `ostd::sync::RwMutex`, `crate::fs::utils::PageCache`, ext2 `RawInode`, `#[ktest]` + `Ext2FixtureBuilder`.

---

## Pre-flight (do once)

### Task 0: Verify baseline before refactor

**Files:** none

**Step 1: Build current kernel**

Run: `make kernel`

**Step 2: Run current kernel tests**

Run: `make ktest`

**Step 3: Record baseline**

Note down any existing flaky tests or known failures before changing code.

---

## Phase 1: Define Split Descriptors + RawInode Conversions

Goal of this phase: introduce `InodeMetaDesc` and `InodeMappingDesc` plus conversion helpers, with tests, while keeping production behavior unchanged.

### Task 1: Add split descriptor types (no wiring yet)

**Files:**
- Modify: `kernel/src/fs/ext2/inode.rs`

**Step 1: Write failing tests for split conversions**

- Add new `#[ktest]` cases near existing `desc_try_from_*` tests that call new APIs:
  - `InodeMetaDesc::try_from_raw`
  - `InodeMappingDesc::from_raw`
  - `RawInode::from_parts`
- Expected at this step: compile fails (APIs not implemented).

**Step 2: Run kernel tests to confirm failure**

Run: `cd kernel && cargo osdk test`

Expected: build/test fails due to missing symbols.

**Step 3: Implement the split descriptor structs**

- Add `InodeMetaDesc` containing all fields except `i_blocks` and `i_block[15]`.
- Add `InodeMappingDesc { blocks: u32, block_ptrs: [u32; 15] }`.
- Keep existing `InodeDesc` temporarily unchanged (migration will happen later).

**Step 4: Implement RawInode <-> split descriptor conversions**

- Implement `InodeMetaDesc::try_from_raw(raw: &RawInode) -> Result<(InodeType, InodeMetaDesc)>` by porting logic from `impl TryFrom<&RawInode> for InodeDesc`:
  - Deleted inode -> `Err(ESTALE)`
  - Invalid flags -> `Err(EIO)`
  - Size overflow -> `Err(EUCLEAN)`
- Implement `InodeMappingDesc::from_raw(raw: &RawInode) -> InodeMappingDesc`:
  - `blocks = raw.blocks`
  - `block_ptrs = raw.block`
- Implement `RawInode::from_parts(type_, meta, mapping)`:
  - Must preserve existing on-disk encoding rules (size_hi only for regular files, etc.).

**Step 5: Run kernel tests**

Run: `cd kernel && cargo osdk test`

Expected: tests pass.

**Step 6: Commit**

Run:
`git add kernel/src/fs/ext2/inode.rs`

Commit message (example):
`refactor(ext2): introduce split inode desc conversions`

---

## Phase 2: Define Meta/Mapping Domain APIs (still not rewiring locks)

Goal of this phase: define method surfaces and move pure logic onto meta/mapping types, but keep the outer inode data layout mostly intact.

### Task 2: Introduce InodeMeta/InodeMapping containers with Dirty tracking

**Files:**
- Modify: `kernel/src/fs/ext2/inode.rs`

**Step 1: Add structs + minimal APIs (compile only)**

- Define:
  - `struct InodeMeta { desc: Dirty<InodeMetaDesc>, is_freed: bool }`
  - `struct InodeMapping { desc: Dirty<InodeMappingDesc> }`

**Step 2: Add thin getters/setters used widely**

- Implement minimal methods needed for compilation later:
  - Meta: `file_size()`, `set_file_size()`, `mode/set_mode`, `uid/gid`, `links_count`, `atime/mtime/ctime`, `file_acl`, `flags`, `generation`.
  - Mapping: `blocks_512()`, `block_ptrs()` accessors.

**Step 3: Add unit tests for Dirty behavior (optional)**

- If adding helpers that rely on dirty tracking, add a small `#[ktest]` verifying `Dirty<T>` is set when mutably deref'ing the descriptor.

**Step 4: Build**

Run: `make kernel`

**Step 5: Commit**

Run:
`git add kernel/src/fs/ext2/inode.rs`

Commit message (example):
`refactor(ext2): add meta/mapping domain containers`

### Task 3: Move block mapping logic onto InodeMapping (API + internal helpers)

**Files:**
- Modify: `kernel/src/fs/ext2/inode.rs`

**Step 1: Write/adjust tests for mapping logic on the new type**

- Identify one existing ktest that exercises `get_or_alloc_block` / indirect traversal.
- Duplicate it (or parameterize it) to call through `InodeMapping::*` once wired.
- For now it can remain TODO/skipped until wiring exists.

**Step 2: Introduce mapping methods with explicit `&Ext2` parameter**

- Add skeleton signatures on `impl InodeMapping`:
  - `block_to_path(fs, iblock)`
  - `get_block(fs, iblock)`
  - `get_or_alloc_block(fs, iblock, create)`
  - `truncate_blocks(fs, new_size)`

**Step 3: Port logic from InodeInner incrementally**

- Move pure mapping code that only depends on `block_ptrs/blocks` and `fs`.
- IMPORTANT: mapping code must not update meta timestamps directly; return a flag or rely on dirty state and let the caller update `ctime/mtime`.

**Step 4: Build**

Run: `make kernel`

**Step 5: Commit**

Commit message (example):
`refactor(ext2): extract mapping operations to InodeMapping`

---

## Phase 3: Introduce Split-Lock InodeInner + PageCacheBackend Rules

Goal of this phase: change the data layout so PageCache callbacks stop depending on the monolithic inode lock.

### Task 4: Replace `RwMutex<InodeInner>` with `InodeInner { meta, mapping, page_cache }`

**Files:**
- Modify: `kernel/src/fs/ext2/inode.rs`
- Modify: `kernel/src/fs/ext2/impl_for_vfs/inode.rs`

**Step 1: Add compilation-only shim methods**

- Implement `InodeInner::{meta_read, meta_write, mapping_read, mapping_write, page_cache}`.
- Keep `Inode::new` building PageCache using `Arc::new_cyclic` and backend `Weak<Inode> as Weak<dyn PageCacheBackend>`.

**Step 2: Wire Inode construction to split descriptors**

- Where we currently do `let desc = InodeDesc::try_from(&raw)?;`:
  - Parse `(type_, meta) = InodeMetaDesc::try_from_raw(&raw)?`
  - Parse `mapping = InodeMappingDesc::from_raw(&raw)`
  - Construct `InodeMeta { desc: Dirty::new(meta), ... }` and `InodeMapping { desc: Dirty::new(mapping) }`

**Step 3: Update PageCacheBackend impl**

- `read_page_async/write_page_async`: only take `mapping.read()` and call `mapping.get_block(...)`.
- `npages()`: derive from `page_cache.pages().size()` (avoid taking meta).

**Step 4: Build**

Run: `make kernel`

**Step 5: Commit**

Commit message (example):
`refactor(ext2): split inode inner locks for meta vs mapping`

### Task 5: Implement persistence from split descriptors

**Files:**
- Modify: `kernel/src/fs/ext2/inode.rs`

**Step 1: Write failing test for persistence assembly**

- Add a ktest that:
  - creates an inode, mutates meta + mapping, calls persist path,
  - reads back raw inode from inode table (via existing testkit helper),
  - asserts `raw.block[]` and `raw.blocks` match mapping, and meta fields match.

**Step 2: Implement `persist_inode_locked`**

- Add `InodeInner::persist_inode_locked(meta, mapping, ino, type_, fs)`:
  - assemble `RawInode::from_parts(type_, &meta.desc, &mapping.desc)`
  - call `fs.write_inode_desc(ino, &raw)`
  - clear both dirty flags on success

**Step 3: Run tests**

Run: `cd kernel && cargo osdk test`

**Step 4: Commit**

Commit message (example):
`refactor(ext2): persist RawInode from split meta/mapping`

---

## Phase 4: Rewire Inode Operations (remove upread choreography)

Goal of this phase: update `Inode` methods (file IO, direntry ops, sync, eviction, xattr) to use split locks with the new rules.

### Task 6: Update file buffered IO (`read_at`, `write_at`, `resize`)

**Files:**
- Modify: `kernel/src/fs/ext2/inode.rs`

**Step 1: Write a deadlock regression test**

- Add a `#[ktest]` that would deadlock under the old monolithic lock design:
  - Hold `meta.write()` and perform a VMO operation that triggers PageCache commit/writeback
  - Ensure it completes.

**Step 2: Rewire `read_at`**

- Use `meta.read()` to obtain size, then call VMO read.

**Step 3: Rewire `write_at` with explicit allocation under mapping.write**

- Under `meta.write()`, allocate required blocks under `mapping.write()`.
- Drop `mapping.write()` before any VMO/PageCache operations.
- Preserve Linux-compatible `ENOSPC` timing (must return from `write_at`).

**Step 4: Rewire `resize`**

- Grow: update PageCache size and meta size.
- Shrink: ensure tail zeroing and truncate do not hold `mapping.write()` across VMO ops.

**Step 5: Run tests**

Run: `cd kernel && cargo osdk test`

**Step 6: Commit**

Commit message (example):
`refactor(ext2): rewire buffered IO to split meta/mapping locks`

### Task 7: Update direct IO paths

**Files:**
- Modify: `kernel/src/fs/ext2/inode.rs`

**Step 1: Rewire `read_direct_at`/`write_direct_at`**

- Ensure eviction/writeback happens without holding `mapping.write()`.

**Step 2: Run tests**

Run: `cd kernel && cargo osdk test`

**Step 3: Commit**

Commit message (example):
`refactor(ext2): rewire direct IO paths for split locks`

### Task 8: Update direntry operations (serialize under meta.write)

**Files:**
- Modify: `kernel/src/fs/ext2/inode.rs`

**Step 1: Replace upread/upgrade choreography**

- `lookup/readdir_at`: take `meta.read()` (or `meta.write()` if simplest) and perform VMO reads.
- `add_entry/delete_entry/set_link/create`:
  - Take `meta.write()` for the duration.
  - If growth/allocation needed: take `mapping.write()` only around the allocation step, then drop.
  - Perform direntry modifications through PageCache/VMO.
  - Persist via `persist_inode_locked`.

**Step 2: Run tests**

Run: `cd kernel && cargo osdk test`

**Step 3: Commit**

Commit message (example):
`refactor(ext2): simplify direntry ops with split locks`

### Task 9: Update metadata/xattr/eviction/sync

**Files:**
- Modify: `kernel/src/fs/ext2/inode.rs`
- Modify: `kernel/src/fs/ext2/impl_for_vfs/inode.rs`

**Step 1: Rewire metadata getters**

- `metadata()` must read `size`/times from meta and `blocks` from mapping.

**Step 2: Rewire xattr updates**

- `set_xattr` must update `meta.desc.file_acl` (not mapping).

**Step 3: Rewire `sync_data/sync_all/prepare_for_evict`**

- Ensure data writeback path does not need meta write lock and never triggers backend deadlock.

**Step 4: Run full build + tests + format**

Run:
- `make kernel`
- `make ktest`
- `make format`

**Step 5: Commit**

Commit message (example):
`refactor(ext2): complete inode split-lock migration`

---

## Notes / Guardrails

- PageCache callbacks (`PageCacheBackend::{read_page_async, write_page_async, npages}`) MUST NOT acquire the meta lock.
- Never hold `mapping.write()` across VMO/PageCache operations that may trigger pager callbacks.
- Block allocation remains explicit in inode write paths (do not allocate in writeback).
- Directory mutations are serialized (block concurrent lookup/readdir during mutation).
