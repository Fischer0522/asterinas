# Asterinas Ext2 High-Level Roadmap (Phase-Only)

## Scope and Ground Rules
- Two-Pillars Law: Linux Ext2 logic fidelity + Asterinas-native architecture (no C-style data/flow).
- No unsafe, no panic/assert, no C-macro style; Rust OOP `impl`-based APIs only.
- Linux sources are the single source of truth for logic; Asterinas infra is the runtime substrate.
- Each phase follows: Spec -> Impl -> Verify, and must pass before moving on.

## High-Level Module Decomposition
1. On-disk formats: superblock, group descriptor, inode, directory entry, feature flags.
2. Superblock/mount: read/validate, feature gating, state/clean flags, mount options.
3. Block groups: descriptor table, counters, group selection policies.
4. Block allocation: block bitmap, allocation policies, free paths, accounting.
5. Inode allocation: inode bitmap, Orlov dir policy, inode table I/O, accounting.
6. Inode lifecycle: read/write, cache, metadata sync, permissions, timestamps.
7. Block mapping: logical->physical mapping, indirect trees, truncation/free.
8. Directory ops: lookup/add/remove/rename, rec_len alignment, filetype handling.
9. File I/O: read/write paths via PageCache, size/offset handling.
10. Symlink & special files: fast/slow symlink, device nodes, fifo/socket.
11. Consistency/limits: bounds checks, feature incompat handling, error mapping.
12. VFS integration: inode/file/super interfaces, mount/unmount wiring.

## Phase Roadmap (High-Level)

### Phase 0: Scaffolding & Interfaces
Goal: establish module layout and VFS wiring points without logic.
- Modules: prelude, errors, traits glue, minimal Ext2 struct, registration.
- Deliverables: module skeletons, empty specs for layout.

### Phase 1: On-Disk Structures & Superblock
Goal: parse/validate core disk structures and mount read-only.
- Modules: raw structs (Pod), superblock read/validate, feature flags, group desc table.
- Linux anchors: include/linux/ext2_fs.h, fs/ext2/super.c.

### Phase 2: Block Groups & Bitmaps (Read-Only Paths)
Goal: load/validate group descriptors and bitmaps, expose counters.
- Modules: block group cache, bitmap read paths, group accounting.
- Linux anchors: fs/ext2/super.c, fs/ext2/balloc.c.

### Phase 3: Inode Read Path & Block Mapping
Goal: read inode from disk, map logical blocks (direct/indirect), support read-only file access.
- Modules: inode table I/O, inode cache, block_to_path, get_block logic.
- Linux anchors: fs/ext2/inode.c.

### Phase 4: Directory Read Path
Goal: traverse directories (lookup/readdir) in read-only mode.
- Modules: dir entry parsing, find_entry, readdir.
- Linux anchors: fs/ext2/dir.c, fs/ext2/namei.c.

### Phase 5: Allocation (Blocks & Inodes) + Write Enable
Goal: enable writable mount with correct allocation semantics.
- Modules: block allocator, inode allocator, bitmap updates, group selection (Orlov).
- Linux anchors: fs/ext2/balloc.c, fs/ext2/ialloc.c.

### Phase 6: Directory Mutation & File Creation
Goal: implement create/link/unlink/mkdir/rmdir/rename.
- Modules: add_link, delete_entry, make_empty, namei ops.
- Linux anchors: fs/ext2/dir.c, fs/ext2/namei.c.

### Phase 7: File Write, Resize, Truncate
Goal: full data I/O and size changes with indirect-tree updates.
- Modules: write path, truncate blocks, free branches, sync metadata.
- Linux anchors: fs/ext2/inode.c, fs/ext2/file.c.

### Phase 8: Metadata & Special Files
Goal: implement symlink/device/fifo/socket behaviors and metadata updates.
- Modules: fast/slow symlink, chmod/chown/utime, special inode types.
- Linux anchors: fs/ext2/inode.c, fs/ext2/namei.c.

### Phase 9: Robustness & Compliance
Goal: finalize boundary checks, feature gating, error mapping, and consistency.
- Modules: feature compat/incompat handling, limits, edge cases, stats.
- Linux anchors: fs/ext2/super.c, fs/ext2/inode.c.

## Phase Completion Gate (applies to every phase)
- Spec written to `kernel/src/fs/ext2/spec/` with pre/post and lock protocol.
- Implementation follows Asterinas infra (`PageCache`, `BlockDevice`, `RwMutex`, etc.).
- Verification against Linux logic passes (state transitions, boundary checks, algorithms).

## Notes
- ext2_old logic must not be referenced (only trait signatures/registration patterns allowed).
- All Linux source references in code must include file:line comments.
