[PROMPT]
Phase 3.2 (cleanup): after the split-lock refactor (spec 11), remove the old
upread/upgrade/downgrade lock choreography and monolithic lock helpers that were
introduced as a workaround for PageCacheBackend callback deadlocks.

This phase is expected to be non-compiling until Phase 4 rewires all inode
operations to the new lock layout.

Provide modifications to `kernel/src/fs/ext2/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
Locking concepts         → (i_rwsem, truncate/mapping locks in VFS)
ext2_rename              → fs/ext2/namei.c:318

[RELY]
```rust
use super::prelude::*;
```

```rust
pub(super) struct InodeInner { /* from spec 11 */ }
pub(super) struct InodeMeta { /* from spec 10 */ }
pub(super) struct InodeMapping { /* from spec 10 */ }
```

[GUARANTEE]

## Remove obsolete upread choreography

Delete any helper that:

- Accepts or returns `RwMutexUpgradeableGuard<...>`.
- Calls `.upread()`, `.upgrade()`, or `.downgrade()` as part of inode operations.

Specifically remove the following patterns from `impl Inode`:

- `*_with_upread_guard(...)`
- `*_from_write_guard(...)`

These will be replaced by serialized `meta.write()` mutations in Phase 4.

## Replace monolithic multi-inode lock helpers

Replace helpers that lock `RwMutex<InodeInner>` (monolithic) with helpers that
lock only the required split domains.

Provide the following new helpers (names may vary, but semantics must match):

```rust
/// Acquire `meta.read()` on two inodes in ascending inode-number order.
///
/// # Lock
/// Lock order is by inode number to prevent deadlock.
fn meta_read_lock_two_inodes<'a>(
    a: &'a Inode,
    b: &'a Inode,
) -> (RwMutexReadGuard<'a, InodeMeta>, RwMutexReadGuard<'a, InodeMeta>);

/// Acquire `meta.write()` on two inodes in ascending inode-number order.
///
/// # Lock
/// Lock order is by inode number to prevent deadlock.
fn meta_write_lock_two_inodes<'a>(
    a: &'a Inode,
    b: &'a Inode,
) -> (RwMutexWriteGuard<'a, InodeMeta>, RwMutexWriteGuard<'a, InodeMeta>);

/// Acquire `meta.write()` on multiple inodes in ascending inode-number order.
/// Returns guards in the same order as the input slice.
fn meta_write_lock_multiple_inodes<'a>(
    inodes: &[&'a Inode],
) -> Vec<RwMutexWriteGuard<'a, InodeMeta>>;
```

[SPECIFICATION]

## Motivation

- The old upread/upgrade pattern existed solely to avoid PageCacheBackend
  callback self-deadlock when PageCache held internal locks.
- With split locks, backend callbacks take `mapping.read()` only.
- Directory mutations are serialized via `meta.write()`, so the old choreography
  becomes harmful complexity.

## Multi-inode locking

- When acquiring multiple `meta.write()` locks (rename, rmdir, cross-dir ops),
  always lock by ascending inode number.
- These helpers must not take `mapping` locks.

[TEST]

## meta_*_lock_two_inodes
- Two inodes with increasing ino → locks in the same order.
- Two inodes with decreasing ino → locks by ino, returns guards in (a,b) order.

## meta_write_lock_multiple_inodes
- Input order differs from ino order → locks by ino, returns guards matching input order.
