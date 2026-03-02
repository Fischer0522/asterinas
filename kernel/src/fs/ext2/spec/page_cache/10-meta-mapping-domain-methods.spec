[PROMPT]
Phase 2 (encapsulation): introduce `InodeMeta` and `InodeMapping` domain types
that own split descriptors with dirty tracking, and move inode algorithms to the
appropriate domain.

Provide modifications to `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_get_block           → fs/ext2/inode.c:783
ext2_get_blocks          → fs/ext2/inode.c:624
ext2_blks_to_allocate    → fs/ext2/inode.c:361
ext2_alloc_branch        → fs/ext2/inode.c:479
ext2_splice_branch       → fs/ext2/inode.c:561
__ext2_truncate_blocks   → fs/ext2/inode.c:1172
ext2_setsize             → fs/ext2/inode.c:1275

[RELY]
```rust
use super::prelude::*;
use super::fs::Ext2;
use super::utils::Dirty;
```

```rust
pub(super) struct InodeMetaDesc { /* from spec 9 */ }
pub(super) struct InodeMappingDesc { /* from spec 9 */ }
```

[GUARANTEE]

```rust
/// Inode metadata domain. Owns meta descriptor dirty tracking.
#[derive(Debug)]
pub(super) struct InodeMeta {
    pub(super) desc: Dirty<InodeMetaDesc>,
    pub(super) is_freed: bool,
}

impl InodeMeta {
    pub(super) fn file_size(&self) -> usize;
    pub(super) fn set_file_size(&mut self, new_size: usize);

    pub(super) fn atime(&self) -> Duration;
    pub(super) fn set_atime(&mut self, t: Duration);
    pub(super) fn mtime(&self) -> Duration;
    pub(super) fn set_mtime(&mut self, t: Duration);
    pub(super) fn ctime(&self) -> Duration;
    pub(super) fn set_ctime(&mut self, t: Duration);

    pub(super) fn links_count(&self) -> u16;
    pub(super) fn set_links_count(&mut self, nlinks: u16);

    pub(super) fn flags(&self) -> FileFlags;
    pub(super) fn set_flags(&mut self, flags: FileFlags);
}
```

```rust
/// Inode mapping domain. Owns i_blocks and i_block[15] dirty tracking.
#[derive(Debug)]
pub(super) struct InodeMapping {
    pub(super) desc: Dirty<InodeMappingDesc>,
}

impl InodeMapping {
    /// Resolves a logical block to physical block (read-only).
    pub(super) fn get_block(&self, fs: &Ext2, iblock: u32) -> Result<Option<Bid>>;

    /// Resolves a logical block to physical, optionally allocating missing branch.
    pub(super) fn get_or_alloc_block(
        &mut self,
        fs: &Ext2,
        iblock: u32,
        create: bool,
    ) -> Result<Option<Bid>>;

    /// Truncates all blocks beyond `new_size`.
    pub(super) fn truncate_blocks(&mut self, fs: &Ext2, new_size: usize) -> Result<()>;

    pub(super) fn blocks_512(&self) -> u32;
    pub(super) fn set_blocks_512(&mut self, blocks: u32);
}
```

[SPECIFICATION]

## Separation of responsibilities

- `InodeMeta` must not contain `i_blocks` or `i_block[15]`.
- `InodeMapping` must be the single source of truth for mapping fields.

## Mutation rules

- Mapping mutation (`get_or_alloc_block`, `truncate_blocks`) MUST set mapping
  dirty state (via `Dirty<T>`).
- Meta mutation (size/times/flags/links) MUST set meta dirty state.

## Linux compatibility

- Block mapping and allocation logic in `InodeMapping` must mirror Linux ext2:
  - `ext2_get_blocks` create path semantics.
  - `ext2_alloc_branch` + `ext2_splice_branch` crash-safety sequencing.
  - `__ext2_truncate_blocks` truncation semantics.

[DIFF]

Linux: mapping logic spans `struct inode` and ext2-specific inode info.
  → Asterinas: move mapping logic into `InodeMapping` to keep the split-lock
    refactor localized and auditable.

[TEST]

## InodeMapping::{get_block,get_or_alloc_block}
- Read unmapped blocks → Ok(None).
- Allocate blocks across indirect levels → mapping pointers updated and readable.
- Allocation ENOSPC → Err(ENOSPC) and mapping unchanged.

## InodeMapping::truncate_blocks
- Truncate to smaller size → blocks beyond end are freed and pointers cleared.
- Truncate to same size → no-op.
