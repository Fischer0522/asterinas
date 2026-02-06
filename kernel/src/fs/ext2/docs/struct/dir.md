# Phase 1 - Core Struct Design (`dir.rs`)

Target implementation file: `kernel/src/fs/ext2/dir.rs`

## 1) Structure Definition

```rust
/// Parsed ext2 directory entry used by lookup/readdir logic.
///
/// # Linux Reference
/// - On-disk layout: `struct ext2_dir_entry_2` (`fs/ext2/ext2.h:592-598`)
/// - Validation logic: `ext2_check_folio` (`fs/ext2/dir.c:99-179`)
///
/// # Concurrency
/// - Read path: shared inode/read lock + page cache read
/// - Write path (add/delete/rename target updates): serialized by directory inode write lock
/// - Cross-inode order for operations touching multiple dirs/inodes:
///   `dir inode(s) ascending ino -> target inode ascending ino -> block group/bitmap`
///   Linux intent refs: `fs/ext2/namei.c:318-404`, `fs/ext2/inode.c:1196-1200`
///
/// # Caching
/// - Entry bytes are cached in directory data pages (`PageCache`).
/// - Authoritative source is on-disk directory blocks.
#[derive(Clone, Debug)]
pub struct DirEntry {
    /// Referenced inode number.
    /// Linux: `ext2_dir_entry_2.inode`
    /// Linux: `fs/ext2/ext2.h:593`
    pub inode: u32,

    /// Record length in bytes, 4-byte aligned.
    /// Linux: `ext2_dir_entry_2.rec_len`
    /// Linux: `fs/ext2/ext2.h:594`, `fs/ext2/ext2.h:605-608`
    pub rec_len: u16,

    /// Name length (<= 255).
    /// Linux: `ext2_dir_entry_2.name_len`
    /// Linux: `fs/ext2/ext2.h:595`
    pub name_len: u8,

    /// Optional file type (when FILETYPE incompat feature is enabled).
    /// Linux: `ext2_dir_entry_2.file_type`, feature check in `ext2_set_de_type`.
    /// Linux: `fs/ext2/ext2.h:596`, `fs/ext2/dir.c:248-254`
    pub file_type: u8,

    /// Decoded entry name.
    /// Linux: `ext2_dir_entry_2.name[]`
    /// Linux: `fs/ext2/ext2.h:597`
    pub name: CStr256,
}

/// Directory entry iterator over a single block-sized slice.
/// Linux intent peer: iterative scan in `ext2_readdir`/`ext2_find_entry`.
/// Linux: `fs/ext2/dir.c:257-324`, `fs/ext2/dir.c:342-404`
pub struct DirEntryIter<'a> {
    buf: &'a [u8],
    offset: usize,
    limit: usize,
    max_inumber: u32,
}

/// Directory mutation intent classification.
/// Linux peers: `ext2_add_link`, `ext2_delete_entry`, `ext2_set_link`.
/// Linux: `fs/ext2/dir.c:450-610`
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirMutationKind {
    Add,
    Delete,
    SetLink,
}

/// Per-directory dirty state for metadata/data updates.
/// Linux intent peer: `mark_inode_dirty(dir)` in directory paths.
/// Linux: `fs/ext2/dir.c:94`, `fs/ext2/dir.c:469`, `fs/ext2/dir.c:556`, `fs/ext2/dir.c:610`
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirDirtyReason {
    DataBlockChanged,
    LinkCountChanged,
    SizeChanged,
}
```

## 2) Method Signatures (No Implementation Yet)

```rust
impl DirEntry {
    /// Convert on-disk rec_len into host representation.
    /// Linux equivalent: `ext2_rec_len_from_disk`.
    /// Linux: `fs/ext2/dir.c:38`
    pub fn rec_len_from_disk(rec_len: u16) -> u16;

    /// Compute minimal aligned record length from name length.
    /// Linux equivalent: `EXT2_DIR_REC_LEN(name_len)`.
    /// Linux: `fs/ext2/ext2.h:607-608`
    pub fn dir_rec_len(name_len: usize) -> u16;

    /// Validate one entry against ext2 layout and boundary rules.
    /// Linux equivalent: checks in `ext2_check_folio`.
    /// Linux: `fs/ext2/dir.c:117-133`
    pub fn validate(
        rec_len: u16,
        name_len: u8,
        offset: usize,
        limit: usize,
        max_inumber: u32,
        inode: u32,
    ) -> Result<()>;

    /// Parse entry at offset from a raw directory block buffer.
    /// Linux equivalent: parsing in `ext2_check_folio` + scan routines.
    /// Linux: `fs/ext2/dir.c:99-133`, `fs/ext2/dir.c:342-404`
    pub fn parse_at(buf: &[u8], offset: usize, limit: usize, max_inumber: u32) -> Result<Self>;
}

impl<'a> DirEntryIter<'a> {
    /// Create an iterator for a directory block segment.
    pub fn new(buf: &'a [u8], limit: usize, max_inumber: u32) -> Result<Self>;

    /// Return next valid entry or end-of-buffer.
    pub fn next_entry(&mut self) -> Result<Option<DirEntry>>;
}

impl InodeInner {
    /// Lookup by name in directory entries.
    /// Linux equivalent: `ext2_find_entry`.
    /// Linux: `fs/ext2/dir.c:342-404`
    pub fn find_entry(&self, name: &str) -> Result<u32>;

    /// Iterate entries from offset for `readdir` behavior.
    /// Linux equivalent: `ext2_readdir`.
    /// Linux: `fs/ext2/dir.c:257-324`
    pub fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize>;

    /// Add a new directory entry and adjust inode metadata.
    /// Linux equivalent: `ext2_add_link`.
    /// Linux: `fs/ext2/dir.c:476-567`
    pub fn add_entry(&self, name: &str, child_ino: u32, file_type: DirEntryFileType) -> Result<()>;

    /// Delete a directory entry and compact free space.
    /// Linux equivalent: `ext2_delete_entry`.
    /// Linux: `fs/ext2/dir.c:571-610`
    pub fn delete_entry(&self, name: &str) -> Result<()>;

    /// Replace directory entry target inode (rename/link update helper).
    /// Linux equivalent: `ext2_set_link`.
    /// Linux: `fs/ext2/dir.c:450-474`
    pub fn set_entry_link(&self, name: &str, new_ino: u32, update_times: bool) -> Result<()>;

    /// Verify directory emptiness for rmdir/rename constraints.
    /// Linux equivalent: `ext2_empty_dir`.
    /// Linux: `fs/ext2/dir.c:659-703`
    pub fn is_empty_dir(&self) -> Result<bool>;

    /// Mark directory inode dirty after metadata/data mutation.
    pub fn mark_dir_dirty(&self, reason: DirDirtyReason);
}
```

## 3) Deadlock-Free Directory Protocol

- **Single-directory mutation** (`create`, `unlink` in one parent): acquire parent directory inode write lock, then block-group/bitmap locks as needed.
- **Two-directory mutation** (`rename` cross-dir): acquire `old_dir` and `new_dir` locks by ascending inode number, then child inode lock (if needed), then allocation locks.
- **No I/O under long-held write lock**: perform minimal critical updates, then release and schedule writeback.
- Linux intent basis: multi-inode rename sequence in `fs/ext2/namei.c:318-404`.

## 4) Corruption Detection & Degradation

Directory entry parser rejects and escalates on:
- `rec_len` too short or unaligned (`fs/ext2/dir.c:121-124`)
- `rec_len < EXT2_DIR_REC_LEN(name_len)` (`fs/ext2/dir.c:125-126`)
- entry spanning block/chunk boundary (`fs/ext2/dir.c:127-128`)
- inode number out of bounds (`fs/ext2/dir.c:129`)

On repeated/critical metadata corruption, propagate to fs-level error handling and transition to readonly-forced mode.

## 5) Design Rationale

### Why keep parser/iterator separate from mutation logic?
- Read-heavy scans (`lookup`, `readdir`) dominate common workload; separating parse/iterate from mutation yields lower lock contention and clearer invariants.

### Why strict boundary checks mirror Linux?
- Ext2 directory corruption can silently cascade; Linux checks are intentionally strict and must be preserved exactly in spirit and sequence.

### Why `PageCache` for directory blocks?
- Replaces `buffer_head`-style paging with Asterinas async I/O integration while preserving logical behavior.
