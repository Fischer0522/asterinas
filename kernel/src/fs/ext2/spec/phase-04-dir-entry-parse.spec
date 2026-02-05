[PROMPT]
Provide additions to `kernel/src/fs/ext2/dir.rs` and `kernel/src/fs/ext2/mod.rs`. Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
struct ext2_dir_entry_2     → fs/ext2/ext2.h:592
EXT2_DIR_REC_LEN            → fs/ext2/ext2.h:607
ext2_rec_len_from_disk      → fs/ext2/dir.c:38
ext2_check_folio            → fs/ext2/dir.c:99

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::inode::RawDirEntry;
```

```rust
use crate::fs::utils::{CStr256, NAME_MAX};
```

```rust
/// A parsed directory entry.
#[derive(Clone, Debug)]
pub(super) struct DirEntry {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
    pub name: CStr256,
}
```

```rust
/// Directory entry iterator over a single block buffer.
pub(super) struct DirEntryIter<'a> {
    pub buf: &'a [u8],
    pub offset: usize,
    pub limit: usize,
    pub max_inumber: u32,
}
```

[GUARANTEE]
impl DirEntry {
    pub(super) fn rec_len_from_disk(rec_len: u16) -> u16;
    pub(super) fn dir_rec_len(name_len: usize) -> u16;
    pub(super) fn validate(
        rec_len: u16,
        name_len: u8,
        offset: usize,
        limit: usize,
        max_inumber: u32,
        inode: u32,
    ) -> Result<()>;
    pub(super) fn parse_at(
        buf: &[u8],
        offset: usize,
        limit: usize,
        max_inumber: u32,
    ) -> Result<DirEntry>;
}

impl<'a> DirEntryIter<'a> {
    pub(super) fn new(buf: &'a [u8], limit: usize, max_inumber: u32) -> Result<Self>;
    pub(super) fn next_entry(&mut self) -> Result<Option<DirEntry>>;
}

[SPECIFICATION]
Pre (DirEntry::rec_len_from_disk):
- `rec_len` is a little-endian on-disk value.

Post (DirEntry::rec_len_from_disk):
- Returns `rec_len` as host-endian `u16` (PAGE_SIZE < 65536, so no special 64K handling).

Pre (DirEntry::dir_rec_len):
- `name_len <= NAME_MAX`.

Post (DirEntry::dir_rec_len):
- Returns `((name_len + 8 + 3) & !3)` as `u16` (Linux `EXT2_DIR_REC_LEN`).

Pre (DirEntry::validate):
- `limit` is the valid byte length of the directory block (<= BLOCK_SIZE).
- `offset < limit`.

Post (DirEntry::validate: success):
- Enforces Linux `ext2_check_folio` constraints for a single entry:
  - `rec_len >= EXT2_DIR_REC_LEN(1)`.
  - `rec_len` is 4-byte aligned (`rec_len & 3 == 0`).
  - `rec_len >= EXT2_DIR_REC_LEN(name_len)`.
  - `offset + rec_len <= limit` (entry does not span block boundary).
  - `name_len <= NAME_MAX`.
  - `inode <= max_inumber`.

Post (DirEntry::validate: failure):
- Returns `Err(Errno::EIO)` when any validation fails.

Pre (DirEntry::parse_at):
- `buf` length is at least `limit`.
- `offset + size_of::<RawDirEntry>() <= limit`.

Post (DirEntry::parse_at: success):
- Reads `RawDirEntry` at `offset`.
- Converts `rec_len` using `rec_len_from_disk`.
- Validates with `validate` using `inode` from the entry.
- Copies `name_len` bytes from `buf` after the header into `CStr256` and appends `\0`.
- Returns `DirEntry { inode, rec_len, name_len, file_type, name }`.

Post (DirEntry::parse_at: failure):
- Returns `Err(Errno::EIO)` for invalid layout or out-of-bounds.

Pre (DirEntryIter::new):
- `limit <= buf.len()`.
- `limit > 0`.

Post (DirEntryIter::new):
- Returns an iterator with `offset = 0` and the provided `limit`.

Post (DirEntryIter::next_entry: success):
- If `offset == limit`, returns `Ok(None)`.
- Otherwise parses the entry at `offset` via `DirEntry::parse_at`.
- Advances `offset += rec_len`.
- Returns `Ok(Some(entry))`.

Post (DirEntryIter::next_entry: failure):
- Returns `Err(Errno::EIO)` if an entry is invalid or `offset` would exceed `limit`.

Invariant:
- Entry validation and record length rules match Linux `ext2_check_folio` semantics.
