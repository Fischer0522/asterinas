// SPDX-License-Identifier: MPL-2.0
//
// Layer 1: Crash Hoare Logic -- Symlink, Sync, and Fallocate Operations
//
// Five HOARE specs for operations that read/write symlink targets,
// establish durability checkpoints, and pre-allocate file space.
//
// Reference: layer1_hoare/00-abstract-state.spec for AbstractFS model.
// Notation: {P} C {Q} down-arrow {R}  (Crash Hoare Logic quadruple)

/// =============================================================================
/// HOARE SPEC 1: read_link
/// =============================================================================
/// Reads the symlink target string from either fast-inline or slow-pagecache
/// storage. Pure read -- no state mutation.
///
/// CODE: kernel/src/fs/ext2/inode.rs:424-462
/// LINUX_REF: fs/ext2/inode.c:1483-1487 (fast), fs/namei.c:6227 (slow)

HOARE read_link(ino) {
    PRE:
        ino in dom(FS.inodes)
        FS.inodes[ino].type_ = SymLink
        FS.inodes[ino].alive

    POST (Ok(target)):
        -- target is the symlink destination as a UTF-8 string
        target = string(FS.data[ino])
        -- Pure read: no state change
        FS' = FS

        FRAME: everything unchanged

    POST_ERR (Err(e)):
        FS' = FS
        e in {
            EINVAL,     -- not a symlink
            EIO,        -- fs dropped, invalid block size, UTF-8 decode failure
        }

    CRASH:
        -- Pure read, no writes to any state
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/inode.c:1483-1487
}

/// =============================================================================
/// HOARE SPEC 2: write_link
/// =============================================================================
/// Writes the symlink target string. Two storage paths:
///   - Fast symlink: target (with NUL) fits in block_ptrs (<=60 bytes)
///   - Slow symlink: target stored in data blocks via page cache
///
/// CODE: kernel/src/fs/ext2/inode.rs:468-565
/// LINUX_REF: fs/ext2/namei.c:165-191 (ext2_symlink)

HOARE write_link(ino, target) {
    PRE:
        ino in dom(FS.inodes)
        FS.inodes[ino].type_ = SymLink
        FS.inodes[ino].alive
        |target| + 1 <= FS.sb.block_size       -- with NUL terminator

    POST (Ok(())):
        -- Data updated to new target
        FS'.data[ino] = bytes(target)
        FS'.inodes[ino].size = |target|

        -- Fast path: no data blocks consumed
        IF |target| + 1 <= MAX_FAST_SYMLINK_LEN:
            FS'.inodes[ino].blocks = 0

        -- Slow path: blocks allocated for target storage
        IF |target| + 1 > MAX_FAST_SYMLINK_LEN:
            FS'.inodes[ino].blocks >= blocks_for(|target|, FS.sb.block_size)

        FRAME: FS'.inodes[j] = FS.inodes[j]  for j != ino
               FS'.dirs = FS.dirs
               FS'.xattrs = FS.xattrs
               FS'.inodes[ino].type_ = FS.inodes[ino].type_
               FS'.inodes[ino].mode = FS.inodes[ino].mode
               FS'.inodes[ino].uid = FS.inodes[ino].uid
               FS'.inodes[ino].gid = FS.inodes[ino].gid
               FS'.inodes[ino].links_count = FS.inodes[ino].links_count

    POST_ERR (Err(e)):
        FS' = FS
        e in {
            EINVAL,         -- not a symlink
            ENAMETOOLONG,   -- |target| + 1 > block_size
            ENOSPC,         -- slow path: cannot allocate data blocks
            EIO,            -- fs dropped, page cache write failure
        }

    CRASH:
        -- Fast path (|target|+1 <= 60): single inode write, atomic-ish
        IF |target| + 1 <= MAX_FAST_SYMLINK_LEN:
            FS_recovered in {FS.durable, FS'.durable}

        -- Slow path: multi-step (alloc blocks, write pages, persist inode)
        IF |target| + 1 > MAX_FAST_SYMLINK_LEN:
            FS_recovered in {
                FS.durable,
                -- Partial: blocks allocated, size updated, data unwritten/partial
                FS_partial where
                    FS_partial.inodes[ino].size in {FS.inodes[ino].size, |target|}
                    AND allocated_blocks(FS_partial) >= allocated_blocks(FS),
                FS'.durable,
            }
        -- Note: fsck reclaims orphan blocks in partial states

    LINUX_REF: fs/ext2/namei.c:165-191
}

/// =============================================================================
/// HOARE SPEC 3: sync_all (fsync)
/// =============================================================================
/// Full fsync: flushes data pages, persists inode metadata, and issues a
/// device cache flush. Establishes a durability checkpoint -- after successful
/// return, both data and metadata for this inode are on stable storage.
///
/// CODE: kernel/src/fs/ext2/inode.rs:1166-1184
/// LINUX_REF: fs/buffer.c:646 (generic_buffers_fsync)

HOARE sync_all(ino) {
    PRE:
        ino in dom(FS.inodes)
        FS.inodes[ino].alive

    POST (Ok(())):
        -- Durability checkpoint: in-memory state becomes durable
        FS'.durable.inodes[ino] = FS'.inodes[ino]
        FS'.durable.data[ino]   = FS'.data[ino]

        -- In-memory state itself is unchanged
        FS'.inodes[ino] = FS.inodes[ino]
        FS'.data[ino]   = FS.data[ino]
        FS'.dirs[ino]   = FS.dirs[ino]

        -- Superblock counters may be refreshed (sync_metadata side effect)
        FS'.sb.free_blocks = actual_free_blocks(FS')
        FS'.sb.free_inodes = actual_free_inodes(FS')

        FRAME: FS'.inodes[j] = FS.inodes[j]  for j != ino
               FS'.data[j]   = FS.data[j]    for j != ino
               FS'.dirs = FS.dirs
               FS'.xattrs = FS.xattrs

    POST_ERR (Err(e)):
        FS' = FS
        e in {
            EIO,    -- fs dropped, page cache writeback failure, device sync failure
        }

    CRASH:
        -- Three crash windows map to two possible recovered states:
        --   1. Before data writeback completes: old durable state
        --   2. After data writeback but before device flush: partial
        --   3. After device flush: new durable state
        FS_recovered in {
            FS.durable,
            -- Partial: data pages on disk but inode metadata may be stale
            FS_partial where
                FS_partial.data[ino] in {FS.durable.data[ino], FS.data[ino]}
                AND FS_partial.inodes[ino] in {FS.durable.inodes[ino], FS.inodes[ino]},
            FS'.durable,
        }

    LINUX_REF: fs/buffer.c:646
}

/// =============================================================================
/// HOARE SPEC 4: sync_data (fdatasync)
/// =============================================================================
/// Data-only sync: flushes data pages, conditionally persists inode metadata
/// (only if dirty), and issues a device cache flush. Weaker than sync_all --
/// metadata durability is not guaranteed unless the descriptor was dirty.
///
/// CODE: kernel/src/fs/ext2/inode.rs:1214-1236
/// LINUX_REF: fs/buffer.c:609 (file_write_and_wait_range)

HOARE sync_data(ino) {
    PRE:
        ino in dom(FS.inodes)
        FS.inodes[ino].alive

    POST (Ok(())):
        -- Data is always made durable
        FS'.durable.data[ino] = FS'.data[ino]

        -- Metadata is made durable only if descriptor was dirty
        -- (conservative: Asterinas uses desc.is_dirty() as I_DIRTY_DATASYNC proxy)
        IF metadata_was_dirty(FS, ino):
            FS'.durable.inodes[ino] = FS'.inodes[ino]

        -- In-memory state itself is unchanged
        FS'.inodes[ino] = FS.inodes[ino]
        FS'.data[ino]   = FS.data[ino]

        FRAME: FS'.inodes[j] = FS.inodes[j]  for j != ino
               FS'.data[j]   = FS.data[j]    for j != ino
               FS'.dirs = FS.dirs
               FS'.xattrs = FS.xattrs

    POST_ERR (Err(e)):
        FS' = FS
        e in {
            EIO,    -- fs dropped, page cache writeback failure, device sync failure
        }

    CRASH:
        FS_recovered in {
            FS.durable,
            -- Partial: data on disk, metadata may be stale
            FS_partial where
                FS_partial.data[ino] in {FS.durable.data[ino], FS.data[ino]}
                AND FS_partial.inodes[ino] = FS.durable.inodes[ino],
            FS'.durable,
        }

    LINUX_REF: fs/buffer.c:609
}

/// =============================================================================
/// HOARE SPEC 5: fallocate
/// =============================================================================
/// Pre-allocates or deallocates file space depending on mode.
/// Four modes: PunchHoleKeepSize, Allocate, AllocateKeepSize, unsupported.
///
/// CODE: kernel/src/fs/ext2/inode.rs:1138-1164
/// LINUX_REF: fs/ext2/file.c:313-328 (no native .fallocate in Linux ext2)

HOARE fallocate(ino, mode, offset, len) {
    PRE:
        ino in dom(FS.inodes)
        FS.inodes[ino].type_ = Reg
        FS.inodes[ino].alive

    POST (Ok(())):
        CASE mode = PunchHoleKeepSize:
            -- Zero-fill the range [offset, min(size, offset+len))
            -- Size is preserved (keep-size semantics)
            IF offset >= FS.inodes[ino].size:
                FS' = FS      -- no-op when offset beyond EOF
            ELSE:
                let end = min(FS.inodes[ino].size, offset + len)
                FS'.data[ino][offset..end] = zeros(end - offset)
                FS'.data[ino][0..offset] = FS.data[ino][0..offset]
                FS'.data[ino][end..] = FS.data[ino][end..]
                FS'.inodes[ino].size = FS.inodes[ino].size

        CASE mode = Allocate:
            -- Extend file size if offset+len exceeds current size
            IF offset + len > FS.inodes[ino].size:
                FS'.inodes[ino].size = offset + len
                FS'.inodes[ino].blocks >= blocks_for(offset + len, FS.sb.block_size)
            ELSE:
                FS' = FS      -- no-op when already large enough

        CASE mode = AllocateKeepSize:
            -- No-op: Asterinas does not pre-allocate without extending
            FS' = FS

        FRAME: FS'.inodes[j] = FS.inodes[j]  for j != ino
               FS'.dirs = FS.dirs
               FS'.xattrs = FS.xattrs
               FS'.inodes[ino].type_ = FS.inodes[ino].type_

    POST_ERR (Err(e)):
        FS' = FS
        e in {
            EOPNOTSUPP,    -- unsupported fallocate mode
            ENOSPC,        -- Allocate mode: cannot allocate blocks for resize
            EIO,           -- page cache or device I/O failure
        }

    CRASH:
        CASE mode = PunchHoleKeepSize:
            -- Zeros written to page cache only, not persisted
            FS_recovered = FS.durable

        CASE mode = Allocate:
            -- Delegates to resize; partial block allocation possible
            FS_recovered in {
                FS.durable,
                FS_partial where
                    FS_partial.inodes[ino].size in {FS.inodes[ino].size, offset + len}
                    AND allocated_blocks(FS_partial) >= allocated_blocks(FS),
                FS'.durable,
            }

        CASE mode = AllocateKeepSize:
            -- No state change
            FS_recovered = FS.durable

    LINUX_REF: fs/ext2/file.c:313-328
}
