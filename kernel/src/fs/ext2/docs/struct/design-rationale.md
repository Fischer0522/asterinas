# Phase 1 - SuperBlock/Inode Design Rationale

Scope: Stage 1 specification/design only (no code implementation changes).

Covered structures in Stage 1:
- `Ext2` / `SuperBlockMem` (`fs.rs`, `super_block.rs`)
- `Inode` / `InodeInner` / `InodeDesc` (`inode.rs`)
- `BlockGroup` / `GroupDesc` (`block_group.rs`)
- `DirEntry` / `DirEntryIter` (`dir.rs`)
- `Ext2Type` / `Ext2MountContext` (`fs_type.rs`)
- `Dirty<T>` / `IsPowerOf` (`utils.rs`)
- `InodeCacheState` / `InodeWritebackState` (`fs.rs` + `inode.rs` design)
- `BlockPath` / `MappingTraversal` (`inode.rs` mapping design)
- `OrphanState` (`super_block.rs` + `inode.rs` orphan design)
- `Feature*Set` / `FsState` / `ErrorsBehaviour` / `OsId` / `RevLevel` (superblock flags/types)
- `FilePerm` / `FileFlags` / `DirEntryFileType` (inode/dir flags/types)

## Design Decisions

### Why `Arc<RwLock<Inode>>` instead of owning `Box<Inode>`?
- **Reason**: One inode can be referenced by multiple dentries (hard links), and by open file handles concurrently.
- **Linux approach**: Shared inode lifetime via VFS inode cache + refcount (`iget_locked`/`iput`).
- **Asterinas adaptation**: `Arc`/`Weak` provides equivalent shared lifetime without manual refcount bugs.
- **Trade-off**: Small atomic refcount overhead for strict safety.

### Why `Dirty<T>` wrappers for `SuperBlock` and `InodeDesc`?
- **Reason**: Explicitly separate cached mutable state from on-disk authoritative image.
- **Linux approach**: dirty markers and explicit sync (`mark_inode_dirty`, `ext2_sync_super`).
- **Asterinas adaptation**: typed `Dirty<T>` state machine removes implicit dirty flag coupling.
- **Trade-off**: Slight API complexity for clearer writeback correctness.

### Why per-filesystem inode cache (`BTreeMap<u32, Weak<Inode>>`) not global hash?
- **Reason**: Isolation and predictable unmount cleanup in a microkernel-like architecture.
- **Linux approach**: global inode hashing integrated with global VFS.
- **Asterinas adaptation**: per-fs map preserves lookup semantics while avoiding global mutable state.
- **Trade-off**: duplicate cache structures per mount, better modularity and fault containment.

### Why explicit mount mode state (`ReadWrite` -> `ReadOnlyForced`)?
- **Reason**: Corruption policy must be observable and auditable.
- **Linux approach**: `ext2_error` updates state and may force `SB_RDONLY`.
- **Asterinas adaptation**: first-class state machine instead of ad-hoc flag mutation.
- **Trade-off**: one more runtime state enum, simpler policy reasoning.

### Why keep `Raw*` and semantic structs split?
- **Reason**: On-disk binary compatibility and runtime invariants have different concerns.
- **Linux approach**: C struct overlays with implicit conversion at use sites.
- **Asterinas adaptation**: `RawSuperBlock`/`RawInode` (`#[repr(C)] + Pod`) for wire format, semantic `SuperBlock`/`InodeDesc` for checked logic.
- **Trade-off**: conversion code needed, greatly improved invariant enforcement.

## Locking Protocol Summary

Global order (must hold everywhere):

1. `SuperBlock` lock
2. `BlockGroup` lock
3. inode cache lock
4. inode locks (ascending inode number)
5. bitmap locks

Additional rules:
- Never hold inode write lock while awaiting async bio completion.
- Truncate/allocation serialization must use a dedicated mutex-like guard (Linux `truncate_mutex` intent).
- Directory cross-inode operations (`rename/link/unlink`) follow ascending inode order to avoid cycles.
- For cross-directory rename, lock `old_dir` and `new_dir` by ascending inode number before touching child inode.

## Dirty / Clean State Model

### SuperBlock
- `Clean` -> `Dirty(reason)` -> `Syncing` -> `Clean`
- fatal corruption path: `Dirty|Clean` -> `ReadOnlyForced`

### Inode
- `Clean` -> `Dirty` -> `Writeback` -> `Clean`
- newly allocated inode: `New` (first write must apply Linux zero-init semantics)
- delete path: `Deleting` until eviction/writeback completes

## Corruption Handling Policy

- **Load-time hard failures**: reject mount/inode load for invalid magic, unsupported incompat features, invalid stale inode conditions.
- **Runtime metadata failures**: return explicit errno and escalate to fs error handler.
- **Critical failures**: switch filesystem to readonly-forced mode; keep read-only service available where possible.

## Validation Checklist (Self-check)

- [x] Every major field links to Linux source or is Asterinas-specific with justification.
- [x] Lock ordering is explicit, hierarchical, and cycle-free.
- [x] Dirty/clean transitions are explicit for superblock and inode.
- [x] Corruption path includes readonly fallback policy.
- [x] Hard-link shared inode references are supported by `Arc` semantics.
- [x] Read-heavy vs write-heavy locking patterns are split.
- [x] On-disk vs in-memory ownership boundaries are clearly documented.
- [x] Block-group descriptor + bitmap invariants are included.
- [x] Directory entry boundary/state checks mirror Linux rules.
- [x] Mount-type registration and mount-context staging are documented.
- [x] Dirty-tracking primitive semantics are documented.
- [x] Inode cache and writeback scheduling structures are documented.
- [x] Indirect-block mapping/traversal state is documented.
- [x] Orphan lifecycle structures are documented.
- [x] Superblock compatibility/error/revision type structures are documented.
- [x] Inode and directory flag/type mapping structures are documented.
