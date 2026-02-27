# Protocol State Machine Verification Methodology for Ext2

## Background & Motivation

You are verifying an Ext2 filesystem implementation in Rust (Asterinas kernel). The codebase
has an existing set of VFS contract specs using an actor-model approach (REQUIRE/ENSURE/FRAME),
which successfully found real bugs but has fundamental limitations:

1. **Actor model assumes no shared state** — but filesystems have shared inode cache, page cache,
   block bitmaps
2. **Actor messages are atomic** — but real VFS operations are multi-step with intermediate states
   visible to concurrent threads
3. **No crash semantics** — `persist_inode_and_sync()` is a real commit point, but the actor
   model treats the whole method as atomic
4. **Two-layer state conflated** — in-memory (page cache, inode cache) vs on-disk (block device)
   are distinct but the actor model merges them into one S/S'

We replace the actor model with a **Protocol State Machine** — inspired by distributed protocol
verification (2PC, Paxos phase decomposition), but applied directly as code-level annotations
with no external model to maintain.

## Core Concept

Each VFS method is a **multi-step protocol** with:
- Explicit **lock acquire/release** points
- Explicit **persist (commit) points** where on-disk state changes
- **Crash semantics** between every pair of persist points
- **Visible state** at each step (what concurrent threads can observe)
- **Rollback protocol** on failure at each step

---

## Part 1: State Model (Replaces Actor State Model)

### Dual-Layer State

The filesystem has two distinct state layers that must be tracked separately:

```
MemoryState (M) = {
    inode_cache   : Map<u32, Arc<Inode>>,
    page_cache    : Map<(ino, page_idx), Page>,   -- dirty flag per page
    dirty_inodes  : Set<u32>,                      -- inodes with uncommitted desc changes
    lock_state    : Map<u32, LockMode>,            -- per-inode lock status: Free|Read(n)|UpRead|Write
    xattr_cache   : Map<u32, XattrBlock>,          -- cached xattr blocks
}

DiskState (D) = {
    inode_table   : Map<u32, RawInodeDesc>,        -- on-disk inode descriptors
    data_blocks   : Map<BlockId, [u8; BLOCK_SIZE]>,
    dir_blocks    : Map<(ino, block_idx), DirBlock>,
    block_bitmap  : BitVec,
    inode_bitmap  : BitVec,
    group_descs   : Vec<GroupDesc>,
    super_block   : SuperBlock,
}
```

Key distinction:
- `M` changes on every in-memory mutation (inside a lock)
- `D` changes ONLY at `persist_inode_and_sync()` and page cache flush points
- A crash loses all of `M` and preserves `D` (possibly with partial writes)

### Consistency Relation

At any point, the system must satisfy:

```
CONSISTENT(M, D) ≡
    ∀ ino ∈ M.inode_cache where ino ∉ M.dirty_inodes:
        M.inode_cache[ino].desc = D.inode_table[ino]
    ∧ ∀ (ino, pg) ∈ M.page_cache where ¬pg.dirty:
        M.page_cache[(ino, pg)] = D.data_blocks[mapping(ino, pg)]
```

Dirty state is allowed — it just means M has diverged from D and a persist is pending.

---

## Part 2: Protocol Spec Format

Each VFS method is decomposed into a sequence of **steps**. A step is the smallest unit
of state change that matters for verification.

### Step Types

There are exactly 5 types of steps:

| Type | Meaning | Example |
|------|---------|---------|
| `LOCK` | Acquire or release a lock | `self.inner.write()` |
| `GUARD` | Check a precondition, fail-fast | `type_ == Dir`, `find_entry == ENOENT` |
| `EFFECT` | Mutate in-memory state (M changes, D unchanged) | `desc.mtime = now()` |
| `PERSIST` | Flush to disk (D changes to match M) | `persist_inode_and_sync()` |
| `UNLOCK` | Release lock (implicit or explicit) | `drop(guard)`, scope exit |

### Protocol Template

```
PROTOCOL <method_name>(<args>)
SOURCE: <file>:<line_start>-<line_end>
LINUX_REF: <linux_source_file>:<function_name>

STEP <n>: <short_description>
  TYPE: LOCK | GUARD | EFFECT | PERSIST | UNLOCK
  CODE: <file>:<line_range>
  <type-specific fields, see below>
  VISIBLE_STATE: { what concurrent observers can see after this step }

ON_FAILURE(step_n, errno):
  <rollback actions per step>

CRASH_ANALYSIS:
  <what happens if system crashes between each pair of PERSIST steps>

INVARIANTS_PRESERVED: [INV-x, INV-y, ...]
LOCK_FOOTPRINT: [<lock_name>(<mode>), ...]
LOCK_ORDER_CHECK: <ascending ino order proof>
```

### Type-Specific Fields

**For LOCK steps:**
```
LOCK: <object>.<field>.<mode>()     -- e.g., self.inner.write()
HOLDER: <variable_name>             -- e.g., mut inner
SCOPE: until STEP <m>               -- when does this lock release?
```

**For GUARD steps:**
```
CONDITION: <boolean expression>
ON_FALSE: Err(<errno>)
STATE_UNCHANGED: true               -- guards never mutate state
```

**For EFFECT steps:**
```
MEMORY_DELTA: {                     -- what changes in M
    <field> : <old> → <new>
}
DISK_DELTA: none                    -- EFFECTs never touch disk
REQUIRES_LOCK: <lock_name>(<mode>) -- which lock protects this mutation
```

**For PERSIST steps:**
```
CALL: <function_name>()             -- e.g., persist_inode_and_sync(&fs)
DISK_DELTA: {                       -- what changes in D
    D.inode_table[ino] = M.inode_cache[ino].desc
}
COMMIT_POINT: true | false          -- is this a logical commit point?
CRASH_BEFORE: <state description>   -- if crash before this persist completes
CRASH_AFTER: <state description>    -- if crash after this persist completes
```

---

## Part 3: Crash Analysis Framework

### Crash Model

A crash can occur at any point. The effect is:

```
CRASH(M, D, write_buffer) →
    D' = apply(any_prefix(write_buffer), D)
    M' = ∅  (all volatile state lost)
    Recovery: mount(D') → new M''
```

For ext2 (no journal), the crash model is simple:
- Writes within a single `persist_inode_and_sync()` may be partially applied
- Page cache flushes may be partially applied
- After recovery (fsck), the filesystem must be in a **valid** state

### Crash Windows

Between every pair of PERSIST steps in a protocol, there is a **crash window**.
For each window, you must specify:

```
CRASH_WINDOW(STEP_i.PERSIST → STEP_j.PERSIST):
    M_lost: { list of in-memory mutations that are lost }
    D_committed: { list of disk mutations already committed }
    D_uncommitted: { list of disk mutations not yet committed }
    RECOVERY_STATE: <description of state after remount>
    VALID: true | false  -- does recovery state satisfy all global invariants?
    FSCK_NEEDED: true | false  -- does fsck need to fix anything?
    FSCK_ACTION: <what fsck would do, e.g., "reclaim orphan inode">
```

### Crash Severity Classification

```
SAFE:     Recovery state satisfies all invariants. No data loss beyond the
          interrupted operation.
BENIGN:   Recovery state has minor inconsistency fixable by fsck without
          data loss. Example: orphan inode (allocated but unreferenced).
LEAKY:    Resources leaked (blocks/inodes allocated but unreferenced).
          fsck can reclaim. No corruption.
CORRUPT:  Structural inconsistency that could cause wrong behavior.
          Example: dir entry points to unallocated inode.
          This is a BUG.
```

---

## Part 4: Concurrency Analysis Framework

### Lock State Tracking

At each step of a protocol, track the complete lock state:

```
LOCK_STATE(step_n) = {
    <ino_or_resource> : <Free | Read(count) | UpRead | Write>,
    ...
}
```

### Interleaving Analysis

Between any two steps where a lock is NOT held (or is held in Read mode),
another thread can execute. For each such gap:

```
INTERLEAVE_WINDOW(STEP_i → STEP_j):
    LOCKS_HELD: { list of locks still held }
    LOCKS_FREE: { list of locks released or never acquired }
    CONCURRENT_OPS: { which operations could run in this window }
    OBSERVABLE_BY_OTHERS: { what state changes are visible }
    RISK: <description of potential race condition, or "none">
```

### Lock Order Verification

For every protocol that acquires multiple locks:

```
LOCK_ORDER_PROOF:
    Acquired: [lock_1, lock_2, ...]
    Order: lock_1.ino < lock_2.ino  (or lock_1.level < lock_2.level)
    Rule: ascending inode number for same-level locks
    Verified: true | false
```

Global lock level ordering (must never be violated):

```
Level 0: Inode.inner (RwMutex)        -- per-inode data
Level 1: Inode.xattr (RwMutex)        -- per-inode xattr
Level 2: Ext2.super_block (RwMutex)   -- global superblock
Level 3: BlockGroup locks             -- per-group bitmap/descriptor

RULE: Never acquire Level N while holding Level N+k (k>0)
RULE: Within same level, acquire by ascending inode/group number
```

---

## Part 5: Rollback Analysis Framework

### Failure Model

Each step can fail. On failure, all effects from previous steps in the
current protocol must be undone (or proven harmless).

```
ROLLBACK_ANALYSIS:
    STEP <n> fails with <errno>:
        UNDO_REQUIRED: { list of effects from steps 1..n-1 that must be reversed }
        UNDO_PERFORMED: { list of effects actually reversed by error handling code }
        UNDO_MISSING: { UNDO_REQUIRED - UNDO_PERFORMED }
        LEAK_IF_MISSING: <what leaks if undo is incomplete>
        SEVERITY: harmless | leak | inconsistency | corruption
```

---

## Part 6: Verification Checklist (Per Protocol)

When verifying a protocol spec against the Rust code, execute these checks
in order. Each check is a concrete yes/no question.

### Check 1: Step-Code Alignment

```
For each STEP in the protocol:
  □ Does the step correspond to identifiable lines in the Rust code?
  □ Is the step ordering in the spec identical to the code execution order?
  □ Are there code paths (branches, early returns) not captured by any step?
  □ Are there steps in the spec that don't correspond to any code?
```

### Check 2: Lock Correctness

```
For each LOCK step:
  □ Does the code acquire exactly the lock specified?
  □ Is the lock mode correct (read vs upread vs write)?
  □ Does the lock scope in code match the SCOPE field in spec?
  □ Is the lock released at the specified UNLOCK step (or scope end)?

For the protocol as a whole:
  □ Does the lock acquisition order respect the global level ordering?
  □ For same-level locks, is ascending inode/group number order respected?
  □ Are there any lock upgrades (read→write)? If so, are they safe?
  □ Is there any window where a lock is released and re-acquired?
      If so, is the state still valid after re-acquisition?
```

### Check 3: Effect Correctness

```
For each EFFECT step:
  □ Is the mutation protected by the lock specified in REQUIRES_LOCK?
  □ Does the MEMORY_DELTA accurately describe all fields changed?
  □ Are there side effects not captured in MEMORY_DELTA?
  □ Does the effect match the corresponding Linux ext2 behavior?
```

### Check 4: Persist Correctness

```
For each PERSIST step:
  □ Does the code call the persist function specified?
  □ Does DISK_DELTA accurately describe what is flushed to disk?
  □ Is the persist call inside the correct lock scope?
  □ If persist fails, is the error propagated correctly?
```

### Check 5: Crash Safety

```
For each CRASH_WINDOW between consecutive PERSIST steps:
  □ Is the recovery state described accurately?
  □ Does the recovery state satisfy all global invariants?
  □ If not, what is the crash severity (SAFE/BENIGN/LEAKY/CORRUPT)?
  □ Is fsck capable of fixing the inconsistency?
  □ Is this consistent with Linux ext2 crash behavior?
```

### Check 6: Rollback Completeness

```
For each step that can fail:
  □ Are all prior effects properly undone?
  □ Are allocated resources (inodes, blocks) freed on failure?
  □ Is the observable state after failure identical to pre-call state?
  □ If rollback is incomplete, what is the severity?
```

### Check 7: Invariant Preservation

```
For each global invariant (INV-1 through INV-10):
  □ Does the protocol preserve this invariant across all steps?
  □ Is the invariant temporarily violated mid-protocol?
      If so, is it restored before any UNLOCK step?
  □ Could a concurrent observer see the temporary violation?
```

---

## Part 7: Complete Example — `create`

This example shows how to apply the methodology to a real VFS method.

```
PROTOCOL create(name, type_, mode)
SOURCE: kernel/src/fs/ext2/impl_for_vfs/inode.rs + kernel/src/fs/ext2/inode.rs
LINUX_REF: fs/ext2/namei.c:ext2_create, fs/ext2/namei.c:ext2_mkdir

### Steps

```
STEP 1: acquire_parent_lock
  TYPE: LOCK
  CODE: impl_for_vfs/inode.rs (create dispatches to inode.rs add_entry)
  LOCK: self.inner.upread() → upgrade to write() as needed
  HOLDER: inner / write_inner
  SCOPE: until STEP 7 (implicit drop)

STEP 2: validate_preconditions
  TYPE: GUARD
  CODE: inode.rs:833-845
  CONDITION: self.type_ == Dir ∧ name.len() ∈ [1,255] ∧ name ∉ {".",".."}
  ON_FALSE: Err(ENOTDIR) | Err(EINVAL)
  STATE_UNCHANGED: true

STEP 3: check_no_duplicate
  TYPE: GUARD
  CODE: inode.rs:850 (scan_dir_for_slot returns existing entry check)
  CONDITION: find_entry(name) == Err(ENOENT)
  ON_FALSE: Err(EEXIST)
  STATE_UNCHANGED: true
```

```
STEP 4: alloc_inode
  TYPE: EFFECT
  CODE: impl_for_vfs/inode.rs (fs.alloc_inode call)
  MEMORY_DELTA: {
      fs.inode_bitmap[child_ino]: 0 → 1
      fs.free_inodes_count: n → n-1
      fs.bg[g].free_inodes_count: m → m-1
      inode_cache += { child_ino → new Inode }
  }
  DISK_DELTA: none (bitmap change is in-memory until persist)
  REQUIRES_LOCK: fs.bg[g].lock (acquired internally by alloc_inode)
  ROLLBACK_IF_FAIL: nothing allocated yet, return Err(ENOSPC)
  VISIBLE_STATE: { child_ino allocated in bitmap, not yet linked anywhere }
```

```
STEP 5: init_child_and_persist
  TYPE: PERSIST
  CODE: impl_for_vfs/inode.rs (init child desc + persist)
  MEMORY_DELTA: {
      child.desc = { mode, uid, gid=parent.gid, size, times=now(), links_count }
      if Dir: child.dir = { "."→child, ".."→parent }
  }
  CALL: child.persist_inode_and_sync(&fs)
  DISK_DELTA: {
      D.inode_table[child_ino] = child.desc
      D.inode_bitmap[child_ino] = 1
      if Dir: D.dir_blocks[child] = {".", ".."}
  }
  COMMIT_POINT: false (child exists on disk but not linked to parent)
  CRASH_BEFORE: child inode partially written → fsck: orphan inode, BENIGN
  CRASH_AFTER: child inode on disk, not linked → fsck: orphan inode, BENIGN
```

```
STEP 6: link_to_parent_and_persist
  TYPE: PERSIST
  CODE: inode.rs:861-865 (write_dir_entry + commit_dir_metadata)
  MEMORY_DELTA: {
      parent.entries += { name → (child_ino, ft) }
      parent.desc.mtime = now()
      parent.desc.ctime = now()
      if Dir: parent.desc.links_count += 1
  }
  CALL: parent.persist_inode_and_sync(&fs)
  DISK_DELTA: {
      D.dir_blocks[parent] contains new entry(name, child_ino)
      D.inode_table[parent_ino].mtime = now()
      D.inode_table[parent_ino].ctime = now()
      if Dir: D.inode_table[parent_ino].links_count += 1
  }
  COMMIT_POINT: true (after this, create is logically complete)
  CRASH_BEFORE: child on disk but not in parent dir → BENIGN (orphan)
  CRASH_AFTER: fully consistent, create is durable → SAFE
```

```
STEP 7: release_parent_lock
  TYPE: UNLOCK
  CODE: implicit drop of write_inner at scope exit
  LOCK: self.inner
  POST: create-lookup roundtrip holds (COMP-1)
```

### Crash Analysis

```
CRASH_WINDOW(before any PERSIST):
    D_committed: nothing
    RECOVERY: as if create never happened
    SEVERITY: SAFE

CRASH_WINDOW(STEP 5 → STEP 6):
    D_committed: child inode + bitmap on disk
    D_uncommitted: parent dir entry, parent metadata
    RECOVERY: child inode exists on disk, no dir entry points to it
    SEVERITY: BENIGN
    FSCK_ACTION: reclaim orphan inode (inode with links_count but no dir ref)

CRASH_WINDOW(after STEP 6):
    D_committed: everything
    RECOVERY: fully consistent
    SEVERITY: SAFE
```

### Rollback Analysis

```
ON_FAILURE(STEP 2, ENOTDIR/EINVAL):
    UNDO_REQUIRED: none (no effects yet)
    SEVERITY: SAFE

ON_FAILURE(STEP 3, EEXIST):
    UNDO_REQUIRED: none (no effects yet)
    SEVERITY: SAFE

ON_FAILURE(STEP 4, ENOSPC):
    UNDO_REQUIRED: none (alloc_inode is atomic, it either succeeds or doesn't)
    SEVERITY: SAFE

ON_FAILURE(STEP 5, EIO):
    UNDO_REQUIRED: { free child_ino from inode_bitmap }
    UNDO_PERFORMED: { alloc_inode cleanup frees the inode }
    SEVERITY: check — is the bitmap actually rolled back?

ON_FAILURE(STEP 6, EIO):
    UNDO_REQUIRED: { free child_ino, undo child persist }
    UNDO_PERFORMED: { ??? }
    SEVERITY: LEAKY — child inode persisted but never linked
    NOTE: this matches Linux ext2 behavior (no journal)
```

### Lock Analysis

```
LOCK_FOOTPRINT:
    self.inner(UpRead → Write)   -- parent inode, upgraded as needed
    fs.bg[g].lock(Write)         -- acquired internally by alloc_inode, released before return
    child.inner(Write)           -- acquired internally during init, released before return

LOCK_ORDER_CHECK:
    Only one user-visible lock held at a time (self.inner).
    Internal locks (bg, child) are acquired and released within sub-calls.
    No ordering violation possible.

INTERLEAVE_WINDOW: none
    Parent lock held throughout STEP 1-7.
    No concurrent mutation of parent dir is possible.
```

---

## Part 8: Method Classification

Not all VFS methods need the full protocol treatment. Classify each method
by complexity to determine the appropriate level of analysis.

### Tier 1: Simple (single lock, no persist, no sub-calls)

Methods: `size`, `type_`, `ino`, `mode`, `owner`, `group`, `atime`, `mtime`,
`ctime`, `page_cache`, `fs`, `extension`

Spec format: Single LOCK → GUARD → read field → UNLOCK. No crash/rollback
analysis needed. Use a simplified one-line format:

```
SIMPLE_READ <method>:
  LOCK: self.inner.read()
  RETURN: self.inner.desc.<field>
  INVARIANT: S' = S
```

### Tier 2: Mutator (single lock, one persist, no sub-inode calls)

Methods: `set_mode`, `set_owner`, `set_group`, `set_atime`, `set_mtime`,
`set_ctime`, `resize`

Spec format: LOCK → GUARD → EFFECT → PERSIST → UNLOCK. Crash analysis
is trivial (atomic single-inode persist). Rollback is trivial (fail before
persist = no change).

```
MUTATOR <method>(<args>):
  LOCK: self.inner.write()
  GUARD: <preconditions>
  EFFECT: self.inner.desc.<field> = <new_value>
  PERSIST: self.inner.persist_inode_and_sync(&fs)
  CRASH: atomic single-inode write, SAFE
```

### Tier 3: Multi-Step (multiple persists, single inode scope)

Methods: `write_at`, `write_link`, `sync_all`, `sync_data`, `fallocate`,
`set_xattr`, `remove_xattr`

Spec format: Full protocol with crash windows between persist points.
Rollback analysis required. Concurrency analysis usually simple (single
inode lock held throughout).

### Tier 4: Multi-Inode (multiple locks, multiple persists, cross-inode)

Methods: `create`, `mknod`, `link`, `unlink`, `rmdir`, `rename`

Spec format: Full protocol with all analyses. These are the most complex
and most likely to contain bugs. `rename` is the hardest (up to 4 inodes
involved, multiple lock orderings, multiple crash windows).

---

## Part 9: Execution Workflow

### Phase 1: Write Protocol Specs

For each VFS method, in tier order (Tier 1 first, Tier 4 last):

1. Read the Rust implementation code thoroughly
2. Read the corresponding Linux ext2 source as reference
3. Identify all lock acquire/release points
4. Identify all `persist_inode_and_sync()` calls — these are your PERSIST steps
5. Decompose the code between lock/persist boundaries into STEP entries
6. Fill in all type-specific fields for each step

### Phase 2: Verify Protocol Specs Against Code

For each protocol spec, run the 7-check verification checklist (Part 6):

1. Step-Code Alignment
2. Lock Correctness
3. Effect Correctness
4. Persist Correctness
5. Crash Safety
6. Rollback Completeness
7. Invariant Preservation

Record findings as:
- **PASS**: code matches spec
- **BUG**: code violates spec (with severity and fix suggestion)
- **SPEC_INACCURACY**: spec doesn't match code but code is correct
- **CONCERN**: not a bug but worth investigating

### Phase 3: Cross-Protocol Analysis

After individual protocols are verified, analyze interactions:

1. **Deadlock freedom**: For every pair of Tier 4 protocols that could run
   concurrently, verify that their lock acquisition orders are compatible.
   Build a lock acquisition graph and check for cycles.

2. **Crash composition**: If protocol A crashes mid-way, can protocol B
   (running after recovery) observe an inconsistent state? Check that
   every crash window of every protocol produces a state that is a valid
   starting state for every other protocol.

3. **Invariant stability**: Verify that the set of global invariants is
   closed under all protocol transitions — i.e., if all invariants hold
   before a protocol runs, they hold after (assuming no crash).

---

## Part 10: Global Invariants (Carried Over)

The following invariants from the actor-model specs remain valid and must
be checked at every UNLOCK step of every protocol. They are unchanged.

```
INV-1:  Inode identity immutability (ino, type_ never change)
INV-2:  Root inode existence (ino=2, Dir, links≥2)
INV-3:  Directory dot-entries consistency ("." → self, ".." → parent)
INV-4:  Link count consistency (links_count = number of references)
INV-5:  Freed inode marking (links=0, dtime≠0)
INV-6:  Filesystem back-pointer validity
INV-7:  Block allocation exclusivity (no block shared by two inodes)
INV-8:  Superblock counter consistency (sum of per-group = global)
INV-9:  Timestamp monotonicity (ctime never decreases)
INV-10: Xattr block validity (magic, h_blocks=1, refcount=1)
```

Additionally, the protocol model introduces new **concurrency invariants**:

```
CINV-1: Lock ordering — no cycle in lock acquisition graph
CINV-2: No lock held across persist — persist_inode_and_sync() is called
         with the inode's own write lock held, never with another inode's lock
         at the same level (exception: rename with write_lock_two_inodes)
CINV-3: Dirty set bounded — every dirty inode is eventually persisted or
         rolled back before its lock is released
```

---

## Part 11: Composition Properties (Carried Over, Reframed)

The existing composition properties remain valid but are now expressed as
**multi-protocol traces** rather than abstract equations:

```
COMP-1: Create-Lookup roundtrip
  TRACE: create(dir, "foo", Reg, 0o644) → Ok(child)
         THEN lookup(dir, "foo") → Ok(found)
  CHECK: found.ino == child.ino

COMP-2: Link-Unlink inverse
  TRACE: link(dir, &inode, "bar") → Ok
         THEN unlink(dir, "bar") → Ok
  CHECK: inode.links_count == original
         lookup(dir, "bar") → Err(ENOENT)

COMP-3: Create-Rmdir inverse
  TRACE: create(dir, "sub", Dir, mode) → Ok(child)
         THEN rmdir(dir, "sub") → Ok
  CHECK: lookup(dir, "sub") → Err(ENOENT)
         child.is_freed == true

COMP-5: Write-Read roundtrip
  TRACE: write_at(inode, offset, data) → Ok(n)
         THEN read_at(inode, offset, n) → Ok(buf)
  CHECK: buf == data[..n]

COMP-9: Symlink write-read roundtrip
  TRACE: write_link(inode, target) → Ok
         THEN read_link(inode) → Ok(result)
  CHECK: result == target

COMP-10: Sync idempotence
  TRACE: sync_all(inode) → Ok
         THEN sync_all(inode) → Ok
  CHECK: second sync is a no-op (no disk writes)
```

---

## Part 12: Methods to Verify (Complete List)

### Tier 1 — Simple Reads (13 methods)

| Method | Lock | Field |
|--------|------|-------|
| `size()` | inner.read() | desc.size |
| `ino()` | none (immutable) | ino |
| `type_()` | none (immutable) | type_ |
| `mode()` | inner.read() | desc.mode |
| `owner()` | inner.read() | desc.uid |
| `group()` | inner.read() | desc.gid |
| `atime()` | inner.read() | desc.atime |
| `mtime()` | inner.read() | desc.mtime |
| `ctime()` | inner.read() | desc.ctime |
| `page_cache()` | none | page_cache ref |
| `fs()` | none | fs.upgrade() |
| `extension()` | none | extension ref |
| `open()` | none | no-op for ext2 |

### Tier 2 — Single-Inode Mutators (8 methods)

| Method | Lock | Persist |
|--------|------|---------|
| `set_mode(mode)` | inner.write() | persist_inode_and_sync |
| `set_owner(uid)` | inner.write() | persist_inode_and_sync |
| `set_group(gid)` | inner.write() | persist_inode_and_sync |
| `set_atime(time)` | inner.write() | persist_inode_and_sync |
| `set_mtime(time)` | inner.write() | persist_inode_and_sync |
| `set_ctime(time)` | inner.write() | persist_inode_and_sync |
| `resize(new_size)` | inner.write() | persist_inode_and_sync |
| `metadata()` | inner.read() | none (pure read, composite) |

### Tier 3 — Multi-Step Single-Inode (8 methods)

| Method | Complexity |
|--------|-----------|
| `read_at(offset, writer, flags)` | page cache read, possible block mapping |
| `write_at(offset, reader, flags)` | page cache write, block alloc, persist |
| `read_link()` | fast path (inline) vs slow path (page cache) |
| `write_link(target)` | fast/slow path, block alloc, persist, rollback |
| `sync_all()` | flush pages + persist inode + flush xattr |
| `sync_data()` | flush pages only |
| `fallocate(mode, offset, len)` | block alloc/dealloc, multiple modes |
| `set_xattr(name, value, flags)` | xattr block alloc, persist |

Also in Tier 3:

| Method | Complexity |
|--------|-----------|
| `get_xattr(name, writer)` | xattr block load (lazy), pure read |
| `list_xattr(namespace, writer)` | xattr block load (lazy), pure read |
| `remove_xattr(name)` | xattr block modify, possible free, persist |

### Tier 4 — Multi-Inode Protocols (6 methods)

| Method | Inodes Involved | Lock Pattern |
|--------|----------------|--------------|
| `create(name, type_, mode)` | parent + child (new) | parent.write, child.write (internal) |
| `mknod(name, mode, type_)` | parent + child (new) | parent.write, child.write (internal) |
| `link(old, name)` | parent + old | parent.write, old.write |
| `unlink(name)` | parent + child | parent.upread→write, child.write |
| `rmdir(name)` | parent + child | parent.upread→write, child.write |
| `rename(old, target, new)` | src_dir + dst_dir + moved + existing | write_lock_two_inodes + moved.write + existing.write |

### FileSystem Trait (5 methods)

| Method | Tier |
|--------|------|
| `name()` | Tier 1 (constant) |
| `root_inode()` | Tier 1 (cached) |
| `sb()` | Tier 1 (read) |
| `fs_event_subscriber_stats()` | Tier 1 (read) |
| `sync()` | Tier 3 (flush all dirty inodes + superblock) |

---

## Part 13: Output Format

### Per-Method Verification Report

Each method produces a report with this structure:

```
## PROTOCOL: <method_name>

### Steps
(full protocol decomposition as described in Part 2)

### Verification Results

| Check | Result | Details |
|-------|--------|---------|
| Step-Code Alignment | PASS/BUG/SPEC_INACCURACY | ... |
| Lock Correctness | PASS/BUG | ... |
| Effect Correctness | PASS/BUG | ... |
| Persist Correctness | PASS/BUG | ... |
| Crash Safety | PASS/CONCERN | ... |
| Rollback Completeness | PASS/BUG/CONCERN | ... |
| Invariant Preservation | PASS/BUG | ... |

### Bugs Found
(if any, with severity, location, fix suggestion)

### Crash Windows Summary
(table of all crash windows with severity)

### Concerns
(non-bugs worth noting)
```

### Summary Report (Cross-Protocol)

After all methods are verified, produce a summary:

```
## Cross-Protocol Verification Summary

### Deadlock Analysis
| Protocol A | Protocol B | Shared Locks | Ordering Compatible | Risk |
|------------|------------|-------------|--------------------|----- |

### Crash Composition Matrix
| Protocol | Crash Windows | Worst Severity | Needs fsck |
|----------|--------------|----------------|------------|

### Invariant Coverage
| Invariant | Verified By Protocols | Status |
|-----------|----------------------|--------|

### Bugs Found (All)
| ID | Protocol | Check | Severity | Description |
|----|----------|-------|----------|-------------|
```

---

## Part 14: Key Source Files Reference

When writing and verifying protocol specs, you need these files:

### Implementation (Rust)

```
kernel/src/fs/ext2/impl_for_vfs/inode.rs   — VFS trait impl, dispatches to inode.rs
kernel/src/fs/ext2/impl_for_vfs/fs.rs      — FileSystem trait impl
kernel/src/fs/ext2/inode.rs                — Core inode logic (all Tier 3/4 methods)
kernel/src/fs/ext2/xattr.rs               — Xattr implementation
kernel/src/fs/ext2/fs.rs                   — Ext2 filesystem struct, mount, alloc
kernel/src/fs/ext2/block_group.rs          — Block group management
kernel/src/fs/ext2/block_ptr.rs            — Block pointer / indirect block logic
kernel/src/fs/ext2/dir.rs                  — Directory entry parsing
kernel/src/fs/ext2/prelude.rs              — Shared types and imports
```

### VFS Trait Definitions

```
kernel/src/fs/utils/inode.rs               — Inode trait (all VFS methods)
kernel/src/fs/utils/fs.rs                  — FileSystem trait
```

### Linux Reference

```
/root/linux/fs/ext2/namei.c               — create, mkdir, link, unlink, rmdir, rename
/root/linux/fs/ext2/dir.c                 — add_link, delete_entry, readdir
/root/linux/fs/ext2/inode.c               — read_inode, write_inode, get_block
/root/linux/fs/ext2/file.c                — read, write, fsync
/root/linux/fs/ext2/xattr.c               — xattr get/set/list/delete
/root/linux/fs/ext2/balloc.c              — block allocation
/root/linux/fs/ext2/ialloc.c              — inode allocation
```

### Existing Specs (for reference, being replaced)

```
kernel/src/fs/ext2/spec/verify/vfs-contract-global.spec
kernel/src/fs/ext2/spec/verify/vfs-contract-metadata.spec
kernel/src/fs/ext2/spec/verify/vfs-contract-dir-ops.spec
kernel/src/fs/ext2/spec/verify/vfs-contract-io-sync-xattr.spec
kernel/src/fs/ext2/spec/verify/vfs-contract-filesystem.spec
```
```
```
