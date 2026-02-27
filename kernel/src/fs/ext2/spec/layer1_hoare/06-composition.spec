// SPDX-License-Identifier: MPL-2.0
//
// Layer 1: Crash Hoare Logic -- Composition Properties & Invariant Preservation
//
// 10 composition properties (sequential operation pairs satisfying algebraic laws)
// + invariant preservation proofs for INV-01 through INV-10.
//
// Each composition property is a HOARE spec over a sequential pair of operations,
// referencing individual HOARE specs from 01-metadata-ops through 05-xattr-fs.
//
// Reference: 00-abstract-state.spec for AbstractFS model, invariants, notation.
// Reference: 01-metadata-ops.spec through 05-xattr-fs.spec for individual specs.
//
// Notation:
//   FS       -- pre-state  (AbstractFS before first operation)
//   FS'      -- intermediate state (after first operation)
//   FS''     -- final state (after second operation)
//   {P} C1;C2 {Q} down {R} -- sequential composition

/// =============================================================================
/// SECTION 1: COMPOSITION PROPERTIES (10 algebraic laws)
/// =============================================================================

/// ---------------------------------------------------------------------------
/// COMP-1: Create-Lookup Roundtrip
/// ---------------------------------------------------------------------------
/// Creating a file and immediately looking it up returns the created child.
///
/// References: 03-dir-ops.spec (create, lookup)

COMPOSITION COMP-1: create_lookup_roundtrip {
    SEQUENCE:
        C1 = create(dir_ino, name, type_, mode)   -- from HOARE create
        C2 = lookup(dir_ino, name)                 -- from HOARE lookup

    PRE:
        -- Combined preconditions of create:
        dir_ino in dom(FS.inodes)
        FS.inodes[dir_ino].alive
        FS.inodes[dir_ino].type_ = Dir
        name != "" AND |name| <= MAX_NAME_LEN
        name not in {".", ".."}
        type_ in {Reg, SymLink, CharDevice, BlockDevice, NamedPipe}
        name not in dom(FS.dirs[dir_ino])
        FS.sb.free_inodes > 0

    POST (Ok(new_ino), Ok(found_ino)):
        -- After create succeeds, lookup must find the created child:
        found_ino = new_ino

        -- Proof:
        --   By HOARE create POST: FS'.dirs[dir_ino][name] = (new_ino, type_)
        --   By HOARE lookup POST: (found_ino, _) = FS'.dirs[dir_ino][name]
        --   Therefore: found_ino = new_ino.
        --   HOARE lookup FRAME: FS'' = FS' (lookup is pure read).

    CRASH:
        -- If crash after C1 but before C2:
        --   FS_recovered in HOARE create CRASH set.
        --   C2 never executes; roundtrip property does not apply.
        -- If crash during C2:
        --   C2 is a pure read; FS_recovered = FS'.durable.
        --   Roundtrip holds iff create's persist completed before crash.
}

/// ---------------------------------------------------------------------------
/// COMP-2: Link-Unlink Inverse
/// ---------------------------------------------------------------------------
/// Linking a file and then unlinking the same name restores the original
/// directory state (modulo timestamps and link count).
///
/// References: 03-dir-ops.spec (link, unlink)

COMPOSITION COMP-2: link_unlink_inverse {
    SEQUENCE:
        C1 = link(dir_ino, child_ino, name)        -- from HOARE link
        C2 = unlink(dir_ino, name)                  -- from HOARE unlink

    PRE:
        dir_ino in dom(FS.inodes)
        child_ino in dom(FS.inodes)
        FS.inodes[dir_ino].alive
        FS.inodes[dir_ino].type_ = Dir
        FS.inodes[child_ino].alive
        FS.inodes[child_ino].type_ != Dir
        FS.inodes[child_ino].links_count < MAX_LINK_COUNT
        name != "" AND |name| <= MAX_NAME_LEN
        name not in {".", ".."}
        name not in dom(FS.dirs[dir_ino])

    POST (Ok(()), Ok(())):
        -- Directory namespace restored:
        name not in dom(FS''.dirs[dir_ino])
        dom(FS''.dirs[dir_ino]) = dom(FS.dirs[dir_ino])

        -- Child link count restored:
        FS''.inodes[child_ino].links_count = FS.inodes[child_ino].links_count
        -- Because: link adds +1, unlink subtracts -1. Net = 0.

        -- Child still alive (was alive before, net link change = 0):
        FS''.inodes[child_ino].alive = FS.inodes[child_ino].alive

        -- Data and xattrs unchanged:
        FS''.data = FS.data
        FS''.xattrs = FS.xattrs

        -- Proof:
        --   By HOARE link POST: FS'.dirs[dir_ino][name] = (child_ino, type_)
        --     FS'.inodes[child_ino].links_count = FS.inodes[child_ino].links_count + 1
        --   By HOARE unlink POST: name not in dom(FS''.dirs[dir_ino])
        --     FS''.inodes[child_ino].links_count = FS'.inodes[child_ino].links_count - 1
        --       = (FS.inodes[child_ino].links_count + 1) - 1
        --       = FS.inodes[child_ino].links_count
        --   By HOARE link FRAME + unlink FRAME: data, xattrs unchanged.

    NOTE:
        -- Timestamps (ctime of child, mtime/ctime of dir) are NOT restored.
        -- This is expected: timestamps are monotonically advancing side effects.

    CRASH:
        -- Crash between C1 and C2: link completed, unlink not started.
        --   FS_recovered has the extra link. fsck-safe (valid state).
        -- Crash during C2: partial unlink.
        --   FS_recovered in HOARE unlink CRASH set.
}

/// ---------------------------------------------------------------------------
/// COMP-3: Create-Rmdir Inverse (mkdir then rmdir)
/// ---------------------------------------------------------------------------
/// Creating a directory and then removing it restores the parent directory
/// namespace and link count (modulo timestamps).
///
/// References: 03-dir-ops.spec (mkdir, rmdir)

COMPOSITION COMP-3: mkdir_rmdir_inverse {
    SEQUENCE:
        C1 = mkdir(dir_ino, name, mode)            -- from HOARE mkdir
        C2 = rmdir(dir_ino, name)                   -- from HOARE rmdir

    PRE:
        dir_ino in dom(FS.inodes)
        FS.inodes[dir_ino].alive
        FS.inodes[dir_ino].type_ = Dir
        name != "" AND |name| <= MAX_NAME_LEN
        name not in {".", ".."}
        name not in dom(FS.dirs[dir_ino])
        FS.sb.free_inodes > 0
        FS.inodes[dir_ino].links_count < MAX_LINK_COUNT

    POST (Ok(new_ino), Ok(())):
        -- Directory namespace restored:
        name not in dom(FS''.dirs[dir_ino])
        dom(FS''.dirs[dir_ino]) = dom(FS.dirs[dir_ino])

        -- Parent link count restored:
        FS''.inodes[dir_ino].links_count = FS.inodes[dir_ino].links_count
        -- Because: mkdir adds +1 (child's ".."), rmdir subtracts -1. Net = 0.

        -- Child inode freed:
        FS''.inodes[new_ino].alive = false
        FS''.inodes[new_ino].links_count = 0

        -- Data and xattrs unchanged:
        FS''.data = FS.data
        FS''.xattrs = FS.xattrs

        -- Proof:
        --   By HOARE mkdir POST:
        --     FS'.dirs[dir_ino][name] = (new_ino, Dir)
        --     FS'.inodes[dir_ino].links_count = FS.inodes[dir_ino].links_count + 1
        --     FS'.inodes[new_ino].links_count = 2
        --     is_empty_dir(FS', new_ino) (only "." and "..")
        --   By HOARE rmdir PRE: is_empty_dir satisfied (mkdir creates empty dir).
        --   By HOARE rmdir POST:
        --     name not in dom(FS''.dirs[dir_ino])
        --     FS''.inodes[dir_ino].links_count = FS'.inodes[dir_ino].links_count - 1
        --       = (FS.inodes[dir_ino].links_count + 1) - 1
        --       = FS.inodes[dir_ino].links_count
        --     FS''.inodes[new_ino].links_count = 0, alive = false

    CRASH:
        -- Crash between C1 and C2: mkdir completed, rmdir not started.
        --   FS_recovered has the new directory. Valid state.
        -- Crash during C2: partial rmdir.
        --   FS_recovered in HOARE rmdir CRASH set.
}

/// ---------------------------------------------------------------------------
/// COMP-4: Rename Preserves Inode Identity
/// ---------------------------------------------------------------------------
/// Renaming an entry preserves the inode number of the moved file.
///
/// References: 03-dir-ops.spec (rename, lookup)

COMPOSITION COMP-4: rename_preserves_identity {
    SEQUENCE:
        C1 = rename(src_dir, old_name, dst_dir, new_name)  -- from HOARE rename
        C2 = lookup(dst_dir, new_name)                      -- from HOARE lookup

    PRE:
        src_dir in dom(FS.inodes)
        dst_dir in dom(FS.inodes)
        FS.inodes[src_dir].alive AND FS.inodes[dst_dir].alive
        FS.inodes[src_dir].type_ = Dir AND FS.inodes[dst_dir].type_ = Dir
        old_name in dom(FS.dirs[src_dir])
        new_name not in dom(FS.dirs[dst_dir])   -- no replacement case
        old_name not in {".", ".."} AND new_name not in {".", ".."}
        LET (moved_ino, moved_type) = FS.dirs[src_dir][old_name]

    POST (Ok(()), Ok(found_ino)):
        -- The looked-up inode is the same one that was renamed:
        found_ino = moved_ino

        -- Inode identity preserved (type_ immutable):
        FS''.inodes[moved_ino].type_ = FS.inodes[moved_ino].type_

        -- Proof:
        --   By HOARE rename POST: FS'.dirs[dst_dir][new_name] = (moved_ino, moved_type)
        --   By HOARE lookup POST: (found_ino, _) = FS'.dirs[dst_dir][new_name]
        --   Therefore: found_ino = moved_ino.
        --   By INV-01: type_ is immutable across all operations.

    CRASH:
        -- Crash between C1 and C2: rename completed, lookup not started.
        --   FS_recovered in HOARE rename CRASH set.
        --   In all crash states, moved_ino retains its identity (INV-01).
}

/// ---------------------------------------------------------------------------
/// COMP-5: Write-Read Roundtrip
/// ---------------------------------------------------------------------------
/// Writing data and then reading it back returns the written bytes.
///
/// References: 02-file-io.spec (write_at, read_at)

COMPOSITION COMP-5: write_read_roundtrip {
    SEQUENCE:
        C1 = write_at(ino, offset, buf)            -- from HOARE write_at
        C2 = read_at(ino, offset, |buf|)            -- from HOARE read_at

    PRE:
        ino in dom(FS.inodes)
        FS.inodes[ino].alive
        FS.inodes[ino].type_ != Dir
        |buf| > 0

    POST (Ok(n_written), Ok(n_read)):
        n_written = |buf|
        n_read = |buf|
        returned_data = buf

        -- Proof:
        --   By HOARE write_at POST:
        --     FS'.data[ino] = splice(padded, offset, buf)
        --     FS'.inodes[ino].size = max(old_size, offset + |buf|)
        --   By HOARE read_at POST:
        --     offset < FS'.inodes[ino].size  (since size >= offset + |buf| > offset)
        --     n_read = min(|buf|, FS'.inodes[ino].size - offset)
        --           = min(|buf|, max(old_size, offset+|buf|) - offset)
        --           >= min(|buf|, |buf|) = |buf|
        --     returned_data = FS'.data[ino][offset .. offset + |buf|]
        --                   = buf  (by splice definition)

    CRASH:
        -- Crash between C1 and C2: write completed in memory, read not started.
        --   If write was persisted: roundtrip holds after recovery.
        --   If write was not persisted: FS_recovered = FS.durable,
        --     read returns old data. Roundtrip does NOT hold.
        -- This is expected for a non-journaled filesystem.
}

/// ---------------------------------------------------------------------------
/// COMP-6: Resize-Size Consistency
/// ---------------------------------------------------------------------------
/// Resizing a file to n bytes and then reading size() returns n.
///
/// References: 01-metadata-ops.spec (resize, size)

COMPOSITION COMP-6: resize_size_consistency {
    SEQUENCE:
        C1 = resize(ino, new_size)                  -- from HOARE resize
        C2 = size(ino)                               -- from HOARE size

    PRE:
        ino in dom(FS.inodes)
        FS.inodes[ino].alive
        FS.inodes[ino].type_ in {Reg, Dir, SymLink}
        NOT (is_fast_symlink(FS, ino) AND FS.inodes[ino].size > 0)

    POST (Ok(()), Ok(ret)):
        ret = new_size

        -- Proof:
        --   By HOARE resize POST: FS'.inodes[ino].size = new_size
        --   By HOARE size POST: ret = FS'.inodes[ino].size = new_size
        --   HOARE size FRAME: FS'' = FS' (size is a pure read).

    CRASH:
        -- Crash between C1 and C2: resize completed in memory.
        --   If persist completed: size returns new_size after recovery.
        --   If persist did not complete: FS_recovered = FS.durable,
        --     size returns old size. Consistency does NOT hold.
}

/// ---------------------------------------------------------------------------
/// COMP-7: Xattr Set-Get Roundtrip
/// ---------------------------------------------------------------------------
/// Setting an xattr and then getting it returns the written value.
///
/// References: 05-xattr-fs.spec (set_xattr, get_xattr)

COMPOSITION COMP-7: xattr_set_get_roundtrip {
    SEQUENCE:
        C1 = set_xattr(ino, name, value, flags)    -- from HOARE set_xattr
        C2 = get_xattr(ino, name, buf)              -- from HOARE get_xattr

    PRE:
        ino in dom(FS.inodes)
        FS.inodes[ino].alive
        FS.inodes[ino].type_ in {Dir, Reg}
        |buf| >= |value|
        -- flags allow creation or replacement as appropriate

    POST (Ok(()), Ok(size)):
        size = |value|
        buf[0..size] = value

        -- Proof:
        --   By HOARE set_xattr POST: FS'.xattrs[ino][name] = value
        --   By HOARE get_xattr POST:
        --     name in dom(FS'.xattrs[ino])  (established by set_xattr)
        --     buf[0..size] = FS'.xattrs[ino][name] = value
        --     size = |FS'.xattrs[ino][name]| = |value|
        --   HOARE get_xattr FRAME: FS'' = FS' (get is pure read).

    CRASH:
        -- Crash between C1 and C2: set_xattr may or may not have persisted.
        --   If xattr block persisted but inode file_acl not updated:
        --     orphan xattr block; get_xattr may return ENODATA.
        --   Roundtrip holds only if both phases of set_xattr persisted.
}

/// ---------------------------------------------------------------------------
/// COMP-8: Xattr Set-Remove-Get Returns ENODATA
/// ---------------------------------------------------------------------------
/// Setting an xattr, removing it, then getting it returns ENODATA.
///
/// References: 05-xattr-fs.spec (set_xattr, remove_xattr, get_xattr)

COMPOSITION COMP-8: xattr_set_remove_get {
    SEQUENCE:
        C1 = set_xattr(ino, name, value, flags)    -- from HOARE set_xattr
        C2 = remove_xattr(ino, name)                -- from HOARE remove_xattr
        C3 = get_xattr(ino, name, buf)              -- from HOARE get_xattr

    PRE:
        ino in dom(FS.inodes)
        FS.inodes[ino].alive
        FS.inodes[ino].type_ in {Dir, Reg}

    POST (Ok(()), Ok(()), Err(e)):
        e = ENODATA

        -- Proof:
        --   By HOARE set_xattr POST: FS'.xattrs[ino][name] = value
        --   By HOARE remove_xattr POST:
        --     FS''.xattrs[ino] = FS'.xattrs[ino] \ {name}
        --     Therefore: name not in dom(FS''.xattrs[ino])
        --   By HOARE get_xattr POST_ERR:
        --     name not in dom(FS''.xattrs[ino]) => Err(ENODATA)

    CRASH:
        -- Crash after C1, before C2: xattr exists. get_xattr returns value.
        -- Crash after C2, before C3: xattr removed. get_xattr returns ENODATA.
        -- Crash during C2: partial remove.
        --   FS_recovered in HOARE remove_xattr CRASH set.
}

/// ---------------------------------------------------------------------------
/// COMP-9: Symlink Write-Read Roundtrip
/// ---------------------------------------------------------------------------
/// Writing a symlink target and then reading it returns the written target.
///
/// References: 04-symlink-sync.spec (write_link, read_link)

COMPOSITION COMP-9: symlink_write_read_roundtrip {
    SEQUENCE:
        C1 = write_link(ino, target)                -- from HOARE write_link
        C2 = read_link(ino)                          -- from HOARE read_link

    PRE:
        ino in dom(FS.inodes)
        FS.inodes[ino].type_ = SymLink
        FS.inodes[ino].alive
        |target| + 1 <= FS.sb.block_size

    POST (Ok(()), Ok(ret)):
        ret = target

        -- Proof:
        --   By HOARE write_link POST:
        --     FS'.data[ino] = bytes(target)
        --     FS'.inodes[ino].size = |target|
        --   By HOARE read_link POST:
        --     ret = string(FS'.data[ino])
        --         = string(bytes(target))
        --         = target
        --   HOARE read_link FRAME: FS'' = FS' (read_link is pure read).

    CRASH:
        -- Fast path (|target|+1 <= 60):
        --   Crash between C1 and C2: single inode write.
        --   FS_recovered in {FS.durable, FS'.durable}.
        --   Roundtrip holds iff persist completed.
        -- Slow path:
        --   Crash between C1 and C2: multi-step write.
        --   FS_recovered in {FS.durable, FS_partial, FS'.durable}.
        --   Roundtrip holds only if all phases completed.
}

/// ---------------------------------------------------------------------------
/// COMP-10: Sync Idempotence
/// ---------------------------------------------------------------------------
/// Calling sync_all twice produces the same durable state as calling it once.
///
/// References: 04-symlink-sync.spec (sync_all)

COMPOSITION COMP-10: sync_idempotence {
    SEQUENCE:
        C1 = sync_all(ino)                          -- from HOARE sync_all
        C2 = sync_all(ino)                          -- from HOARE sync_all

    PRE:
        ino in dom(FS.inodes)
        FS.inodes[ino].alive

    POST (Ok(()), Ok(())):
        -- Durable state after two syncs equals durable state after one:
        FS''.durable.inodes[ino] = FS'.durable.inodes[ino]
        FS''.durable.data[ino]   = FS'.durable.data[ino]

        -- In-memory state unchanged by either sync:
        FS''.inodes[ino] = FS'.inodes[ino] = FS.inodes[ino]
        FS''.data[ino]   = FS'.data[ino]   = FS.data[ino]

        -- Proof:
        --   By HOARE sync_all POST (first call):
        --     FS'.durable.inodes[ino] = FS'.inodes[ino] = FS.inodes[ino]
        --     FS'.durable.data[ino]   = FS'.data[ino]   = FS.data[ino]
        --     FS'.inodes[ino] = FS.inodes[ino]  (sync does not mutate in-memory)
        --   By HOARE sync_all POST (second call):
        --     FS''.durable.inodes[ino] = FS''.inodes[ino] = FS'.inodes[ino]
        --     FS''.durable.data[ino]   = FS''.data[ino]   = FS'.data[ino]
        --   Since FS'.inodes[ino] = FS.inodes[ino] and FS'.data[ino] = FS.data[ino]:
        --     FS''.durable = FS'.durable  (for this inode).
        --   The second sync is a no-op on already-clean data.

    CRASH:
        -- Crash between C1 and C2: first sync completed.
        --   FS_recovered = FS'.durable (first sync established checkpoint).
        -- Crash during C2: second sync in progress.
        --   FS_recovered in {FS'.durable, FS''.durable}.
        --   Since FS'.durable = FS''.durable for this inode, both are equivalent.
        --   Idempotence holds even under crash.
}

/// =============================================================================
/// SECTION 2: INVARIANT PRESERVATION PROOFS
/// =============================================================================
///
/// For each invariant INV-01 through INV-10 (from 00-abstract-state.spec),
/// we identify which HOARE specs could threaten it and prove they preserve it
/// by referencing specific POST/FRAME clauses.

/// ---------------------------------------------------------------------------
/// INV-01: inode_identity_immutable
/// ---------------------------------------------------------------------------
/// type_ is never modified after creation.
///
/// FORMAL: forall op, forall ino in dom(FS.inodes):
///             FS'.inodes[ino].type_ = FS.inodes[ino].type_

INVARIANT_PROOF INV-01 {
    THREATENED_BY:
        -- No HOARE spec modifies type_. Potential threats are operations
        -- that create new inodes or modify inode fields.

    PROOF:
        -- Pure reads (01-metadata-ops SECTION 1, 02-file-io read_at,
        --   03-dir-ops lookup/readdir_at, 04-symlink-sync read_link,
        --   05-xattr-fs get_xattr/list_xattr, fs_name/fs_root_inode/fs_sb/
        --   fs_event_subscriber_stats):
        --   All have FRAME: FS' = FS. type_ trivially preserved.

        -- Metadata mutators (set_mode, set_owner, set_group, set_atime,
        --   set_mtime, set_ctime):
        --   FRAME clauses explicitly list modified fields; type_ is never
        --   in the modified set. E.g., set_mode FRAME: "all fields unchanged
        --   except {mode, ctime}".

        -- resize:
        --   FRAME: "all fields unchanged except {size, blocks, mtime, ctime}".
        --   type_ not modified.

        -- write_at:
        --   FRAME: "FS'.inodes[ino] \ {size, mtime, ctime, blocks} =
        --           FS.inodes[ino] \ {size, mtime, ctime, blocks}".
        --   type_ preserved.

        -- write_link:
        --   FRAME: "FS'.inodes[ino].type_ = FS.inodes[ino].type_" (explicit).

        -- create, mkdir, mknod:
        --   Create new inodes with a fixed type_. Existing inodes' type_
        --   preserved by FRAME: "forall j not in {dir_ino, new_ino}:
        --   FS'.inodes[j] = FS.inodes[j]". Parent dir_ino type_ = Dir
        --   is unchanged (only mtime/ctime modified).

        -- link, unlink, rmdir:
        --   FRAME clauses preserve all fields except {links_count, ctime,
        --   dtime, alive, mtime}. type_ not in modified set.

        -- rename:
        --   FRAME: "forall j not in {src_dir, dst_dir, moved_ino, existing_ino}:
        --   FS'.inodes[j] = FS.inodes[j]". For affected inodes, only
        --   {links_count, ctime, dtime, alive, mtime} modified. type_ preserved.

        -- set_xattr, remove_xattr:
        --   FRAME: "FS'.inodes[j] = FS.inodes[j] for j != ino".
        --   For ino: only file_acl and ctime modified. type_ preserved.

        -- sync_all, sync_data, fs_sync:
        --   Do not modify in-memory inode fields. type_ preserved.

        -- fallocate:
        --   FRAME: "FS'.inodes[ino].type_ = FS.inodes[ino].type_" (explicit).

    CONCLUSION: INV-01 preserved by all HOARE specs. QED.
}

/// ---------------------------------------------------------------------------
/// INV-02: root_inode_exists
/// ---------------------------------------------------------------------------
/// ROOT_INO in dom(FS.inodes), type_ = Dir, alive, links_count >= 2.

INVARIANT_PROOF INV-02 {
    THREATENED_BY:
        -- unlink, rmdir: could free ROOT_INO if it were the target.
        -- rename: could replace ROOT_INO's entry.

    PROOF:
        -- unlink PRE: "name not in {'.', '..'}". The root inode is always
        --   reachable via "/" and is never a child entry that can be unlinked.
        --   Additionally, unlink PRE requires child_type != Dir, but ROOT_INO
        --   is Dir, so unlink cannot target it.

        -- rmdir PRE: "name not in {'.', '..'}". ROOT_INO is the mount point;
        --   it has no parent entry that can be rmdir'd (its ".." points to
        --   itself). Even if a child of root is rmdir'd, that child is not
        --   ROOT_INO.

        -- rename: Cannot rename "." or "..". ROOT_INO cannot be the moved
        --   inode in a way that would free it (rename preserves the inode).

        -- No operation sets ROOT_INO.alive = false or reduces its links_count
        --   below 2 (it always has at least "." self-link + ".." from itself
        --   or parent mount).

        -- create, mkdir: Add entries to root but do not modify root's
        --   existence or type_.

    CONCLUSION: INV-02 preserved by all HOARE specs. QED.
}

/// ---------------------------------------------------------------------------
/// INV-03: dir_dot_entries
/// ---------------------------------------------------------------------------
/// Every alive directory has "." -> (self, Dir) and ".." -> (parent, Dir).

INVARIANT_PROOF INV-03 {
    THREATENED_BY:
        -- mkdir: creates a new directory (must initialize "." and "..").
        -- rmdir: removes a directory (must not leave orphan dot entries).
        -- rename (cross-dir, dir move): updates ".." of moved directory.

    PROOF:
        -- mkdir POST:
        --   "FS'.dirs[new_ino]['.'] = (new_ino, Dir)"
        --   "FS'.dirs[new_ino]['..'] = (dir_ino, Dir)"
        --   Dot entries correctly initialized. INV-03 holds for new_ino.
        --   FRAME: existing directories' dot entries unchanged.

        -- rmdir POST:
        --   "FS'.inodes[child_ino].alive = false"
        --   INV-03 quantifies over alive directories only.
        --   Dead child_ino is excluded from the quantifier.
        --   Parent dir_ino's dot entries unchanged (FRAME).

        -- rename POST (cross-dir, dir move):
        --   "FS'.dirs[moved_ino]['..'] = (dst_dir, Dir)"
        --   ".." updated to point to new parent. "." unchanged.
        --   INV-03 holds for moved_ino with new parent.
        --   FRAME: other directories' dot entries unchanged.

        -- All other operations:
        --   FRAME clauses preserve dirs for non-affected inodes.
        --   No operation modifies "." or ".." entries except mkdir and rename.

    CONCLUSION: INV-03 preserved by all HOARE specs. QED.
}

/// ---------------------------------------------------------------------------
/// INV-04: link_count_consistency
/// ---------------------------------------------------------------------------
/// links_count equals the number of directory references to the inode.

INVARIANT_PROOF INV-04 {
    THREATENED_BY:
        -- create: adds dir entry + sets links_count = 1.
        -- mkdir: adds dir entry + sets links_count = 2, increments parent.
        -- link: adds dir entry + increments links_count.
        -- unlink: removes dir entry + decrements links_count.
        -- rmdir: removes dir entry + decrements child by 2, parent by 1.
        -- rename: moves dir entry, adjusts link counts for replacement/dir move.

    PROOF:
        -- create POST:
        --   New entry: dirs[dir_ino][name] = (new_ino, type_).
        --   new_ino.links_count = 1. One reference exists. Consistent.

        -- mkdir POST:
        --   New entry: dirs[dir_ino][name] = (new_ino, Dir).
        --   new_ino.links_count = 2 (parent entry + "." self-link).
        --   dir_ino.links_count += 1 (child's ".." reference).
        --   All reference counts match. Consistent.

        -- link POST:
        --   New entry: dirs[dir_ino][name] = (child_ino, type_).
        --   child_ino.links_count += 1. One new reference added. Consistent.

        -- unlink POST:
        --   Entry removed: name not in dirs[dir_ino].
        --   child_ino.links_count -= 1. One reference removed. Consistent.

        -- rmdir POST:
        --   Entry removed: name not in dirs[dir_ino].
        --   child_ino.links_count -= 2 (parent entry + "." self-link).
        --   dir_ino.links_count -= 1 (child's ".." removed). Consistent.

        -- rename POST:
        --   Source entry removed, destination entry added. Net reference
        --   change for moved_ino = 0 (links_count unchanged for non-dir).
        --   For dir move: ".." update adjusts parent link counts.
        --   For replacement: existing_ino links decremented by reference count.
        --   All adjustments match reference changes. Consistent.

        -- POST_ERR for all: FS' = FS. No change, invariant trivially holds.

    CONCLUSION: INV-04 preserved by all HOARE specs. QED.
}

/// ---------------------------------------------------------------------------
/// INV-05: freed_inode_marking
/// ---------------------------------------------------------------------------
/// Every non-alive inode has links_count = 0 and dtime > 0.

INVARIANT_PROOF INV-05 {
    THREATENED_BY:
        -- unlink: may set alive = false when links_count reaches 0.
        -- rmdir: sets child alive = false.
        -- rename: may set existing_ino alive = false on replacement.

    PROOF:
        -- unlink POST:
        --   "IF FS'.inodes[child_ino].links_count = 0:
        --       FS'.inodes[child_ino].alive = false
        --       FS'.inodes[child_ino].dtime > 0"
        --   links_count = 0 and dtime > 0 are set together. INV-05 holds.

        -- rmdir POST:
        --   "FS'.inodes[child_ino].links_count = 0"
        --   "FS'.inodes[child_ino].alive = false"
        --   "FS'.inodes[child_ino].dtime > 0"
        --   All three conditions satisfied simultaneously. INV-05 holds.

        -- rename POST (replacement):
        --   "IF FS'.inodes[existing_ino].links_count = 0:
        --       FS'.inodes[existing_ino].alive = false
        --       FS'.inodes[existing_ino].dtime > 0"
        --   Same pattern as unlink. INV-05 holds.

        -- No other operation sets alive = false.

    CONCLUSION: INV-05 preserved by all HOARE specs. QED.
}

/// ---------------------------------------------------------------------------
/// INV-06: block_exclusivity
/// ---------------------------------------------------------------------------
/// No block is referenced by more than one inode.

INVARIANT_PROOF INV-06 {
    THREATENED_BY:
        -- write_at: allocates new blocks via get_or_alloc_block.
        -- resize (grow): may allocate blocks (sparse, but page cache extends).
        -- write_link (slow): allocates blocks for symlink data.
        -- mkdir: allocates one data block for child directory.
        -- create/mknod: allocate inode but no data blocks (size = 0).
        -- fallocate (Allocate): delegates to resize.

    PROOF:
        -- Block allocation (get_or_alloc_block, used by write_at/write_link):
        --   Allocates from free block bitmap. A free block is by definition
        --   not referenced by any inode. After allocation, it is referenced
        --   by exactly one inode. INV-06 preserved.

        -- resize (shrink): frees blocks, returning them to free bitmap.
        --   Freed blocks are no longer referenced. INV-06 preserved.

        -- mkdir: allocates one block from free bitmap for child dir data.
        --   Same argument as write_at. INV-06 preserved.

        -- FRAME clauses: all operations only modify blocks belonging to
        --   the target inode(s). No operation assigns an already-allocated
        --   block to a different inode.

        -- POST_ERR: rollback mechanisms (write_failed_cleanup, free_inode)
        --   return allocated blocks to free bitmap. INV-06 preserved.

    CONCLUSION: INV-06 preserved by all HOARE specs. QED.
}

/// ---------------------------------------------------------------------------
/// INV-07: superblock_counter_consistency
/// ---------------------------------------------------------------------------
/// free_blocks = actual_free_blocks(FS), free_inodes = actual_free_inodes(FS).
/// NOTE: May be transiently stale; restored by sync_metadata.

INVARIANT_PROOF INV-07 {
    THREATENED_BY:
        -- create, mkdir, mknod: decrement free_inodes.
        -- write_at, write_link (slow), resize (grow): may consume free_blocks.
        -- resize (shrink): frees blocks, increments free_blocks.
        -- unlink, rmdir: free inodes (deferred until sync).
        -- set_xattr: may allocate xattr block.

    PROOF:
        -- create POST: "FS'.sb.free_inodes = FS.sb.free_inodes - 1".
        --   One inode allocated, counter decremented. Consistent.

        -- mkdir POST: "FS'.sb.free_inodes = FS.sb.free_inodes - 1",
        --   "FS'.sb.free_blocks <= FS.sb.free_blocks". Consistent.

        -- write_at POST: "FS'.sb.free_blocks <= FS.sb.free_blocks".
        --   Blocks allocated, counter adjusted. Consistent.

        -- resize (shrink) POST: "FS'.sb.free_blocks >= FS.sb.free_blocks".
        --   Blocks freed, counter adjusted. Consistent.

        -- sync_all POST: "FS'.sb.free_blocks = actual_free_blocks(FS')",
        --   "FS'.sb.free_inodes = actual_free_inodes(FS')".
        --   Counters recomputed from group descriptors. Exact consistency.

        -- fs_sync POST: "FS'.sb.free_blocks = actual_free_blocks(FS')",
        --   "FS'.sb.free_inodes = actual_free_inodes(FS')".
        --   Full filesystem sync recomputes all counters. Exact consistency.

        -- NOTE: Between syncs, counters may be transiently stale.
        --   This is acceptable per INV-07 NOTE. fsck recomputes on recovery.

    CONCLUSION: INV-07 preserved (eventually consistent via sync). QED.
}

/// ---------------------------------------------------------------------------
/// INV-08: fast_symlink_size_bound
/// ---------------------------------------------------------------------------
/// Fast symlinks have size <= MAX_FAST_SYMLINK_LEN (60 bytes).

INVARIANT_PROOF INV-08 {
    THREATENED_BY:
        -- write_link: writes symlink target, may change size.
        -- resize: changes inode size (but PRE excludes fast symlinks with size>0).

    PROOF:
        -- write_link (fast path, |target|+1 <= MAX_FAST_SYMLINK_LEN):
        --   POST: "FS'.inodes[ino].size = |target|"
        --   Since |target| + 1 <= MAX_FAST_SYMLINK_LEN = 60:
        --     |target| <= 59 < 60 = MAX_FAST_SYMLINK_LEN.
        --   INV-08 holds.

        -- write_link (slow path, |target|+1 > MAX_FAST_SYMLINK_LEN):
        --   The inode is no longer a fast symlink after this write
        --   (blocks > 0, data stored in page cache). is_fast_symlink
        --   returns false. INV-08 quantifier excludes this inode.

        -- resize PRE: "NOT (is_fast_symlink(FS, I) AND FS.inodes[I].size > 0)".
        --   If inode is a fast symlink with size > 0, resize is rejected.
        --   If inode is a fast symlink with size = 0, resize to any size
        --   would make it a slow symlink (blocks allocated). INV-08 holds.

        -- All other operations: FRAME clauses preserve size for non-target
        --   inodes. For target inodes, only write_at and resize modify size,
        --   and write_at PRE requires type_ != Dir (symlinks are valid targets
        --   but write_at on a symlink would make it a slow symlink if size
        --   exceeds MAX_FAST_SYMLINK_LEN).

    CONCLUSION: INV-08 preserved by all HOARE specs. QED.
}

/// ---------------------------------------------------------------------------
/// INV-09: ctime_monotonicity
/// ---------------------------------------------------------------------------
/// Mutations advance ctime: FS'.inodes[ino].ctime >= FS.inodes[ino].ctime.
/// EXCEPTION: explicit set_ctime from VFS layer may set arbitrary value.

INVARIANT_PROOF INV-09 {
    THREATENED_BY:
        -- set_mode, set_owner, set_group: set ctime = now().
        -- set_ctime: sets ctime to arbitrary VFS-provided value (EXCEPTION).
        -- write_at: sets ctime = now().
        -- resize: sets ctime = now() (if size changed).
        -- create, mkdir, link, unlink, rmdir, rename: set ctime = now()
        --   on affected inodes.
        -- set_xattr: sets ctime >= old ctime.
        -- write_link: modifies inode but ctime update is implicit.

    PROOF:
        -- set_mode POST: "FS'.inodes[I].ctime >= FS.inodes[I].ctime".
        --   Explicit monotonicity guarantee. INV-09 holds.

        -- set_owner POST: "FS'.inodes[I].ctime >= FS.inodes[I].ctime".
        --   Same guarantee. INV-09 holds.

        -- set_group POST: "FS'.inodes[I].ctime >= FS.inodes[I].ctime".
        --   Same guarantee. INV-09 holds.

        -- set_ctime POST: "FS'.inodes[I].ctime = new_time".
        --   new_time may be arbitrary. This is the documented EXCEPTION.
        --   INV-09 NOTE: "explicit set_ctime from VFS layer may set
        --   arbitrary value."

        -- write_at POST: "FS'.inodes[ino].ctime = now()".
        --   now() >= old ctime (monotonic clock). INV-09 holds.

        -- resize POST: "FS'.inodes[I].ctime >= FS.inodes[I].ctime"
        --   (only if size changed). INV-09 holds.

        -- create/mkdir POST: new inode ctime = now(). Parent ctime = now().
        --   Both >= old values. INV-09 holds.

        -- link POST: "FS'.inodes[child_ino].ctime = now()".
        --   Parent ctime = now(). INV-09 holds.

        -- unlink/rmdir POST: child ctime = now(), parent ctime = now().
        --   INV-09 holds.

        -- rename POST: "FS'.inodes[moved_ino].ctime = now()".
        --   Parent ctimes = now(). INV-09 holds.

        -- set_xattr POST: "FS'.inodes[ino].ctime >= FS.inodes[ino].ctime".
        --   Explicit monotonicity. INV-09 holds.

    CONCLUSION: INV-09 preserved by all HOARE specs (with documented
        set_ctime exception). QED.
}

/// ---------------------------------------------------------------------------
/// INV-10: dir_links_ge_2
/// ---------------------------------------------------------------------------
/// Every alive directory has links_count >= 2.

INVARIANT_PROOF INV-10 {
    THREATENED_BY:
        -- mkdir: creates directory with links_count = 2.
        -- rmdir: sets child links_count = 0 (but also sets alive = false).
        -- rename: may decrement parent links_count for dir moves.
        -- unlink: decrements links_count (but only for non-directories).

    PROOF:
        -- mkdir POST:
        --   "FS'.inodes[new_ino].links_count = 2" (new dir: "." + parent entry).
        --   2 >= 2. INV-10 holds for new_ino.
        --   Parent: links_count += 1 (child's ".."). Since parent was alive
        --   dir with links >= 2, after +1 it has links >= 3. INV-10 holds.

        -- rmdir POST:
        --   "FS'.inodes[child_ino].links_count = 0"
        --   "FS'.inodes[child_ino].alive = false"
        --   INV-10 quantifies over alive directories only.
        --   Dead child_ino excluded. INV-10 holds.
        --   Parent: links_count -= 1. Parent had links >= 2 + (at least
        --   one child's ".."), so links >= 3 before rmdir. After -1,
        --   links >= 2. INV-10 holds.

        -- rename (cross-dir, dir move, no replacement):
        --   src_dir.links_count -= 1 (lost child's "..").
        --   dst_dir.links_count += 1 (gained child's "..").
        --   src_dir had links >= 2 + (at least this child's "..") = 3.
        --   After -1: links >= 2. INV-10 holds.
        --   dst_dir: links += 1 >= 3. INV-10 holds.

        -- rename (cross-dir, dir move, with replacement):
        --   src_dir.links_count -= 1.
        --   dst_dir.links_count unchanged (replaced dir's ".." was already
        --   counted; new dir's ".." takes its place).
        --   src_dir: same argument as above. INV-10 holds.

        -- rename (same-dir, dir move, no replacement):
        --   Net link count change = 0 (+1 then -1). INV-10 holds.

        -- rename (same-dir, dir move, with replacement):
        --   links_count -= 1 (lost replaced dir's "..").
        --   Parent had links >= 2 + (replaced child's "..") = 3.
        --   After -1: links >= 2. INV-10 holds.

        -- unlink PRE: "child_type != Dir".
        --   unlink never targets directories. Directory link counts
        --   are not decremented by unlink. INV-10 holds.

    CONCLUSION: INV-10 preserved by all HOARE specs. QED.
}
