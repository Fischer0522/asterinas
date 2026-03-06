# Ext2 Spec Verify 总结（并行核对）

## 1. 范围与方式
- Spec 目录：`kernel/src/fs/ext2/spec/verify/`
- 覆盖文件：
  - `vfs-contract-filesystem.spec`
  - `vfs-contract-metadata.spec`
  - `vfs-contract-io-sync-xattr.spec`
  - `vfs-contract-dir-ops.spec`
  - `vfs-contract-global.spec`
- 执行方式：拆分为 5 个并行子任务（filesystem / metadata / io+xattr / dir-ops / global invariants），静态对照实现，不改代码。

## 2. 总体结论
- 核对总量（合同+不变量+组合性质）：约 65 条
- `SATISFIED`: 43
- `POSSIBLE_GAP`: 12
- `VIOLATION`: 10

## 3. 已确认 VIOLATION（逐条）

### V1. MSG-FS-02 `sync()` 覆盖不全
- 旧结论已过时：当前实现已改为 `Ext2::sync_all()` 统一组织 `Ext2 -> BlockGroup -> Inode`，并在 `BlockGroup::sync_all()` 中覆盖组内 inode 与 metadata，同步结束后由 VFS wrapper 做一次最终 device flush。
- 先前关于 `root_inode` 可能漏刷的判断也已过时：root inode 通过 `read_inode(ROOT_INO)` 懒加载后会进入 per-group `inode_cache`，因此参与正常的 group sync 遍历。
- 当前该项应视为“已修正 / 已对齐文档”，而非现存 violation。

### V2. MSG-META-02 `resize()` 语义偏差
- 问题：类型约束、扩容行为、失败回滚与 spec 不一致。
- 证据：
  - `kernel/src/fs/ext2/impl_for_vfs/inode.rs:55`
  - `kernel/src/fs/ext2/inode.rs:146`
  - `kernel/src/fs/ext2/inode.rs:200`
  - `kernel/src/fs/ext2/inode.rs:231`
- 可能漏洞：失败后内存态/磁盘态分裂。
- 结果：size/blocks 不一致，后续 I/O 异常或一致性劣化。

### V3. MSG-IO-01 `read_at()` 读路径改 `atime`
- 问题：读操作触发 `atime` 更新，且类型限制与 spec 不完全一致。
- 证据：
  - `kernel/src/fs/ext2/inode.rs:568`
  - `kernel/src/fs/ext2/inode.rs:588`
- 可能漏洞：只读流量可诱发额外写放大（I/O DoS 面）。
- 结果：性能下降、介质磨损、审计噪声增加。

### V4. MSG-IO-04 `write_link()` 未更新 `ctime/mtime`
- 问题：fast/slow symlink 写入后未见 `ctime/mtime` 更新；slow path NUL 终止语义不稳。
- 证据：
  - `kernel/src/fs/ext2/inode.rs:504`
  - `kernel/src/fs/ext2/inode.rs:537`
  - `kernel/src/fs/ext2/inode.rs:552`
- 可能漏洞：修改链接目标但时间戳未反映，可绕过部分审计策略。
- 结果：增量备份/监控/取证可能误判。

### V5. MSG-IO-07 `fallocate()` 语义偏差
- 问题：`Allocate`/`PunchHoleKeepSize` 行为与 spec 不一致。
- 证据：
  - `kernel/src/fs/ext2/inode.rs:1138`
  - `kernel/src/fs/ext2/inode.rs:1147`
  - `kernel/src/fs/ext2/inode.rs:1150`
- 可能漏洞：预分配承诺与真实分配不一致，空间管理可被打穿。
- 结果：晚发 ENOSPC、空间利用异常、性能抖动。

### V6. MSG-DIR-01 `create()` 目录创建语义偏差
- 问题：`Socket` 被拒绝，且新 inode gid 语义与 spec 预期不同。
- 证据：
  - `kernel/src/fs/ext2/inode.rs:3452`
  - `kernel/src/fs/ext2/fs.rs:602`
- 可能漏洞：group ownership 偏差导致权限策略偏移。
- 结果：访问控制过宽/过严，兼容性问题。

### V7. MSG-DIR-04 `readdir_at()` 返回值语义偏差
- 问题：返回“前进量 advanced”，而非 spec 要求的“绝对 offset”语义。
- 证据：
  - `kernel/src/fs/ext2/inode.rs:2415`
- 可能漏洞：上层遍历器可能漏扫、重复或死循环。
- 结果：备份/扫描/清理任务出现目录漏项或重复处理。

### V8. MSG-DIR-05 `link()` 失败回滚不完整
- 问题：错误回滚未恢复全部状态（`ctime` 留痕）。
- 证据：
  - `kernel/src/fs/ext2/inode.rs:3544`
  - `kernel/src/fs/ext2/inode.rs:3552`
- 可能漏洞：失败调用仍污染元数据，形成审计不可信。
- 结果：变更检测假阳性、审计链条污染。

### V9. MSG-DIR-07 `rmdir()` 子目录时间戳语义偏差
- 问题：删除成功后子目录 `ctime` 更新不满足 spec 预期。
- 证据：
  - `kernel/src/fs/ext2/inode.rs:999`
  - `kernel/src/fs/ext2/inode.rs:1037`
- 可能漏洞：时间线不完整，取证能力下降。
- 结果：删除事件可见性不足。

### V10. INV-10 xattr block 有效性校验不完整
- 问题：读取路径未强制 `h_refcount == 1`。
- 证据：
  - `kernel/src/fs/ext2/xattr.rs:230`
  - `kernel/src/fs/ext2/xattr.rs:577`
- 可能漏洞：恶意镜像可构造 xattr block 共享/别名。
- 结果：跨 inode 元数据串扰风险，完整性/权限语义可能被破坏（高风险）。

## 4. 关键 POSSIBLE_GAP（建议先补测）
- 挂载时 root inode 类型/links 强校验是否缺失：
  - `kernel/src/fs/ext2/fs.rs:74`
- `mknod()` 二阶段失败回滚是否完整：
  - `kernel/src/fs/ext2/impl_for_vfs/inode.rs:144`
- `write_at()` 持久化失败后是否真正 `S'=S`：
  - `kernel/src/fs/ext2/inode.rs:660`
- `set_xattr()` 错误路径回滚 TODO：
  - `kernel/src/fs/ext2/xattr.rs:633`
- superblock 计数与 group 计数初始一致性：
  - `kernel/src/fs/ext2/fs.rs:759`
- `sync_all` 幂等性缺显式测试：
  - `kernel/src/fs/ext2/inode.rs:1166`

## 5. 修复优先级建议
- P0（先修）：V10, V1, V2
- P1：V5, V7, V8
- P2：V3, V4, V6, V9

## 6. 建议下一步
- 先为每个 `VIOLATION` 补最小 ktest（含故障注入），确认可复现。
- 再决定“改实现”还是“调 spec”：
  - 若 Linux 语义优先，优先修实现。
  - 若当前行为是有意设计，回写 spec 并记录差异原因。
