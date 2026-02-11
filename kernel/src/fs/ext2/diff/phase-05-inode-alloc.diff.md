# Phase 05 Diff Report: Inode Allocation

## Scope

- Rust: `kernel/src/fs/ext2/fs.rs`
- Linux references:
  - `/root/linux/fs/ext2/ialloc.c:419` (`ext2_new_inode`)
  - `/root/linux/fs/ext2/ialloc.c:105` (`ext2_free_inode`)
  - `/root/linux/fs/ext2/ialloc.c:199` (`find_group_dir`)
  - `/root/linux/fs/ext2/ialloc.c:251` (`find_group_orlov`)

---

## 1) No Orlov / find_group_dir Policy

- **Linux**: Directory inode allocation uses two policies:
  - `find_group_dir()`: Forward search for group with above-average free inodes and most free blocks.
  - `find_group_orlov()`: Sophisticated policy that spreads top-level directories, considers debt counters, and uses randomization.
  - Non-directory inodes use `find_group_other()`: starts from parent group, uses quadratic probing.
  - Reference: `/root/linux/fs/ext2/ialloc.c:199-418`
- **Asterinas**: Simple cyclic scan from parent group. No distinction between directory and file allocation policy.
  - Rust: `fs.rs:543-544`
- **Impact**: No directory spreading. All inodes tend to cluster near the parent group.

## 2) free_inode: Reads Inode from Disk to Determine is_dir

- **Linux**: `ext2_free_inode()` receives the in-memory `struct inode *` which already has `i_mode` loaded. It checks `S_ISDIR(inode->i_mode)` directly — no disk I/O needed.
  - Reference: `/root/linux/fs/ext2/ialloc.c:127`
- **Asterinas**: `free_inode()` receives only `ino: u32`. It must call `read_inode_desc(ino)` to read the inode from disk just to check `type_().is_directory()`.
  - Rust: `fs.rs:668`
- **Impact**: Extra disk I/O on every inode free. With an inode cache, this would be a cache lookup instead.

## 3) free_inode: Ordering — clear_inode Before Bitmap Clear

- **Linux**: `ext2_free_inode()` is called from `ext2_evict_inode()` which has already called `clear_inode()`. The comment explicitly states: "we must call clear_inode() _before_ we mark the inode not in use in the inode bitmaps. Otherwise a newly created file might use the same inode number."
  - Reference: `/root/linux/fs/ext2/ialloc.c:89-103`
- **Asterinas**: `free_inode()` clears the bitmap bit directly. There is no `clear_inode` equivalent and no ordering guarantee.
  - Rust: `fs.rs:689-691`
- **Impact**: Race condition window where a newly allocated inode could reuse a number whose old data hasn't been fully cleared.

## 4) create_inode: Reduced Field Initialization

- **Linux**: `ext2_new_inode()` initializes uid/gid (from current credentials with SGID inheritance), timestamps (current time), generation number (random), flags, ACL, security labels.
  - Reference: `/root/linux/fs/ext2/ialloc.c:539+`
- **Asterinas**: `create_inode()` initializes mode, links_count, and zeros everything else. No uid/gid, no timestamps, no generation.
  - Rust: `fs.rs:606-632`
- **Impact**: Created inodes have uid=0, gid=0, all timestamps=0, generation=0.

## 5) No Quota Integration

- **Linux**: `ext2_new_inode()` calls `dquot_initialize()` and `dquot_alloc_inode()`. `ext2_free_inode()` calls `dquot_free_inode()` and `dquot_drop()`.
- **Asterinas**: No quota support (explicitly out of scope per roadmap).

---

## Spec-vs-Linux Supplement (from spec review)

### S1) phase-05-inode-alloc.spec: Cyclic Scan from Parent Group

- **Spec**: `alloc_inode` computes `parent_group = (parent_ino - 1) / inodes_per_group`, then scans cyclically from `parent_group`.
- **Linux**: Non-directory inodes use `find_group_other()` which starts from parent group with **quadratic probing** (`group += stride; stride = stride * 2 + 1`), not simple cyclic scan.
  - Reference: `/root/linux/fs/ext2/ialloc.c:260-310`
- **Diff**: Asterinas uses linear cyclic scan; Linux uses quadratic probing. Quadratic probing spreads allocations more evenly across groups.

### S2) phase-05-inode-alloc.spec: Already-Free Inode Handling

- **Spec**: If bitmap bit is already clear during `free_inode`, logs metadata inconsistency and skips counter updates.
- **Linux**: `ext2_free_inode()` calls `ext2_clear_bit_atomic()` and checks the return value. If the bit was already clear, calls `ext2_error()` and continues.
  - Reference: `/root/linux/fs/ext2/ialloc.c:135-140`
- **Status**: Aligned in behavior. Both log and continue without counter mutation.
