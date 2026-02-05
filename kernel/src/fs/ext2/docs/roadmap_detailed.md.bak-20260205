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

### Module 0.2: VFS Glue Interfaces
- New/extend structs:
  - `impl FileSystem for Ext2` (from `kernel/src/fs/utils`).
  - `impl Inode for Ext2Inode` (stub trait impls).
- Methods:
  - `FileSystem::sync`, `FileSystem::root_inode`, `FileSystem::sb`.
- Linux refs: none (glue only).
- Asterinas adjustments: follow VFS trait expectations and error model.
- Spec: `phase-00-vfs-glue.spec`.

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
  - use `Frame` + `VmIo` to read, no manual endian swaps (Pod).
- Spec: `phase-01-raw-structures.spec`.

### Module 1.2: Superblock Read/Validate
- New/extend structs:
  - `SuperBlock` (in-memory), `FeatureCompatSet`, `FeatureInCompatSet`, `FeatureRoCompatSet`.
- Methods:
  - `RawSuperBlock::read_from(frame, offset)`.
  - `impl TryFrom<RawSuperBlock> for SuperBlock`.
  - `Ext2::read_super(block_device)`.
  - `Ext2::validate_super(&SuperBlock)`.
- Linux refs:
  - `/root/linux/fs/ext2/super.c:800+` (`ext2_fill_super`).
  - `/root/linux/fs/ext2/super.c:645` (`ext2_setup_super`).
- Asterinas adjustments:
  - block size fixed to 4096 (per project constraint), restrict features accordingly.
  - error handling via `Errno` mapping.
- Spec: `phase-01-superblock.spec`.

### Module 1.3: Group Descriptor Table Load
- New/extend structs:
  - `GroupDescTable` (cached descriptors + segment bounds).
- Methods:
  - `GroupDescTable::load(block_device, super_block)`.
  - `Ext2::group_desc(group)`.
- Linux refs:
  - `/root/linux/fs/ext2/super.c:695` (`ext2_check_descriptors`).
- Asterinas adjustments:
  - use `USegment` + `PageCache` for descriptor table pages.
- Spec: `phase-01-group-desc-table.spec`.

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

### Module 2.2: Block Bitmap Read Path
- New/extend structs:
  - `BlockBitmap` (view over bitmap page(s)).
- Methods:
  - `BlockBitmap::load(group)`.
  - `BlockBitmap::is_free(block)`.
- Linux refs:
  - `/root/linux/fs/ext2/balloc.c:128` (`read_block_bitmap`).
- Asterinas adjustments:
  - use `PageCache` pages instead of `buffer_head`.
- Spec: `phase-02-block-bitmap-read.spec`.

### Module 2.3: Inode Bitmap Read Path
- New/extend structs:
  - `InodeBitmap` (view over bitmap page(s)).
- Methods:
  - `InodeBitmap::load(group)`.
  - `InodeBitmap::is_free(ino)`.
- Linux refs:
  - `/root/linux/fs/ext2/ialloc.c:79` (bitmap access patterns).
- Asterinas adjustments:
  - same PageCache usage as block bitmap.
- Spec: `phase-02-inode-bitmap-read.spec`.

---

## Phase 3: Inode Read Path & Block Mapping
Goal: read inode from disk, map logical to physical blocks, enable read-only file access.

### Module 3.1: Inode Table I/O
- New/extend structs:
  - `InodeDesc` (in-memory), `InodeTable` helper.
- Methods:
  - `Ext2::read_inode_desc(ino)`.
  - `Ext2::inode_table_block(group, index)`.
- Linux refs:
  - `/root/linux/fs/ext2/inode.c:1314` (`ext2_get_inode`).
- Asterinas adjustments:
  - compute offsets using `block_size` and `inode_size`.
- Spec: `phase-03-inode-table-io.spec`.

### Module 3.2: Inode Cache & Instantiation
- New/extend structs:
  - `Inode` (Asterinas inode wrapper), `InodeInner`, `InodeImpl`.
- Methods:
  - `Ext2::read_inode(ino)` (cache lookup + load).
  - `Ext2::cache_inode(ino, inode)`.
- Linux refs:
  - `/root/linux/fs/ext2/inode.c:1387` (`ext2_iget`).
- Asterinas adjustments:
  - cache via `BTreeMap<u32, Weak<Inode>>`.
- Spec: `phase-03-inode-cache.spec`.

### Module 3.3: Logical-to-Physical Block Mapping
- New/extend structs:
  - `BlockPtrs` helper (direct/indirect indices).
- Methods:
  - `Ext2Inode::block_to_path(logical_block)`.
  - `Ext2Inode::get_block(logical_block)`.
- Linux refs:
  - `/root/linux/fs/ext2/inode.c:163` (`ext2_block_to_path`).
  - `/root/linux/fs/ext2/inode.c:783` (`ext2_get_block`).
- Asterinas adjustments:
  - indirect blocks read via `PageCache` and `BlockDevice`.
- Spec: `phase-03-block-mapping.spec`.

---

## Phase 4: Directory Read Path
Goal: lookup and readdir in read-only mode.

### Module 4.1: Directory Entry Parsing
- New/extend structs:
  - `DirEntry` (view helper), `DirEntryIter`.
- Methods:
  - `DirEntry::validate(rec_len, name_len)`.
  - `Ext2Inode::dir_iter()`.
- Linux refs:
  - `/root/linux/fs/ext2/dir.c:342` (`ext2_find_entry`).
- Asterinas adjustments:
  - iterate via PageCache, enforce 4-byte alignment.
- Spec: `phase-04-dir-entry-parse.spec`.

### Module 4.2: Lookup / Readdir
- Methods:
  - `Ext2Inode::find_entry(name)`.
  - `Ext2Inode::readdir_at(pos, visitor)`.
- Linux refs:
  - `/root/linux/fs/ext2/dir.c:342` (`ext2_find_entry`).
  - `/root/linux/fs/ext2/namei.c` (lookup semantics).
- Asterinas adjustments:
  - return `Errno::ENOENT` on miss, integrate `DirentVisitor`.
- Spec: `phase-04-dir-lookup-readdir.spec`.

---

## Phase 5: Allocation (Blocks & Inodes) + Write Enable
Goal: enable writable mount and allocation semantics.

### Module 5.1: Block Allocation Core
- New/extend structs:
  - `BlockAllocator` (per-fs or per-group).
- Methods:
  - `Ext2::alloc_blocks(goal, count)`.
  - `Ext2::free_blocks(start, count)`.
- Linux refs:
  - `/root/linux/fs/ext2/balloc.c:1208` (`ext2_new_blocks`).
  - `/root/linux/fs/ext2/balloc.c:482` (`ext2_free_blocks`).
- Asterinas adjustments:
  - update bitmap pages via PageCache; sync via `BioWaiter`.
- Spec: `phase-05-block-alloc.spec`.

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
  - Orlov policy implemented with `BTreeMap` statistics, no global hash.
- Spec: `phase-05-inode-alloc.spec`.

### Module 5.3: Writable Superblock / Group Counters
- Methods:
  - `SuperBlock::inc/dec_free_blocks`, `inc/dec_free_inodes`.
  - `BlockGroup::inc/dec_free_*`.
- Linux refs:
  - `/root/linux/fs/ext2/balloc.c` and `/root/linux/fs/ext2/ialloc.c` counter updates.
- Asterinas adjustments:
  - atomicity via `RwMutex` and `Dirty`.
- Spec: `phase-05-counter-accounting.spec`.

---

## Phase 6: Directory Mutation & File Creation
Goal: create/remove/rename entries and update link counts.

### Module 6.1: Add/Delete Dir Entries
- Methods:
  - `Ext2Inode::add_entry(name, ino, file_type)`.
  - `Ext2Inode::delete_entry(name)`.
- Linux refs:
  - `/root/linux/fs/ext2/dir.c:476` (`ext2_add_link`).
  - `/root/linux/fs/ext2/dir.c:571` (`ext2_delete_entry`).
- Asterinas adjustments:
  - update PageCache-backed dir blocks; maintain alignment.
- Spec: `phase-06-dir-mutation.spec`.

### Module 6.2: make_empty / mkdir / rmdir
- Methods:
  - `Ext2Inode::make_empty(parent_ino)`.
  - `Ext2Inode::rmdir(name)`.
- Linux refs:
  - `/root/linux/fs/ext2/dir.c:617` (`ext2_make_empty`).
  - `/root/linux/fs/ext2/namei.c:228` (`ext2_mkdir`).
- Asterinas adjustments:
  - update link counts and timestamps via inode methods.
- Spec: `phase-06-dir-create-remove.spec`.

### Module 6.3: Namei Ops (create/link/unlink/rename)
- Methods:
  - `Ext2Inode::create`, `link`, `unlink`, `rename`.
- Linux refs:
  - `/root/linux/fs/ext2/namei.c:102` (`ext2_create`).
  - `/root/linux/fs/ext2/namei.c:273` (`ext2_unlink`).
  - `/root/linux/fs/ext2/namei.c:318` (`ext2_rename`).
- Asterinas adjustments:
  - use Asterinas inode/type enums; no raw pointers.
- Spec: `phase-06-namei-ops.spec`.

---

## Phase 7: File Write, Resize, Truncate (split modules)
Goal: full data write path and size changes with correct block tree updates.

### Module 7.1: File Write Path
- Methods:
  - `Ext2Inode::write_at(offset, data)`.
  - `Ext2Inode::write_page(idx)` (PageCache backend hook).
- Linux refs:
  - `/root/linux/fs/ext2/file.c` (write semantics).
  - `/root/linux/fs/ext2/inode.c:783` (`ext2_get_block`).
- Asterinas adjustments:
  - use PageCache and async `BioWaiter` for flush.
- Spec: `phase-07-file-write.spec`.

### Module 7.2: Resize (grow)
- Methods:
  - `Ext2Inode::resize(new_size)` (grow path).
  - `Ext2Inode::alloc_blocks_for_range`.
- Linux refs:
  - `/root/linux/fs/ext2/inode.c` (block allocation during write).
- Asterinas adjustments:
  - update `Dirty<InodeDesc>` and PageCache size.
- Spec: `phase-07-file-resize-grow.spec`.

### Module 7.3: Truncate (shrink + free)
- Methods:
  - `Ext2Inode::truncate(new_size)`.
  - `Ext2Inode::free_branches` (indirect tree free).
- Linux refs:
  - `/root/linux/fs/ext2/inode.c:1262` (`ext2_truncate_blocks`).
  - `/root/linux/fs/ext2/inode.c:1136` (`ext2_free_branches`).
- Asterinas adjustments:
  - use BlockAllocator + PageCache eviction.
- Spec: `phase-07-file-truncate.spec`.

---

## Phase 8: Metadata & Special Files
Goal: symlink and special inode types with correct metadata behavior.

### Module 8.1: Symlinks
- Methods:
  - `Ext2Inode::read_link`, `write_link` (fast/slow).
- Linux refs:
  - `/root/linux/fs/ext2/inode.c` (symlink paths).
- Asterinas adjustments:
  - fast symlink stored in inode block pointers; slow via data blocks.
- Spec: `phase-08-symlink.spec`.

### Module 8.2: Special Files
- Methods:
  - `Ext2Inode::set_device_id`.
- Linux refs:
  - `/root/linux/fs/ext2/inode.c` (special file encoding).
- Asterinas adjustments:
  - map device IDs to `InodeType::CharDevice/BlockDevice`.
- Spec: `phase-08-special-files.spec`.

### Module 8.3: Metadata Updates
- Methods:
  - `set_atime/mtime/ctime`, `set_mode`, `set_owner`.
- Linux refs:
  - `/root/linux/fs/ext2/inode.c` (timestamp and chmod semantics).
- Asterinas adjustments:
  - use `UnixTime` and Asterinas credential APIs.
- Spec: `phase-08-metadata.spec`.

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

### Module 9.2: Boundary Checks & Error Mapping
- Methods:
  - `Ext2::check_block_range`, `Ext2::check_ino_range`.
- Linux refs:
  - `/root/linux/fs/ext2/inode.c` and `balloc.c` boundary checks.
- Asterinas adjustments:
  - return `Errno::EIO`/`EINVAL` instead of asserts.
- Spec: `phase-09-boundary-checks.spec`.

### Module 9.3: Consistency & Sync
- Methods:
  - `Ext2::sync_metadata`, `Ext2Inode::sync_metadata`.
- Linux refs:
  - `/root/linux/fs/ext2/super.c` (superblock writeback).
- Asterinas adjustments:
  - use PageCache and `BlockDevice::sync`.
- Spec: `phase-09-sync.spec`.

---

## Phase 10: Extended Attributes & ioctl
Goal: support extended attributes and file attribute ioctls.

### Module 10.1: Extended Attributes Core
- New/extend structs:
  - `XattrEntry` (on-disk xattr entry format).
  - `XattrBlock` (xattr block reader/writer).
- Methods:
  - `Ext2Inode::getxattr(name)`.
  - `Ext2Inode::setxattr(name, value, flags)`.
  - `Ext2Inode::listxattr()`.
  - `Ext2Inode::removexattr(name)`.
- Linux refs:
  - `/root/linux/fs/ext2/xattr.c:200` (`ext2_xattr_get`).
  - `/root/linux/fs/ext2/xattr.c:400` (`ext2_xattr_set`).
- Asterinas adjustments:
  - use PageCache for xattr block I/O.
  - namespace handlers as trait objects or enum dispatch.
- Spec: `phase-10-xattr-core.spec`.

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

### Module 10.3: ioctl Operations
- Methods:
  - `Ext2Inode::ioctl_getflags()`.
  - `Ext2Inode::ioctl_setflags(flags)`.
  - `Ext2Inode::ioctl_getversion()`.
  - `Ext2Inode::ioctl_setversion(version)`.
- Linux refs:
  - `/root/linux/fs/ext2/ioctl.c:20` (`ext2_ioctl`).
- Asterinas adjustments:
  - integrate with VFS ioctl dispatch.
- Spec: `phase-10-ioctl.spec`.

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

### Module 11.2: Orphan Cleanup on Mount
- Methods:
  - `Ext2::cleanup_orphans()`.
- Linux refs:
  - `/root/linux/fs/ext2/super.c:200` (orphan cleanup in `ext2_fill_super`).
- Asterinas adjustments:
  - iterate orphan chain via `i_dtime` linkage.
  - truncate and free each orphan inode.
- Spec: `phase-11-orphan-cleanup.spec`.

### Module 11.3: Evict Inode Integration
- Methods:
  - `Ext2Inode::evict()` (called when inode refcount drops to zero).
- Linux refs:
  - `/root/linux/fs/ext2/inode.c:130` (`ext2_evict_inode`).
- Asterinas adjustments:
  - if nlink=0: truncate data, free inode, remove from orphan list.
  - if nlink>0: just sync metadata.
- Spec: `phase-11-evict-inode.spec`.

---

## Phase Gate Checklist (repeat for every phase)
- Spec(s) exist under `kernel/src/fs/ext2/spec/` with pre/post and lock protocol.
- Implementation uses Asterinas primitives and OOP methods only.
- Linux-logic-verify passes for state transitions, boundary checks, and algorithms.
