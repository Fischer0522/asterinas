# Asterinas Ext2 Detailed Roadmap (Phase + Module + Spec)

## Conventions
- Linux source of truth: `/root/linux` (case-sensitive). All Linux references are file:line.
- Each module produces one spec under `kernel/src/fs/ext2/spec/`.
- Rust style: OOP `impl` methods only, no unsafe, no panic/assert, Asterinas infra only.
- Isolation wall: do not reuse ext2_old logic (only trait signatures/registration patterns).

## Unsupported Features
The following Linux Ext2 features are explicitly out of scope:
- **Quota**: User/group disk usage limits (`CONFIG_QUOTA`, `i_dquot[]` in `ext2_inode_info`)
- **DAX (Direct Access)**: Persistent memory direct mapping (`-o dax`, `s_daxdev` in `ext2_sb_info`)
- **fiemap**: Physical block mapping ioctl (`FS_IOC_FIEMAP`, `ext2_fiemap()` in `inode.c`)

## Shared Asterinas Adaptation Patterns (apply everywhere)
- I/O: `BlockDevice` + `PageCache` + `BioWaiter` instead of `buffer_head` chains.
- Sync: `RwMutex`/`Mutex`/`SpinLock` with lock ordering from `developing-ext2`.
- Caches: `BTreeMap` and `Arc<Weak<_>>` caches instead of Linux hash tables.
- Errors: `Result<T>` + `Errno` mapping, no asserts.

## Naming & API Alignment (current code)
- Ext2 inode struct is `Inode` (no `Ext2Inode` type yet).
- Block mapping helper is `BlockPath` (not `BlockPtrs`).
- Bitmap reads use `IdBitmap` (no `BlockBitmap`/`InodeBitmap` wrappers yet).
- Directory operations are methods on `Inode`.

---

## Progress Summary (updated 2026-02-25)

| Phase | Description | Status |
|-------|-------------|--------|
| 0 | Scaffolding & Interfaces | ✅ Complete |
| 1 | On-Disk Structures & Superblock | ✅ Complete |
| 2 | Block Groups & Bitmaps | ✅ Complete |
| 3 | Inode Read Path & Block Mapping | ✅ Complete |
| 4 | Directory Read Path | ✅ Complete |
| 5 | Allocation + Write Enable | ✅ Complete (minor gaps: no Orlov, no reserved-block gate) |
| 6 | Directory Mutation & File Creation | ✅ Complete |
| 7 | File Write, Resize, Truncate | ✅ Complete (known issue: PageCache::discard_range bug) |
| 8 | Metadata & Special Files | ✅ Complete (incl. fallocate) |
| 9 | Robustness & Compliance | ⚠️ Partial (sync done; feature gating, reserved blocks, boundary checks missing) |
| 10 | Extended Attributes & ioctl | ❌ Not started |
| 11 | Orphan Inode & Crash Recovery | ⚠️ Partial (evict_inode done; orphan list/cleanup missing) |

### Open TODOs in code
- `inode.rs:407` — rollback path on failed allocation/write
- `inode.rs:1422` — refactor fast symlink detection logic
- `inode.rs:3133` — simplify mask logic
- `inode.rs:3248` — shared truncate/evict pipeline
- `inode.rs:3898` — refactor with enum approach
- `inode.rs:4070` — behavior differs from Linux
- `inode.rs:5920` — test blocked by PageCache::discard_range bug

---

## Phase 0: Scaffolding & Interfaces
Goal: establish module layout, VFS glue, and basic types with no on-disk logic.

### Module 0.1: Ext2 Core Skeleton
- New/extend structs:
  - `Ext2` (core filesystem struct).
  - `Ext2Type` (filesystem registration glue).
- Methods:
  - `Ext2::open(block_device)` (signature only, returns stub error).
  - `Ext2::root_inode()` (stub).
- Linux refs: none (scaffolding only).
- Asterinas adjustments: use `Arc`/`Weak` and `RwMutex` for future state.
- Spec: `phase-00-core-skeleton.spec`.
- Status: ✅ implemented (`Ext2::open` fully mounts the filesystem; `root_inode()` returns cached root inode).

### Module 0.2: VFS Glue Interfaces
- New/extend structs:
  - `impl FileSystem for Ext2` (from `kernel/src/fs/utils`).
  - `impl Inode for Inode` (stub trait impls).
- Methods:
  - `FileSystem::sync`, `FileSystem::root_inode`, `FileSystem::sb`.
- Linux refs: none (glue only).
- Asterinas adjustments: follow VFS trait expectations and error model.
- Spec: `phase-00-vfs-glue.spec`.
- Status: ✅ implemented (`impl FileSystem for Ext2` in `impl_for_vfs/fs.rs`; `impl VfsInode for Inode` in `impl_for_vfs/inode.rs` with full trait coverage).

---

## Phase 1: On-Disk Structures & Superblock
Goal: parse/validate on-disk structures and mount read-only.

### Module 1.1: Raw On-Disk Structures
- New structs:
  - `RawSuperBlock`, `RawGroupDesc`, `RawInode`, `RawDirEntry` (`#[repr(C)] + Pod`).
- Methods:
- Linux refs:
  - `/root/linux/include/linux/ext2_fs.h:103` (superblock).
  - `/root/linux/include/linux/ext2_fs.h:180` (group desc).
  - `/root/linux/include/linux/ext2_fs.h:227` (inode).
  - `/root/linux/include/linux/ext2_fs.h:295` (dir entry).
- Asterinas adjustments:
  - use `Pod` layouts with `VmReader`/`VmIo`; no manual endian swaps.
- Spec: `phase-01-raw-structures.spec`.
- Status: ✅ implemented (RawSuperBlock/RawGroupDesc/RawInode/RawDirEntry).

### Module 1.2: Superblock Read/Validate
- New/extend structs:
  - `SuperBlock` (in-memory), `FeatureCompatSet`, `FeatureInCompatSet`, `FeatureRoCompatSet`.
- Methods:
  - `impl TryFrom<RawSuperBlock> for SuperBlock`.
  - `load_super_block(device, read_only)`.
- Linux refs:
  - `/root/linux/fs/ext2/super.c:800+` (`ext2_fill_super`).
  - `/root/linux/fs/ext2/super.c:645` (`ext2_setup_super`).
- Asterinas adjustments:
  - direct `BlockDevice::read_val` from `SUPER_BLOCK_OFFSET`.
  - block size fixed to 4096 (per project constraint), restrict features accordingly.
  - error handling via `Errno` mapping.
- Spec: `phase-01-superblock.spec`.
- Status: ✅ implemented (load_super_block + TryFrom).

### Module 1.3: Group Descriptor Table Load
- New/extend structs: none (use `USegment` + `RawGroupDesc`).
- Methods:
  - `Ext2::load_group_desc_table(sb)`.
  - `Ext2::check_group_desc_table(sb, group_descs)`.
- Linux refs:
  - `/root/linux/fs/ext2/super.c:695` (`ext2_check_descriptors`).
- Asterinas adjustments:
  - read into `USegment` via `BlockDevice` I/O (no PageCache yet).
- Spec: `phase-01-group-desc-table.spec`.
- Status: ✅ implemented (load/check group descriptor table).

---

## Phase 2: Block Groups & Bitmaps (Read-Only)
Goal: load/validate block group metadata and bitmap read paths.

### Module 2.1: Block Group Cache
- New/extend structs:
  - `BlockGroup` (descriptor + counters + bitmap cache state).
- Methods:
  - `BlockGroup::load(group_idx)`.
  - `BlockGroup::free_blocks_count()` / `free_inodes_count()`.
- Linux refs:
  - `/root/linux/fs/ext2/super.c` (group validation logic).
- Asterinas adjustments:
  - store descriptor in `RwMutex<Dirty<_>>`.
- Spec: `phase-02-block-group-cache.spec`.
- Status: ✅ implemented (BlockGroup load + counters + per-group inode cache via BTreeMap).

### Module 2.2: Block Bitmap Read Path
- New/extend structs: none (use `IdBitmap`).
- Methods:
  - `BlockGroup::load_block_bitmap(fs, sb)`.
  - `IdBitmap::is_allocated(bit)` (existing helper).
- Linux refs:
  - `/root/linux/fs/ext2/balloc.c:128` (`read_block_bitmap`).
- Asterinas adjustments:
  - direct block reads into a buffer; PageCache integration later.
- Spec: `phase-02-block-bitmap-read.spec`.
- Status: ✅ implemented (load_block_bitmap; bitmap kept in memory).

### Module 2.3: Inode Bitmap Read Path
- New/extend structs: none (use `IdBitmap`).
- Methods:
  - `BlockGroup::load_inode_bitmap(fs, sb)`.
  - `IdBitmap::is_allocated(bit)` (existing helper).
- Linux refs:
  - `/root/linux/fs/ext2/ialloc.c:79` (bitmap access patterns).
- Asterinas adjustments:
  - same direct-read approach as block bitmap.
- Spec: `phase-02-inode-bitmap-read.spec`.
- Status: ✅ implemented (load_inode_bitmap; bitmap kept in memory).

---

## Phase 3: Inode Read Path & Block Mapping
Goal: read inode from disk, map logical to physical blocks, enable read-only file access.

### Module 3.1: Inode Table I/O
- New/extend structs:
  - `InodeDesc` (in-memory).
- Methods:
  - `Ext2::read_inode_desc(ino)`.
  - `Ext2::inode_table_block(group, index)`.
- Linux refs:
  - `/root/linux/fs/ext2/inode.c:1314` (`ext2_get_inode`).
- Asterinas adjustments:
  - compute offsets using `block_size` and `inode_size`.
- Spec: `phase-03-inode-table-io.spec`.
- Status: ✅ implemented (read_inode_desc + inode_table_block; PageCache-backed inode table via InodeTableBackend).

### Module 3.2: Inode Cache & Instantiation
- New/extend structs:
  - `Inode` (Asterinas inode wrapper).
- Methods:
  - `Ext2::read_inode(ino)` (load path only).
  - `Inode::from_desc(ino, desc, fs)`.
- Linux refs:
  - `/root/linux/fs/ext2/inode.c:1387` (`ext2_iget`).
- Asterinas adjustments:
  - inode cache deferred; will use `BTreeMap<u32, Weak<Inode>>` when added.
- Spec: `phase-03-inode-cache.spec`.
- Status: ✅ implemented (read_inode with per-group BTreeMap<u32, Weak<Inode>> cache; eviction integrated into sync_all_inodes).

### Module 3.3: Logical-to-Physical Block Mapping
- New/extend structs:
  - `BlockPath` helper (direct/indirect indices).
- Methods:
  - `Inode::block_to_path(logical_block)`.
  - `Inode::get_block(logical_block)`.
- Linux refs:
  - `/root/linux/fs/ext2/inode.c:163` (`ext2_block_to_path`).
  - `/root/linux/fs/ext2/inode.c:783` (`ext2_get_block`).
- Asterinas adjustments:
  - indirect blocks read via `BlockDevice` into buffers; PageCache integration later.
- Spec: `phase-03-block-mapping.spec`.
- Status: ✅ implemented (block_to_path + get_block + get_or_alloc_block; direct/single/double/triple indirect).

### Module 3.4: File Read Path (read-only)
- Methods:
  - `Inode::read_at(offset, buf)`.
  - `Inode::read_page(idx)` (PageCache backend hook).
- Linux refs:
  - `/root/linux/fs/ext2/file.c` (read semantics).
  - `/root/linux/fs/ext2/inode.c:783` (`ext2_get_block`).
- Asterinas adjustments:
  - use PageCache for data pages; fall back to direct block reads until PageCache is wired.
- Spec: `phase-03-file-read.spec`.
- Status: ✅ implemented (read_at via PageCache; read_direct_at for O_DIRECT; read_page_async implements PageCacheBackend).

---

## Phase 4: Directory Read Path
Goal: lookup and readdir in read-only mode.

### Module 4.1: Directory Entry Parsing
- New/extend structs:
  - `DirEntry` (view helper), `DirEntryIter`.
- Methods:
  - `DirEntry::validate(rec_len, name_len)`.
- Linux refs:
  - `/root/linux/fs/ext2/dir.c:99` (`ext2_check_folio`).
- Asterinas adjustments:
  - parse entries from raw block buffers; enforce 4-byte alignment.
- Spec: `phase-04-dir-entry-parse.spec`.
- Status: ✅ implemented (DirEntry/DirEntryIter with validation and 4-byte alignment).

### Module 4.2: Lookup / Readdir
- Methods:
  - `Inode::find_entry(name)`.
  - `Inode::readdir_at(pos, visitor)`.
- Linux refs:
  - `/root/linux/fs/ext2/dir.c:342` (`ext2_find_entry`).
  - `/root/linux/fs/ext2/namei.c` (lookup semantics).
- Asterinas adjustments:
  - return `Errno::ENOENT` on miss, integrate `DirentVisitor`.
- Spec: `phase-04-dir-lookup-readdir.spec`.
- Status: ✅ implemented; linux-logic-verify pass (2026-02-05).

---

## Phase 5: Allocation (Blocks & Inodes) + Write Enable
Goal: enable writable mount and allocation semantics.

### Module 5.1: Block Allocation Core
- New/extend structs:
  - `BlockAllocator` (per-fs or per-group).
- Methods:
  - `Ext2::alloc_blocks(count)`.
  - `Ext2::free_blocks(start, count)`.
- Linux refs:
  - `/root/linux/fs/ext2/balloc.c:1208` (`ext2_new_blocks`).
  - `/root/linux/fs/ext2/balloc.c:482` (`ext2_free_blocks`).
- Asterinas adjustments:
  - direct bitmap read/write via `BlockDevice`; `IdBitmap::alloc_consecutive` with halving fallback.
- Spec: `phase-05-block-alloc.spec`.
- Status: ✅ implemented (`BlockGroup::alloc_blocks` / `BlockGroup::free_blocks` with bitmap operations).
- Known gaps vs Linux:
  - No goal-based placement heuristic (`find_next_usable_block` path).
  - Reserved-block policy (`ext2_has_free_blocks`) not implemented yet.

### Module 5.2: Inode Allocation Core
- New/extend structs:
  - `InodeAllocator`.
- Methods:
  - `Ext2::alloc_inode(parent_ino, inode_type)`.
  - `Ext2::free_inode(ino)`.
- Linux refs:
  - `/root/linux/fs/ext2/ialloc.c:419` (`ext2_new_inode`).
  - `/root/linux/fs/ext2/ialloc.c:79` (`ext2_free_inode`).
- Asterinas adjustments:
  - Current implementation uses cyclic scan from parent group; Orlov policy is not implemented yet.
- Spec: `phase-05-inode-alloc.spec`.
- Status: ✅ implemented (`BlockGroup::alloc_inode` / `BlockGroup::free_inode`; cyclic scan from parent group).
- Known gap: Orlov directory allocation policy not implemented.

### Module 5.3: Writable Superblock / Group Counters
- Methods:
  - `SuperBlock::inc/dec_free_blocks`, `inc/dec_free_inodes`.
  - `BlockGroup::inc/dec_free_*`.
- Linux refs:
  - `/root/linux/fs/ext2/balloc.c` and `/root/linux/fs/ext2/ialloc.c` counter updates.
- Asterinas adjustments:
  - atomicity via `RwMutex` and `Dirty`.
- Spec: `phase-05-counter-accounting.spec`.
- Status: ✅ implemented (`sync_metadata` + group/super counter updates via `Dirty` tracking).

### Phase 5 Gate Snapshot (2026-02-06)
- Spec: pass (phase-05 specs complete).
- Implementation: ✅ pass (core methods present and functional).
- linux-logic-verify: pending (no recorded pass yet).

---

## Phase 6: Directory Mutation & File Creation
Goal: create/remove/rename entries and update link counts.

### Module 6.1: Add/Delete Dir Entries
- Methods:
  - `Inode::add_entry(name, ino, file_type)`.
  - `Inode::delete_entry(name)`.
- Linux refs:
  - `/root/linux/fs/ext2/dir.c:476` (`ext2_add_link`).
  - `/root/linux/fs/ext2/dir.c:571` (`ext2_delete_entry`).
- Asterinas adjustments:
  - update PageCache-backed dir blocks; maintain alignment.
- Spec: `phase-06-dir-mutation.spec`.
- Status: ✅ implemented (add_entry/delete_entry as methods on Inode with PageCache-backed dir blocks).

### Module 6.2: make_empty / mkdir / rmdir
- Methods:
  - `Inode::make_empty(parent_ino)`.
  - `Inode::rmdir(name)`.
- Linux refs:
  - `/root/linux/fs/ext2/dir.c:617` (`ext2_make_empty`).
  - `/root/linux/fs/ext2/namei.c:228` (`ext2_mkdir`).
- Asterinas adjustments:
  - update link counts and timestamps via inode methods.
- Spec: `phase-06-dir-create-remove.spec`.
- Status: ✅ implemented (`mkdir` at inode.rs:931, `rmdir` at inode.rs:877; link counts and timestamps updated).

### Module 6.3: Namei Ops (create/link/unlink/rename)
- Methods:
  - `Inode::create`, `link`, `unlink`, `rename`.
- Linux refs:
  - `/root/linux/fs/ext2/namei.c:102` (`ext2_create`).
  - `/root/linux/fs/ext2/namei.c:273` (`ext2_unlink`).
  - `/root/linux/fs/ext2/namei.c:318` (`ext2_rename`).
- Asterinas adjustments:
  - use Asterinas inode/type enums; no raw pointers.
- Spec: `phase-06-namei-ops.spec`.
- Status: ✅ implemented (`create` at inode.rs:3386, `link` at inode.rs:3446, `unlink` at inode.rs:3502, `rename` at inode.rs:3552 with same-dir and cross-dir paths).

---

## Phase 7: File Write, Resize, Truncate (split modules)
Goal: full data write path and size changes with correct block tree updates.

### Module 7.1: File Write Path
- Methods:
  - `Inode::write_at(offset, data)`.
  - `Inode::write_page(idx)` (PageCache backend hook).
- Linux refs:
  - `/root/linux/fs/ext2/file.c` (write semantics).
  - `/root/linux/fs/ext2/inode.c:783` (`ext2_get_block`).
- Asterinas adjustments:
  - use PageCache and async `BioWaiter` for flush.
- Spec: `phase-07-file-write.spec`.
- Status: ✅ implemented (`write_at` via PageCache; `write_direct_at` for O_DIRECT; `write_page_async` implements PageCacheBackend).

### Module 7.2: Resize (grow)
- Methods:
  - `Inode::resize(new_size)` (grow path).
  - `Inode::alloc_blocks_for_range`.
- Linux refs:
  - `/root/linux/fs/ext2/inode.c` (block allocation during write).
- Asterinas adjustments:
  - update `Dirty<InodeDesc>` and PageCache size.
- Spec: `phase-07-file-resize-grow.spec`.
- Status: ✅ implemented (`resize` with grow path; `get_or_alloc_block` allocates blocks on demand).
- Known TODO: rollback path on failed allocation/write (inode.rs:407).

### Module 7.3: Truncate (shrink + free)
- Methods:
  - `Inode::truncate(new_size)`.
  - `Inode::free_branches` (indirect tree free).
- Linux refs:
  - `/root/linux/fs/ext2/inode.c:1262` (`ext2_truncate_blocks`).
  - `/root/linux/fs/ext2/inode.c:1136` (`ext2_free_branches`).
- Asterinas adjustments:
  - use BlockAllocator + PageCache eviction.
- Spec: `phase-07-file-truncate.spec`.
- Status: ✅ implemented (`resize` with shrink path; indirect tree freeing via `free_branches`-equivalent logic).
- Known TODO: shared truncate/evict pipeline (inode.rs:3248).
- Known issue: PageCache::discard_range bug affects truncate tests (inode.rs:5920).

---

## Phase 8: Metadata & Special Files
Goal: symlink and special inode types with correct metadata behavior.

### Module 8.1: Symlinks
- Methods:
  - `Inode::read_link`, `write_link` (fast/slow).
- Linux refs:
  - `/root/linux/fs/ext2/inode.c` (symlink paths).
- Asterinas adjustments:
  - fast symlink stored in inode block pointers; slow via data blocks.
- Spec: `phase-08-symlink.spec`.
- Status: ✅ implemented (`read_link`/`write_link` with fast symlink in inode block pointers and slow symlink via data blocks; commit d8afce08).
- Known TODO: refactor fast symlink detection logic (inode.rs:1422-1423).

### Module 8.2: Special Files
- Methods:
  - `Inode::set_device_id`.
- Linux refs:
  - `/root/linux/fs/ext2/inode.c` (special file encoding).
- Asterinas adjustments:
  - map device IDs to `InodeType::CharDevice/BlockDevice`.
- Spec: `phase-08-special-files.spec`.
- Status: ✅ implemented (`mknod` with device ID encoding/decoding for char/block devices and named pipes; commit be13a2c0).

### Module 8.3: Metadata Updates
- Methods:
  - `set_atime/mtime/ctime`, `set_mode`, `set_owner`.
- Linux refs:
  - `/root/linux/fs/ext2/inode.c` (timestamp and chmod semantics).
- Asterinas adjustments:
  - use `UnixTime` and Asterinas credential APIs.
- Spec: `phase-08-metadata.spec`.
- Status: ✅ implemented (`set_mode`, `set_owner`, `set_group`, atime/mtime/ctime setters via VfsInode trait; `UnixTime` used for timestamps).

### Module 8.4: Fallocate
- Methods:
  - `Inode::fallocate(mode, offset, len)`.
- Linux refs:
  - `/root/linux/fs/ext2/file.c` (`ext2_fallocate`).
- Asterinas adjustments:
  - supports ALLOCATE, PUNCH_HOLE, KEEP_SIZE modes.
  - block preallocation via `get_or_alloc_block`; hole punching via block freeing + PageCache eviction.
- Spec: `phase-08-fallocate.spec`.
- Status: ✅ implemented (commit 5aaab809; VfsInode::fallocate wired in impl_for_vfs/inode.rs).

---

## Phase 9: Robustness & Compliance
Goal: finalize boundary checks, feature gating, and consistency rules.

### Module 9.1: Feature Gating / RO Fallback
- Methods:
  - `Ext2::check_feature_sets`.
- Linux refs:
  - `/root/linux/fs/ext2/super.c` (feature compat/incompat logic).
- Asterinas adjustments:
  - enforce limited supported feature bits per project constraints.
- Spec: `phase-09-feature-gating.spec`.
- Status: ❌ not implemented (feature compat/incompat validation not enforced at mount time).

### Module 9.2: Reserved Block Policy
- Methods:
  - `Ext2::has_free_blocks()` (reserved block gate).
- Linux refs:
  - `/root/linux/fs/ext2/balloc.c:1158` (`ext2_has_free_blocks`).
- Asterinas adjustments:
  - integrate with Asterinas credential APIs (resuid/resgid + CAP_SYS_RESOURCE).
- Spec: `phase-09-reserved-blocks.spec`.
- Status: ❌ not implemented (reserved_blocks_count is parsed but no `has_free_blocks` gate exists).

### Module 9.2: Boundary Checks & Error Mapping
- Methods:
  - `Ext2::check_block_range`, `Ext2::check_ino_range`.
- Linux refs:
  - `/root/linux/fs/ext2/inode.c` and `balloc.c` boundary checks.
- Asterinas adjustments:
  - return `Errno::EIO`/`EINVAL` instead of asserts.
- Spec: `phase-09-boundary-checks.spec`.
- Status: ❌ not implemented (no dedicated `check_block_range`/`check_ino_range` methods).

### Module 9.3: Consistency & Sync
- Methods:
  - `Ext2::sync_metadata`, `Inode::sync_metadata`.
- Linux refs:
  - `/root/linux/fs/ext2/super.c` (superblock writeback).
- Asterinas adjustments:
  - use PageCache and `BlockDevice::sync`.
- Spec: `phase-09-sync.spec`.
- Status: ✅ implemented (`Ext2::sync_metadata` in fs.rs:637, `Inode::sync_all` in inode.rs:1047, `BlockGroup::sync_bitmaps` in block_group.rs:459; commit efcf1018).

---

## Phase 10: Extended Attributes & ioctl
Goal: support extended attributes and file attribute ioctls.

### Module 10.1: Extended Attributes Core
- New/extend structs:
  - `XattrEntry` (on-disk xattr entry format).
  - `XattrBlock` (xattr block reader/writer).
- Methods:
  - `Inode::getxattr(name)`.
  - `Inode::setxattr(name, value, flags)`.
  - `Inode::listxattr()`.
  - `Inode::removexattr(name)`.
- Linux refs:
  - `/root/linux/fs/ext2/xattr.c:200` (`ext2_xattr_get`).
  - `/root/linux/fs/ext2/xattr.c:400` (`ext2_xattr_set`).
- Asterinas adjustments:
  - use PageCache for xattr block I/O.
  - namespace handlers as trait objects or enum dispatch.
- Spec: `phase-10-xattr-core.spec`.
- Status: ❌ not implemented (VfsInode methods return EOPNOTSUPP).

### Module 10.2: Xattr Namespace Handlers
- Methods:
  - `UserXattrHandler::get/set`.
  - `TrustedXattrHandler::get/set`.
  - `SecurityXattrHandler::get/set`.
- Linux refs:
  - `/root/linux/fs/ext2/xattr_user.c`.
  - `/root/linux/fs/ext2/xattr_trusted.c`.
  - `/root/linux/fs/ext2/xattr_security.c`.
- Asterinas adjustments:
  - permission checks via Asterinas credential APIs.
- Spec: `phase-10-xattr-handlers.spec`.
- Status: ❌ not implemented.

### Module 10.3: ioctl Operations
- Methods:
  - `Inode::ioctl_getflags()`.
  - `Inode::ioctl_setflags(flags)`.
  - `Inode::ioctl_getversion()`.
  - `Inode::ioctl_setversion(version)`.
- Linux refs:
  - `/root/linux/fs/ext2/ioctl.c:20` (`ext2_ioctl`).
- Asterinas adjustments:
  - integrate with VFS ioctl dispatch.
- Spec: `phase-10-ioctl.spec`.
- Status: ❌ not implemented.

---

## Phase 11: Orphan Inode & Crash Recovery
Goal: handle unlinked-but-open files and ensure crash consistency.

### Module 11.1: Orphan List Management
- New/extend structs:
  - `OrphanList` (in-memory orphan inode tracking).
- Methods:
  - `Ext2::add_orphan(inode)`.
  - `Ext2::remove_orphan(inode)`.
- Linux refs:
  - `/root/linux/fs/ext2/inode.c:70` (`ext2_add_orphan`).
  - `/root/linux/fs/ext2/inode.c:100` (`ext2_orphan_del`).
- Asterinas adjustments:
  - use linked list or BTreeSet for orphan tracking.
  - update `s_last_orphan` in superblock.
- Spec: `phase-11-orphan-list.spec`.
- Status: ❌ not implemented (no orphan list tracking; `last_orphan` field is parsed from superblock but not used).

### Module 11.2: Orphan Cleanup on Mount
- Methods:
  - `Ext2::cleanup_orphans()`.
- Linux refs:
  - `/root/linux/fs/ext2/super.c:200` (orphan cleanup in `ext2_fill_super`).
- Asterinas adjustments:
  - iterate orphan chain via `i_dtime` linkage.
  - truncate and free each orphan inode.
- Spec: `phase-11-orphan-cleanup.spec`.
- Status: ❌ not implemented.

### Module 11.3: Evict Inode Integration
- Methods:
  - `Inode::evict()` (called when inode refcount drops to zero).
- Linux refs:
  - `/root/linux/fs/ext2/inode.c:130` (`ext2_evict_inode`).
- Asterinas adjustments:
  - if nlink=0: truncate data, free inode, remove from orphan list.
  - if nlink>0: just sync metadata.
- Spec: `phase-11-evict-inode.spec`.
- Status: ✅ partially implemented (`prepare_for_evict` at inode.rs:1073; `evict_inode` at block_group.rs:276 handles nlink=0 truncation and inode freeing during `sync_all_inodes`; but not wired into orphan list).

---

## Phase Gate Checklist (repeat for every phase)
- Spec(s) exist under `kernel/src/fs/ext2/spec/` with pre/post and lock protocol.
- Implementation uses Asterinas primitives and OOP methods only.
- Linux-logic-verify passes for state transitions, boundary checks, and algorithms.
