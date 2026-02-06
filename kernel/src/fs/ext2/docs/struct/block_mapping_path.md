# Phase 1 - Supplementary Struct Design (`inode.rs`)

Target implementation file: `kernel/src/fs/ext2/inode.rs`

## 1) Structure Definition

```rust
/// Logical-to-physical mapping path for direct/indirect pointer traversal.
///
/// # Linux Reference
/// - Source: `fs/ext2/inode.c:163-203`
/// - Corresponds to: `ext2_block_to_path` output (`offsets[4]`, `depth`, `boundary`)
#[derive(Clone, Copy, Debug)]
pub struct BlockPath {
    /// Number of levels in path (1..=4).
    pub depth: usize,
    /// Offset indices for each level.
    pub offsets: [u32; 4],
    /// Boundary hint for contiguous mapping.
    pub boundary: u32,
}

/// Chain element for traversed branch level.
///
/// # Linux Reference
/// - Source: `fs/ext2/inode.c:205-230`
/// - Corresponds to: internal `Indirect { key, p, bh }` chain semantics
#[derive(Clone, Copy, Debug)]
pub struct IndirectStep {
    /// Block pointer value observed at this level.
    pub key: u32,
    /// Pointer-slot index in parent node.
    pub slot: u32,
    /// Block id for intermediate node (none at inode root level).
    pub node_bid: Option<Bid>,
}

/// Complete traversal state for get/alloc/truncate coordination.
/// Linux intent refs: `ext2_get_blocks` / `__ext2_truncate_blocks`.
/// Linux: `fs/ext2/inode.c:624-780`, `fs/ext2/inode.c:1172-1260`
#[derive(Debug)]
pub struct MappingTraversal {
    /// Parsed target path.
    pub path: BlockPath,
    /// Steps successfully walked.
    pub chain: [Option<IndirectStep>; 4],
    /// Last valid chain index.
    pub last_filled: usize,
    /// Traversal status.
    pub status: TraversalStatus,
}

/// Traversal outcome states.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraversalStatus {
    /// Full mapping found.
    Complete,
    /// Missing pointer (hole / not allocated).
    Missing,
    /// Chain changed concurrently; caller should retry.
    Retry,
    /// I/O failure while reading indirect block.
    IoError,
}

/// Allocation splice plan for create path in `get_block(create=true)`.
/// Linux intent: `ext2_alloc_branch` + `ext2_splice_branch`.
/// Linux: `fs/ext2/inode.c:734-766`
#[derive(Debug)]
pub struct BranchAllocationPlan {
    /// Number of new indirect blocks needed.
    pub indirect_levels: usize,
    /// Number of direct data blocks to allocate.
    pub data_blocks: u32,
    /// Goal physical block for allocator locality.
    pub goal: u64,
}
```

## 2) Method Signatures (No Implementation Yet)

```rust
impl InodeInner {
    /// Parse logical block index into pointer-tree path.
    /// Linux equivalent: `ext2_block_to_path`.
    /// Linux: `fs/ext2/inode.c:163-203`
    pub fn block_to_path(&self, iblock: u32) -> Result<BlockPath>;

    /// Walk existing mapping branch without allocation.
    /// Linux intent equivalent: `ext2_get_branch` read side.
    /// Linux: `fs/ext2/inode.c:205-272`
    pub fn walk_mapping(&self, path: &BlockPath) -> Result<MappingTraversal>;

    /// Verify chain consistency to detect concurrent truncate/allocation races.
    /// Linux equivalent: `verify_chain`.
    /// Linux: `fs/ext2/inode.c:126-131`
    pub fn verify_chain(&self, traversal: &MappingTraversal) -> bool;

    /// Build allocation plan for missing branch.
    /// Linux intent equivalent: `ext2_blks_to_allocate` + goal selection.
    /// Linux: `fs/ext2/inode.c:721-731`
    pub fn plan_branch_allocation(
        &self,
        traversal: &MappingTraversal,
        max_blocks: u32,
    ) -> Result<BranchAllocationPlan>;
}

impl Inode {
    /// Map logical block; optionally allocate if missing.
    /// Linux equivalent: `ext2_get_block` / `ext2_get_blocks`.
    /// Linux: `fs/ext2/inode.c:624-780`, `fs/ext2/inode.c:783-794`
    pub fn get_block_mapped(&self, iblock: u32, create: bool) -> Result<Option<Bid>>;

    /// Truncate pointer tree from offset.
    /// Linux equivalent: `__ext2_truncate_blocks`.
    /// Linux: `fs/ext2/inode.c:1172-1260`
    pub fn truncate_mapping(&self, offset: u64) -> Result<()>;
}
```

## 3) Concurrency Rules

- Read-only walk may use shared inode lock.
- Any allocation splice or truncate operation must serialize through truncate mutex equivalent.
- Race on chain verification leads to retry state, not silent success.
- Lock hierarchy remains: inode lock -> truncate mutex -> block allocator/group locks.

## 4) Design Rationale

### Why explicit traversal struct?
- Makes race/retry and partial-chain conditions explicit and reviewable, mirroring Linux `-EAGAIN` branch logic.

### Why keep allocation planning as separate struct?
- Avoids intertwining pointer-tree traversal with allocator policy decisions; easier to verify against Linux flow.
