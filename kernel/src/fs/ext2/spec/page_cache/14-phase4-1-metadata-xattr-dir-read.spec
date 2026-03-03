[PROMPT]
Phase 4.1 (small rewires): rewire the following `Inode` operations to the
split-lock layout (spec 11) with minimal behavioral changes:

- metadata getters/setters ("set/get")
- xattr get/list/set/remove glue
- read-only directory ops: lookup, readdir_at

Directory mutations are serialized (Phase 4.4), so directory reads must acquire
`meta.read()` and will be blocked while a mutation holds `meta.write()`.

Provide modifications to:

- `kernel/src/fs/ext2/inode.rs`
- `kernel/src/fs/ext2/impl_for_vfs/inode.rs` (only if signatures changed)

Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_iget               → fs/ext2/inode.c:1387
__ext2_write_inode      → fs/ext2/inode.c:1589
ext2_get_block          → fs/ext2/inode.c:783
ext2_find_entry         → fs/ext2/dir.c:342
ext2_readdir            → fs/ext2/dir.c:257
ext2_xattr_get          → fs/ext2/xattr.c:195
ext2_xattr_list         → fs/ext2/xattr.c:287
ext2_xattr_set          → fs/ext2/xattr.c:405

[RELY]
```rust
use super::prelude::*;
use super::fs::Ext2;
```

```rust
pub(super) struct InodeInner { /* from spec 11 */ }
pub(super) struct InodeMeta { /* from spec 10 */ }
pub(super) struct InodeMapping { /* from spec 10 */ }
```

[GUARANTEE]

The following methods must be updated to use split locks.

```rust
impl Inode {
    pub(super) fn file_size(&self) -> usize;

    pub(super) fn device_id(&self) -> u64;
    pub(super) fn set_device_id(&self, device_id: u64) -> Result<()>;

    pub(super) fn metadata(&self) -> Metadata;
    pub(super) fn inode_type(&self) -> InodeType;

    pub(super) fn mode(&self) -> InodeMode;
    pub(super) fn set_mode(&self, mode: InodeMode) -> Result<()>;

    pub(super) fn uid(&self) -> u32;
    pub(super) fn set_uid(&self, uid: u32) -> Result<()>;
    pub(super) fn gid(&self) -> u32;
    pub(super) fn set_gid(&self, gid: u32) -> Result<()>;

    pub(super) fn atime(&self) -> Duration;
    pub(super) fn set_atime(&self, time: Duration);
    pub(super) fn mtime(&self) -> Duration;
    pub(super) fn set_mtime(&self, time: Duration);
    pub(super) fn ctime(&self) -> Duration;
    pub(super) fn set_ctime(&self, time: Duration);

    pub(super) fn get_xattr(&self, name: XattrName, value_writer: &mut VmWriter) -> Result<usize>;
    pub(super) fn list_xattr(
        &self,
        namespace: XattrNamespace,
        list_writer: &mut VmWriter,
    ) -> Result<usize>;
    pub(super) fn set_xattr(
        &self,
        name: XattrName,
        value_reader: &mut VmReader,
        flags: XattrSetFlags,
    ) -> Result<()>;
    pub(super) fn remove_xattr(&self, name: XattrName) -> Result<()>;

    pub(super) fn lookup(&self, name: &str) -> Result<Arc<Inode>>;
    pub(super) fn readdir_at(
        &self,
        offset: usize,
        visitor: &mut dyn DirentVisitor,
    ) -> Result<usize>;
}
```

[SPECIFICATION]

## General locking

- Read-only metadata access uses `meta.read()`.
- Metadata mutation uses `meta.write()`.
- When both domains are needed (e.g., `metadata()` needs blocks from mapping),
  lock order is `meta` then `mapping`.

## Behavior preservation

- Keep existing persistence behavior:
  - Methods that previously persisted immediately (e.g., `set_device_id`, xattr
    set/remove) must still persist.
  - Simple setters that previously only mutated in-memory state may continue to
    do so (they will be persisted by later sync/writeback mechanisms).
- Where persistence occurs, it must be done via `InodeInner::persist_inode_locked`
  (take `meta.write()` + `mapping.write()` as required by spec 11).

## Directory reads vs directory mutations

- `lookup` and `readdir_at` must take `meta.read()` for the duration of the scan.
  - This blocks while a directory mutation holds `meta.write()` (serialization).
  - While `meta.read()` is held, PageCache reads may trigger backend callbacks,
    which take `mapping.read()` only (allowed).

[DIFF]

Linux: VFS attribute changes set inode dirty and rely on writeback.
  → Asterinas: some setters persist immediately while others rely on explicit
    sync paths (preserve existing behavior).

[TEST]

## metadata
- Metadata reflects updated `i_size` (from meta) and `i_blocks` (from mapping).

## xattr set/remove
- Setting/removing xattr updates `file_acl` and persists inode (read back raw inode).

## lookup/readdir_at
- lookup finds existing entry, returns inode.
- readdir_at iterates stable snapshot while mutation is blocked by `meta.write()`.
