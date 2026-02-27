// SPDX-License-Identifier: MPL-2.0
//
// Layer 1: Crash Hoare Logic — Abstract State Model
//
// Pure mathematical model of an ext2 filesystem. No locks, no page cache,
// no Rust types. All Layer 1 specs reference only this model.
//
// Notation:
//   FS       — pre-state  (AbstractFS before operation)
//   FS'      — post-state (AbstractFS after operation)
//   FS.durable — last synced persistent state
//   {P} C {Q} ↓ {R} — Crash Hoare Logic quadruple:
//       P = precondition, C = command, Q = postcondition, R = crash condition
//
// Inspired by FSCQ (Chen et al., SOSP 2015).

/// =============================================================================
/// SECTION 1: ABSTRACT STATE MODEL
/// =============================================================================

AbstractFS = {
    inodes   : Map<Ino, InodeRecord>,
    dirs     : Map<Ino, Map<Name, (Ino, FileType)>>,
    data     : Map<Ino, ByteSeq>,
    xattrs   : Map<Ino, Map<XName, Bytes>>,
    sb       : SuperBlockRecord,
    durable  : AbstractFS,              -- snapshot at last sync
}

InodeRecord = {
    type_       : FileType,             -- immutable after creation
    mode        : u16,
    uid         : u32,
    gid         : u32,
    size        : u64,
    links_count : u16,
    blocks      : u32,
    atime       : Timestamp,
    mtime       : Timestamp,
    ctime       : Timestamp,
    dtime       : Timestamp,
    alive       : bool,                 -- ¬is_freed
    file_acl    : u32,                  -- xattr block (0 = none)
}

SuperBlockRecord = {
    total_blocks   : u32,
    free_blocks    : u32,
    total_inodes   : u32,
    free_inodes    : u32,
    block_size     : u32,
    blocks_per_group : u32,
    inodes_per_group : u32,
}

/// =============================================================================
/// SECTION 2: DERIVED FUNCTIONS
/// =============================================================================

-- File content splice (used by write_at)
splice(seq, offset, buf) =
    seq[0..offset] ++ buf ++ seq[offset+|buf|..]

-- Effective file size after splice
splice_size(old_size, offset, buf_len) =
    max(old_size, offset + buf_len)

-- Directory child count (excluding "." and "..")
child_count(FS, ino) =
    |{ name : (name, _) ∈ FS.dirs[ino] ∧ name ∉ {".", ".."} }|

-- Is directory empty (only "." and "..")?
is_empty_dir(FS, ino) =
    child_count(FS, ino) = 0

-- Is fast symlink (target stored inline in block pointers)?
is_fast_symlink(FS, ino) =
    FS.inodes[ino].type_ = SymLink
    ∧ |FS.data[ino]| ≤ MAX_FAST_SYMLINK_LEN

-- Blocks needed for byte range
blocks_for(byte_count, block_size) =
    ceil(byte_count / block_size)

/// =============================================================================
/// SECTION 3: CONSTANTS
/// =============================================================================

MAX_FAST_SYMLINK_LEN = 60       -- block_ptrs storage (15 * 4 bytes)
MAX_LINK_COUNT       = 32000    -- EXT2_LINK_MAX
ROOT_INO             = 2        -- root directory inode number
MAX_NAME_LEN         = 255      -- maximum filename length
XATTR_MAGIC          = 0xEA020000

/// =============================================================================
/// SECTION 4: STRUCTURAL INVARIANTS (Hoare Logic)
/// =============================================================================
///
/// These hold at every quiescent point (between VFS operations).
/// Written as universally quantified predicates over AbstractFS.

INVARIANT INV-01: inode_identity_immutable {
    FORMAL:
        ∀ op, ∀ ino ∈ dom(FS.inodes):
            FS'.inodes[ino].type_ = FS.inodes[ino].type_
    PROOF_OBLIGATION:
        Every HOARE spec must preserve type_ in its FRAME clause.
}

INVARIANT INV-02: root_inode_exists {
    FORMAL:
        ROOT_INO ∈ dom(FS.inodes)
        ∧ FS.inodes[ROOT_INO].type_ = Dir
        ∧ FS.inodes[ROOT_INO].alive
        ∧ FS.inodes[ROOT_INO].links_count ≥ 2
}

INVARIANT INV-03: dir_dot_entries {
    FORMAL:
        ∀ ino where FS.inodes[ino].type_ = Dir ∧ FS.inodes[ino].alive:
            FS.dirs[ino]["."]  = (ino, Dir)
            ∧ FS.dirs[ino][".."] = (parent_ino, Dir)
            ∧ parent_ino ∈ dom(FS.inodes)
}

INVARIANT INV-04: link_count_consistency {
    FORMAL:
        ∀ ino where FS.inodes[ino].alive:
            FS.inodes[ino].links_count =
                |{ (p, name) : FS.dirs[p][name].0 = ino ∧ name ≠ "." }|
                -- Note: "." self-link is counted in the parent's ".." entry
                -- For dirs: links = parent_refs + child_dotdot_refs + "." self
                -- Simplified: links_count = hard_link_references
}

INVARIANT INV-05: freed_inode_marking {
    FORMAL:
        ∀ ino where ¬FS.inodes[ino].alive:
            FS.inodes[ino].links_count = 0
            ∧ FS.inodes[ino].dtime > 0
}

INVARIANT INV-06: block_exclusivity {
    FORMAL:
        ∀ block b in allocated_blocks(FS):
            |{ ino : b ∈ blocks_of(FS, ino) }| ≤ 1
    NOTE: No block is referenced by more than one inode.
}

INVARIANT INV-07: superblock_counter_consistency {
    FORMAL:
        FS.sb.free_blocks = actual_free_blocks(FS)
        ∧ FS.sb.free_inodes = actual_free_inodes(FS)
    NOTE: Maintained by sync_metadata; may be transiently stale.
}

INVARIANT INV-08: fast_symlink_size_bound {
    FORMAL:
        ∀ ino where is_fast_symlink(FS, ino):
            FS.inodes[ino].size ≤ MAX_FAST_SYMLINK_LEN
}

INVARIANT INV-09: ctime_monotonicity {
    FORMAL:
        ∀ mutation op on inode ino:
            FS'.inodes[ino].ctime ≥ FS.inodes[ino].ctime
    EXCEPTION: explicit set_ctime from VFS layer may set arbitrary value.
}

INVARIANT INV-10: dir_links_ge_2 {
    FORMAL:
        ∀ ino where FS.inodes[ino].type_ = Dir ∧ FS.inodes[ino].alive:
            FS.inodes[ino].links_count ≥ 2
    NOTE: "." self-link + ".." from parent (or self for root).
}

/// =============================================================================
/// SECTION 5: CRASH SEMANTICS
/// =============================================================================
///
/// Ext2 has no journal. On crash, the filesystem recovers to FS.durable
/// (possibly repaired by fsck). The CRASH clause in each HOARE spec
/// describes the set of possible recovered states.

CRASH_MODEL {
    -- After crash, recovery produces a state FS_r such that:
    --   1. FS_r is "between" FS.durable and FS' (partial writes possible)
    --   2. fsck repairs structural inconsistencies:
    --      - Orphan inodes (allocated but unreferenced) are reclaimed
    --      - Link counts are recomputed from directory entries
    --      - Free counts are recomputed from bitmaps
    --      - Block/inode bitmaps are reconciled with actual usage
    --   3. Data content of partially written blocks is undefined

    FSCK_GUARANTEES {
        -- After fsck, all structural invariants hold:
        POST_FSCK(FS_r):
            INV-02 through INV-10 hold for FS_r
            -- INV-01 always holds (type_ is on-disk immutable)
    }

    -- Sync establishes a new durable checkpoint:
    SYNC_SEMANTICS {
        After successful sync_all on inode ino:
            FS'.durable.inodes[ino] = FS'.inodes[ino]
            FS'.durable.data[ino]   = FS'.data[ino]

        After successful FileSystem::sync:
            FS'.durable = FS'
    }
}

/// =============================================================================
/// SECTION 6: ERROR MODEL
/// =============================================================================

ERROR_MODEL {
    -- Failed operations preserve abstract state:
    ERROR_PRESERVATION:
        ∀ op, ∀ args:
            op(args) = Err(e) ⟹ FS' = FS

    -- Common error conditions (referenced by HOARE specs):
    ERR_FS_DEAD    = Err(Errno::EIO)       -- filesystem unmounted
    ERR_IO         = Err(Errno::EIO)       -- block device I/O failure
    ERR_NOSPC      = Err(Errno::ENOSPC)    -- no free blocks/inodes
    ERR_NOENT      = Err(Errno::ENOENT)    -- name not found
    ERR_EXIST      = Err(Errno::EEXIST)    -- name already exists
    ERR_NOTDIR     = Err(Errno::ENOTDIR)   -- expected directory
    ERR_ISDIR      = Err(Errno::EISDIR)    -- unexpected directory
    ERR_NOTEMPTY   = Err(Errno::ENOTEMPTY) -- directory not empty
    ERR_NAMETOOLONG= Err(Errno::ENAMETOOLONG) -- name > 255
    ERR_MLINKS     = Err(Errno::EMLINK)    -- too many links
    ERR_INVAL      = Err(Errno::EINVAL)    -- invalid argument
    ERR_NODATA     = Err(Errno::ENODATA)   -- xattr not found
    ERR_RANGE      = Err(Errno::ERANGE)    -- buffer too small
    ERR_NOSYS      = Err(Errno::EOPNOTSUPP)-- operation not supported
}

/// =============================================================================
/// SECTION 7: HOARE SPEC TEMPLATE
/// =============================================================================
///
/// Every Layer 1 spec follows this format:
///
///   HOARE method_name(args...) {
///       PRE:
///           -- predicates over FS (abstract pre-state)
///
///       POST (Ok(ret)):
///           -- predicates over FS' and ret
///           FRAME: fields NOT listed here are unchanged
///
///       POST_ERR (Err(e)):
///           FS' = FS
///           e ∈ { ... }
///
///       CRASH:
///           FS_recovered ∈ { ... }
///
///       LINUX_REF: path/to/linux/source
///   }
///
/// Rules:
///   1. PRE/POST/CRASH reference ONLY AbstractFS fields
///   2. No locks, no page cache, no Dirty<>, no Rust types
///   3. FRAME is implicit: anything not mentioned in POST is unchanged
///   4. CRASH describes the set of possible states after power loss
