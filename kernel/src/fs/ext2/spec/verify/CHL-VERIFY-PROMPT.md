# Ext2 CHL Dual-Layer Spec Verification Prompt

## 你的角色

你是一个形式化验证审计员。你的任务是验证 Asterinas Ext2 文件系统的 Crash Hoare Logic (CHL) 双层 spec 架构的**内部一致性**和**与源码的忠实性**。

## 仓库结构

```
kernel/src/fs/ext2/
├── inode.rs                          # 主要实现 (~3800 行)
├── impl_for_vfs/inode.rs             # VFS Inode trait 实现
├── impl_for_vfs/fs.rs                # VFS FileSystem trait 实现
└── spec/
    ├── layer1_hoare/                 # 纯 Hoare 逻辑 spec (抽象状态)
    │   ├── 00-abstract-state.spec    # AbstractFS 模型 + 10 不变量
    │   ├── 01-metadata-ops.spec      # 21 HOARE specs
    │   ├── 02-file-io.spec           # 2 HOARE specs
    │   ├── 03-dir-ops.spec           # 9 HOARE specs
    │   ├── 04-symlink-sync.spec      # 5 HOARE specs
    │   ├── 05-xattr-fs.spec         # 9 HOARE specs
    │   └── 06-composition.spec       # 10 COMP + 10 INV preservation
    ├── layer2_protocol/              # 实现协议 + SATISFIES_PROOF
    │   ├── 00-impl-state.spec        # 具体状态 + 锁模型
    │   ├── 01-metadata-ops.spec      # 23 PROTOCOL
    │   ├── 02-file-io.spec           # 4 PROTOCOL
    │   ├── 03-dir-ops.spec           # 10 PROTOCOL
    │   ├── 04-symlink-sync.spec      # 8 PROTOCOL
    │   ├── 05-xattr-fs.spec         # 9 PROTOCOL
    │   └── 06-cross-protocol.spec    # 死锁自由 + crash 组合 + 覆盖
    └── state_machine/archive/        # 旧 spec (参考)
```

## 验证任务

按以下 7 个维度逐一检查，每个维度输出 PASS / ISSUE(问题描述) / BUG(严重问题)。

---

### V1: Layer 1 抽象纯净性

**规则**: Layer 1 的 HOARE spec 中 PRE/POST/POST_ERR/CRASH 子句只能引用 `00-abstract-state.spec` 中定义的 AbstractFS 字段。不允许出现以下实现概念：

- 锁 (RwMutex, read/write/upread/upgrade, LOCK/UNLOCK)
- page cache, PageCache, Vmo, dirty pages
- Dirty<>, is_dirty, persist, persist_inode_and_sync
- InodeInner, InodeDesc (具体 Rust 类型名)
- block_device, block_group, inode_cache (具体实现结构)
- 具体 Rust 方法名 (fs_arc, find_entry, get_or_alloc_block 等)

**检查方法**: 逐文件扫描 `layer1_hoare/01-05*.spec` 的 PRE/POST/POST_ERR/CRASH 块，标记任何违规引用。`LINUX_REF` 和注释中的实现提示可以保留。

---

### V2: Layer 2 SATISFIES 完备性

**规则**:
1. 每个 Layer 2 PROTOCOL 必须声明 `SATISFIES: layer1::xxx`，且 xxx 必须是 Layer 1 中实际存在的 HOARE spec 名称
2. 每个 Layer 1 HOARE spec 必须被至少一个 Layer 2 PROTOCOL 引用
3. 对于拆分的 PROTOCOL（如 read_at_buffered / read_at_direct），其 DISPATCH 条件的并集必须覆盖所有输入

**检查方法**:
- 提取所有 Layer 1 HOARE 名称集合 H = {read_at, write_at, ...}
- 提取所有 Layer 2 SATISFIES 目标集合 S = {layer1::read_at, ...}
- 验证 S 的去重值 ⊆ H（前向完备）
- 验证 H ⊆ S 的去重值（反向完备）
- 对每组拆分 PROTOCOL，验证 DISPATCH 条件互斥且穷尽

---

### V3: SATISFIES_PROOF 逐条验证

**规则**: 每个 PROTOCOL 的 SATISFIES_PROOF 必须逐条对应 Layer 1 HOARE spec 的每个子句：

| Layer 1 子句 | SATISFIES_PROOF 必须包含 |
|---|---|
| PRE | PRE: PROTOCOL.REQUIRE ⟹ HOARE.PRE |
| POST (每个字段) | POST.field: 引用具体 STEP 编号 |
| FRAME | FRAME: 证明未修改的字段确实未被触及 |
| POST_ERR | POST_ERR: 引用 ROLLBACK 或说明无副作用 |
| CRASH | CRASH: 每个 CRASH_WINDOW ⊆ HOARE.CRASH 集合 |

**检查方法**: 对每个 PROTOCOL，打开对应的 Layer 1 HOARE spec，逐条比对：
- POST 中的每个字段变更是否在 SATISFIES_PROOF 中有对应条目？
- CRASH 集合中的每个元素是否被 CRASH_WINDOW 覆盖？
- 是否有遗漏的子句？

---

### V4: 源码忠实性

**规则**: Layer 2 PROTOCOL 的 STEPS 必须忠实反映源码的实际执行流程。

**检查方法**: 对以下高风险 PROTOCOL，逐步比对源码：

1. **write_at_buffered** — 对比 `inode.rs` 中 write_at 的 buffered 路径
   - 三阶段锁模式 (WRITE → UPREAD → WRITE) 是否与代码一致？
   - write_failed_cleanup 的触发条件和恢复步骤是否准确？
   - 块分配循环的范围计算是否正确？

2. **create_mkdir** — 对比 `inode.rs` 中 create 的 Dir 路径
   - inode 分配 → make_empty → add_entry → parent link++ 的顺序是否正确？
   - rollback 路径（free_inode）是否覆盖所有失败点？

3. **rename_cross_dir** — 对比 `inode.rs` 中 rename 的跨目录路径
   - 两个 inode 的锁获取顺序是否与 write_lock_two_inodes 一致？
   - dotdot 更新和 link count 调整是否完整？

4. **set_xattr** — 对比 `inode.rs` 中 set_xattr
   - xattr lock → inner lock 的两阶段模式是否准确？
   - block 分配和 file_acl 更新的顺序是否正确？

5. **resize (shrink)** — 对比 `inode.rs` 中 resize 的缩小路径
   - upread → upgrade 模式是否准确？
   - truncate_blocks 和 page_cache 操作的顺序是否正确？

---

### V5: 不变量保持证明

**规则**: `06-composition.spec` 中的 10 个不变量保持证明必须：
1. 正确识别所有可能威胁该不变量的 HOARE spec
2. 对每个威胁者，引用其 POST/FRAME 子句证明不变量被保持

**检查方法**: 对每个 INV-01 到 INV-10：
- 是否遗漏了某个可能修改相关字段的 HOARE spec？
- 引用的 POST/FRAME 子句是否确实蕴含不变量保持？

重点检查：
- INV-04 (link count consistency): create, mkdir, link, unlink, rmdir, rename 是否全部覆盖？
- INV-07 (superblock counters): 所有分配/释放操作是否覆盖？
- INV-10 (dir links ≥ 2): mkdir 和 rmdir 的证明是否完整？

---

### V6: Crash 语义一致性

**规则**:
1. Layer 1 CRASH 集合必须是保守的超集（允许比实际更多的 crash 状态）
2. Layer 2 CRASH_WINDOW 的可达状态必须是 Layer 1 CRASH 集合的子集
3. 无日志 ext2 的 crash 语义：crash 后恢复到 `FS.durable`，fsck 修复结构不一致

**检查方法**:
- 对每个有 CRASH_WINDOW 的 PROTOCOL，验证每个 window 的效果是否在 Layer 1 CRASH 集合中
- 检查 fsck 保证是否被正确引用
- 特别关注：
  - write_at: W1 (blocks allocated, data zero) 是否在 CRASH 的 partial_write 中？
  - rename: W1 (duplicate entries) 是否在 CRASH 中？
  - create_mkdir: W1 (orphan inode) 是否在 CRASH 中？

---

### V7: 组合属性可推导性

**规则**: 10 个 COMP 属性必须可以从各自引用的 HOARE spec 的 POST 子句机械推导出来。

**检查方法**: 对每个 COMP-1 到 COMP-10：
1. 将 C1 的 POST 代入 C2 的 PRE，检查 PRE 是否满足
2. 将 C1 的 POST 和 C2 的 POST 组合，检查是否蕴含 COMP 的 POST
3. 检查 CRASH 子句是否正确组合了两个操作的 crash 集合

重点检查：
- COMP-5 (Write-Read roundtrip): write_at 的 splice 语义 + read_at 的读取语义是否确实蕴含 roundtrip？
- COMP-2 (Link-Unlink inverse): link 的 links_count++ 和 unlink 的 links_count-- 是否精确抵消？

---

## 输出格式

对每个维度，输出：

```
## V{N}: {维度名}

### 检查结果

| 文件 | 条目 | 状态 | 说明 |
|------|------|------|------|
| ... | ... | PASS/ISSUE/BUG | ... |

### 发现的问题

#### {ISSUE/BUG}-{序号}: {标题}
- 位置: {文件:行号}
- 严重性: BUG / ISSUE
- 描述: ...
- 建议修复: ...
```

最后输出汇总表：

```
## 汇总

| 维度 | PASS | ISSUE | BUG | 总计 |
|------|------|-------|-----|------|
| V1 | ... | ... | ... | ... |
| ... |
| 总计 | ... | ... | ... | ... |
```

## 重要提示

1. 你必须实际读取每个 spec 文件和对应的源码，不要凭记忆或猜测
2. 对于 V4 (源码忠实性)，必须读取 `inode.rs` 中的实际代码并逐步比对
3. 如果发现 Layer 1 spec 与源码行为不一致（类似旧 VERIFICATION-REPORT.md 中的 BUG-1/2/3），这是 BUG 级别
4. 如果发现 SATISFIES_PROOF 遗漏了某个 POST 子句，这是 ISSUE 级别
5. 优先检查高复杂度的 spec: write_at, rename, create_mkdir, set_xattr, resize
