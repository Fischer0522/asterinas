[PROMPT]
Phase 4.3: rewire resize/fallocate to split locks.

Covered ops:

- resize
- fallocate

Provide modifications to `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_setsize            → fs/ext2/inode.c:1275
block_truncate_page     → fs/buffer.c:2654
ext2_inode_is_fast_symlink → fs/ext2/inode.c:48

[RELY]
```rust
use super::prelude::*;
use super::fs::Ext2;
```

```rust
pub(super) struct InodeInner { /* from spec 11 */ }
```

[GUARANTEE]

```rust
impl Inode {
    pub(super) fn resize(&self, new_size: usize) -> Result<()>;
    pub(super) fn fallocate(&self, mode: FallocMode, offset: usize, len: usize) -> Result<()>;
}
```

[SPECIFICATION]

## resize

Lock:
- Use `meta.write()` for the inode for the duration.
- Use `mapping.write()` only around mapping mutations (expand/shrink/truncate).
- PageCache operations (`fill_zeros`, `resize`) must not hold `mapping.write()`.

Behavior:
- Preserve fast-symlink rules: existing fast symlink with non-zero size cannot
  be resized.
- Preserve flags gate: APPEND_ONLY/IMMUTABLE → EPERM.
- Shrink:
  - If `new_size % block_size != 0`, zero tail bytes in the last block (PageCache).
  - Resize PageCache to `new_size`.
  - Truncate blocks beyond `new_size` (mapping).
  - Update size/timestamps and persist.
- Grow:
  - Resize PageCache.
  - Expand mapping/size as needed (mapping allocation may be deferred to writes
    if current semantics do not allocate on resize; preserve existing behavior).
  - Persist.

## fallocate

Behavior:
- Preserve current Asterinas compat behavior:
  - PunchHoleKeepSize → zero range via PageCache.
  - Allocate → if `new_size > file_size`, call `resize(new_size)`.

Lock:
- Must respect the same PageCache/mapping constraints as resize/write.

[TEST]

## resize
- Shrink unaligned size zeros tail bytes.
- Grow then write/read works.
