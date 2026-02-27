# Ext2 VFS Contract Verification Report

Date: 2026-02-27
Scope: 45 method contracts + 10 invariants + 10 composition properties

## Executive Summary

| Spec File | Total | PASS | PARTIAL | FAIL |
|-----------|-------|------|---------|------|
| vfs-contract-filesystem.spec | 5 | 5 | 0 | 0 |
| vfs-contract-metadata.spec | 21 | 19 | 2 | 0 |
| vfs-contract-dir-ops.spec | 8 | 7 | 1 | 0 |
| vfs-contract-io-sync-xattr.spec | 11 | 6 | 5 | 0 |
| vfs-contract-global.spec (INV) | 10 | 7 | 3 | 0 |
| vfs-contract-global.spec (COMP) | 10 | 10 | 0 | 0 |
| **Total** | **65** | **54** | **11** | **0** |

---

## Real Bugs Found (需要修复)

### BUG-1: write_link 缺少 ctime/mtime 更新
- **Severity**: Medium
- **Contract**: MSG-IO-04
- **Location**: `inode.rs:468-565`
- **Description**: `write_link` 的 fast path 和 slow path 都没有设置 `desc.ctime = now()` 和 `desc.mtime = now()`。Spec 明确要求这两个时间戳更新。Linux ext2 也会更新。
- **Fix**: 在 persist 之前添加 ctime/mtime 赋值

### BUG-2: rmdir 缺少 child.ctime 更新
- **Severity**: Low
- **Contract**: MSG-DIR-07
- **Location**: `inode.rs:1033-1039`
- **Description**: `rmdir` 没有在 persist 前设置 `child.ctime = now()`。Linux ext2 的 `ext2_rmdir` 会调用 `inode_set_ctime_to_ts(inode, ...)`。
- **Fix**: 在 line 1035 前添加 `child_write.desc.ctime = now();`

### BUG-3: remove_xattr 缺少 ctime 更新
- **Severity**: Low
- **Contract**: MSG-XATTR-04
- **Location**: `inode.rs:401-418`
- **Description**: `remove_xattr` 没有设置 `desc.ctime = now()`，但 `set_xattr` (line 394) 会设置。行为不一致，且不符合 Unix 语义。
- **Fix**: 在 persist 前添加 `inner.desc.ctime = now();`

---

## Spec 侧不准确 (Spec 需更新，实现正确)

### SPEC-1: resize 允许 SymLink 类型
- **Contract**: MSG-META-02
- **Spec**: `inode.type_ ∈ {Reg, Dir}`
- **实际**: 实现还允许 SymLink（用于空 symlink 增长为 slow symlink 的场景），这是有意为之
- **建议**: Spec 改为 `{Reg, Dir, SymLink}`

### SPEC-2: metadata() 的 dev 字段
- **Contract**: MSG-META-03
- **Spec**: `result.dev = ⊥` (not tracked)
- **实际**: 实现从 `block_device().id()` 填充 dev 字段，还填充了 rdev 字段（spec 未提及）
- **建议**: 更新 spec 反映实际行为

### SPEC-3: read_at 的 atime 更新
- **Contract**: MSG-IO-01
- **Spec**: INVARIANT `S' = S`，注释说 atime 是 VFS 层责任
- **实际**: ext2 实现在 `read_at` 内部调用 `set_atime(now())`
- **建议**: 更新 spec 的 FRAME 子句包含 `desc.atime`

### SPEC-4: sync_data 条件性持久化元数据
- **Contract**: MSG-IO-06
- **Spec**: "Inode metadata NOT necessarily persisted"
- **实际**: 当 `desc.is_dirty()` 时会持久化元数据（Linux fdatasync 语义）
- **建议**: 更新 spec 说明 data-affecting metadata 会被持久化

### SPEC-5: rename 对无效名称返回 EISDIR
- **Contract**: MSG-DIR-08
- **Spec**: 空名/超长名应返回 EINVAL
- **实际**: 返回 EISDIR（与 "."、".." 检查合并）
- **建议**: 更新 spec 或修正实现的 errno

---

## Implementation Concerns (非 bug，但值得关注)

### CONCERN-1: sync_all 缺少显式 xattr block flush
- **Contract**: MSG-IO-05
- **风险**: Low（xattr 操作会立即 flush，实际不会有 dirty xattr block 残留）
- **建议**: 可考虑在 sync_all 中添加防御性 xattr flush

### CONCERN-2: fallocate PunchHoleKeepSize 不释放底层 block
- **Contract**: MSG-IO-07
- **现状**: 只清零 page cache 数据，不释放磁盘 block
- **风险**: 已知限制，兼容性实现

### CONCERN-3: set_xattr 部分失败无 rollback
- **Location**: `xattr.rs:633` (TODO 注释)
- **风险**: 如果 `alloc_bid_if_needed()` 成功但后续 `write_working_block` 失败，已分配的 block 会泄漏

### CONCERN-4: link() 的 TOCTOU
- **Contract**: MSG-DIR-05
- **现状**: `links_count` 在 read lock 下检查，在 write lock 下递增，中间有窗口
- **风险**: 与 Linux ext2 行为一致

### CONCERN-5: 挂载时不验证 root inode 类型和 links_count
- **Invariant**: INV-2
- **现状**: 信任磁盘数据，不检查 `root.type_ == Dir` 或 `links_count >= 2`
- **风险**: 损坏的文件系统镜像可能导致异常行为
