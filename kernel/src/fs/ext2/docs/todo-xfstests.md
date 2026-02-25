# Ext2 TODO: xfstests 对齐计划

> 目标：修复功能缺口，尽可能通过 xfstests generic 测试集。
> 基于 fix-list.md 审计 + 代码实际状态 + xfstests 覆盖分析。
> 更新日期：2026-02-25

---

## fix-list.md 状态核对

已修复（代码已合入 refactor_ext2）：
- ~~F02~~ uid/gid from credentials（fs.rs:530-535）
- ~~F03~~ inodes_count 宽松校验（super_block.rs:209-219）
- ~~F04~~ ext2_setup_super（super_block.rs:307-310）
- ~~F05~~ create_inode 时间戳（fs.rs:555-557）
- ~~F09~~ find_entry 用 i_size 而非 i_blocks（inode.rs:2182）
- ~~F17~~ symlink（commit d8afce08）
- ~~F18~~ special files / mknod（commit be13a2c0）
- ~~F20~~ generation counter（fs.rs:549-550）

仍需修复：F01, F06, F07, F08, F10, F11, F12, F13, F14, F15, F16, F19

---

## Stage 0: 阻塞性（不修会 panic 或数据损坏）

| # | 问题 | 位置 | fix-list | xfstests 影响 |
|---|------|------|----------|---------------|
| T01 | counter 算术不统一，SuperBlock 路径会 panic | super_block.rs, block_group.rs | F01 | 高负载写入直接 panic |

**T01**：三种策略混用——`checked_add().unwrap()`（panic）、raw `+= 1`（溢出）、`saturating_*`（截断）。
Linux 用 `le16_add_cpu()` under spinlock，bitmap 是 source of truth。
→ 统一为 `saturating_*` + `log::warn!`，counter 不应 panic 内核。

---

## Stage 1: 功能正确性（xfstests generic 高频失败项）

| # | 问题 | 位置 | fix-list | xfstests 影响 |
|---|------|------|----------|---------------|
| T04 | hard link 无 EMLINK 检查，`saturating_add` 静默截断到 65535 | inode.rs:3485 | 新发现 | link 超限测试 |
| T05 | `statfs` 缺 overhead 计算，`f_bavail` 未扣 reserved blocks | impl_for_vfs/fs.rs | F08+F15 | generic/statfs 系列 |
| T06 | 无 reserved block 策略，非 root 可耗尽磁盘 | fs.rs:344 | F08 | ENOSPC 边界测试 |
| T07 | `sync_metadata` 不从 group desc 重算 free counts | fs.rs:593 | F14 | umount→remount 后 statfs 不一致 |
| T08 | symlink 写失败无 rollback，block 泄漏 | inode.rs:407 | 新发现 | symlink 创建失败路径 |
| T09 | `get_block` 无 `verify_chain`，并发下读 stale indirect | inode.rs block mapping | F11 | 并发读写测试 |

**T04**：`links_count` 用 `saturating_add(1)` 到 u16 上限后静默不增。
Linux 在 `ext2_link` 中检查 `EXT2_LINK_MAX` (65000) 并返回 EMLINK。
→ 在 `link()` 入口检查 `links_count >= EXT2_LINK_MAX`，返回 `Errno::EMLINK`。

**T05**：`FileSystem::sb()` 返回的 `f_bavail` 应为 `f_bfree - reserved_blocks_count`（非 root 视角）。
Linux `ext2_statfs` 计算 overhead（superblock + group desc + bitmap + inode table blocks）。
→ 实现 overhead 计算；`f_bavail = max(0, f_bfree - s_r_blocks_count)`。

**T06**：当前只检查 `sb_free_blocks == 0`。Linux 非 root 用户在 `free < s_r_blocks_count` 时返回 ENOSPC。
→ 添加 `has_free_blocks()`：非 root 且无 `CAP_SYS_RESOURCE` 时，free 需 > reserved。

**T07**：`sync_metadata` 写 superblock 前应从各 group descriptor 汇总 free_blocks/free_inodes。
Linux `ext2_sync_super` 调 `ext2_count_free_blocks/inodes`。
→ 在 `sync_metadata` 开头遍历 groups 重算 counter。

**T08**：`set_symlink_target()` 分配 block 后写入失败，已分配的 block 不回收。
→ 添加 rollback：写失败时释放已分配的 block，恢复 inode 状态。

**T09**：读 indirect chain 时不验证首指针是否被并发 truncate 修改。
Linux 用 `ext2_get_branch` + `verify_chain` + `i_meta_lock`。
→ 读完 chain 后 verify 首指针未变；变了则 retry。

---

## Stage 2: 语义完整性（通过更多 xfstests 边界用例）

| # | 问题 | 位置 | fix-list | xfstests 影响 |
|---|------|------|----------|---------------|
| T10 | 无 max file size 强制检查 | inode.rs resize/write | 新发现 | 大文件写入边界测试 |
| T11 | fast symlink 判定未考虑 EA blocks | inode.rs:1422-1428 | 新发现 | 带 xattr 的 symlink 读写 |
| T12 | `free_inode` 读磁盘判断 is_dir，应由 caller 传入 | fs.rs:567 | F10 | 性能 + 不必要 I/O |
| T13 | block alloc 总从 group 0 开始，无 goal block | fs.rs:349 | F07 | 碎片化 + 分配效率 |
| T14 | 目录 timestamp 更新路径需审计完整性 | inode.rs dir 操作 | F06 | stat 时间戳测试 |

**T10**：当前只检查 `size > i64::MAX`（inode.rs:1088）。ext2 实际上限取决于 block size：
4KB block → 最大 ~4TB（triple indirect 上限）。Linux `ext2_max_size()` 在 mount 时计算。
→ 在 `SuperBlock` 中计算并缓存 `max_file_size`，resize/write 时检查。

**T11**：fast symlink 判定用 `blocks == 0 && size <= 60`，但 Linux 用 `i_blocks - ea_blocks == 0`。
如果 symlink 有 xattr block，`i_blocks != 0` 但数据仍在 `i_block[]` 中。
→ 当 xattr 实现后需修正；当前无 xattr 所以暂不影响。标记为 deferred。

**T12**：`free_inode(ino)` 从磁盘读 inode 只为判断 `is_dir`。caller 已知 type。
→ 改签名为 `free_inode(ino, is_dir: bool)`，省一次磁盘 I/O。

**T13**：block 分配从 group 0 线性扫描。Linux 从 `goal_group`（inode 所在 group）开始。
→ 接受 `goal: Bid` 参数，从 goal 所在 group 开始 cyclic 扫描。

**T14**：`update_dir_timestamps_and_flags()` 已存在（inode.rs:3257），需审计所有 dir 操作路径
（create/link/unlink/rmdir/rename）是否都正确调用。

---

## Stage 3: 性能与健壮性（不影响正确性但影响通过率）

| # | 问题 | 位置 | fix-list | xfstests 影响 |
|---|------|------|----------|---------------|
| T15 | 无 `i_dir_start_lookup` hint，目录查找总从 block 0 开始 | inode.rs find_entry | F12 | 大目录性能测试 |
| T16 | inode 分配无 quadratic probing，简单 cyclic scan | fs.rs:440-481 | F13 | inode 耗尽场景（blocked/ext2.txt: rename14） |
| T17 | 统一 truncate/evict pipeline，消除重复清理逻辑 | inode.rs:3248 | 新发现 | 错误路径 block 泄漏 |
| T18 | PageCache::discard_range bug 导致 truncate 测试失败 | inode.rs:5920 | 新发现 | truncate 相关测试 |

**T15**：`find_entry` 每次从 block 0 线性扫描。Linux 缓存上次命中位置 `i_dir_start_lookup`。
→ 在 `InodeInner` 加 `dir_start_lookup: u32`，find 成功后更新，下次从该位置开始 wrap-around。

**T16**：inode 分配从 parent group 开始简单 cyclic scan。Linux 对文件用 quadratic probing（`find_group_other`）。
→ 最低限度实现 quadratic probing：`group = (group + 1 + i*(i+1)/2) % ngroups`。

**T17**：mkdir/rmdir 失败时用 `release_dir_data_blocks_for_cleanup()` 内联清理，与 evict 路径重复。
→ 抽取统一的 `truncate_and_free` pipeline，evict 和错误路径共用。

**T18**：这是 OSTD 层的 bug，不在 ext2 模块内。需要上报/修复 PageCache::discard_range。
→ 跟踪 OSTD issue；ext2 侧暂时跳过相关测试。

---

## Stage 4: 新子系统（大量 xfstests 依赖但工作量大）

| # | 问题 | 位置 | fix-list | xfstests 影响 |
|---|------|------|----------|---------------|
| T19 | 无 orphan inode 管理（add/remove/cleanup） | fs.rs, super_block.rs | F16 | unlink-while-open 测试 |
| T20 | 无 xattr 支持（全部返回 EOPNOTSUPP） | impl_for_vfs/inode.rs:215-234 | F19 | generic xattr 系列（~30+ 测试） |
| T21 | 无 ioctl 支持（getflags/setflags/getversion） | 未实现 | F19 | chattr/lsattr 相关测试 |

**T19**：orphan inode 是 ext2 crash recovery 的核心机制。
Linux 在 unlink（nlink→0 但仍有 fd open）时将 inode 加入 orphan 链表（`s_last_orphan` → `i_dtime` 链）。
mount 时扫描链表，truncate + free 所有 orphan inode。
当前 `evict_inode` 已实现（block_group.rs:276），但缺少：
- `add_orphan(inode)` — unlink 时加入链表
- `remove_orphan(inode)` — close 时移除
- `cleanup_orphans()` — mount 时清理
→ 工作量中等，影响面广（所有 unlink-while-open 场景）。

**T20**：xattr 是独立子系统，需要：
- on-disk xattr block 解析（`ext2_xattr_header` + `ext2_xattr_entry`）
- namespace handler（user/trusted/security）
- block 分配/共享/引用计数
→ 工作量大。xfstests 中 ~30+ 测试依赖 xattr，但多数会被 skip（EOPNOTSUPP）而非 fail。
→ 优先级低于 T01-T19，除非 xfstests 配置强制要求。

**T21**：ext2 ioctl 主要是 `EXT2_IOC_GETFLAGS/SETFLAGS`（immutable/append-only 等）和 `GETVERSION/SETVERSION`。
→ 需要 VFS 层 ioctl dispatch 支持。工作量中等。

---

## 建议执行顺序

```
第一轮：消除 panic + 核心功能修正（T01 → T05 → T06 → T07）
  T01  counter 统一           ~1h   消除 panic 风险
  T05  statfs overhead        ~2h   statfs 测试全面依赖
  T06  reserved block 策略    ~2h   ENOSPC 语义正确性
  T07  sync_metadata 重算     ~1h   remount 一致性

第二轮：关键语义修正（T04 → T10 → T14）
  T04  EMLINK 检查            ~0.5h 一行检查
  T10  max file size 检查     ~1h   计算 + 缓存 + 检查点
  T14  dir timestamp 审计     ~1h   审计 + 补漏

第三轮：健壮性修正（T08 → T09 → T12 → T13）
  T08  symlink rollback       ~2h   错误路径 block 回收
  T09  verify_chain           ~3h   需要理解锁协议
  T12  free_inode 签名优化    ~0.5h 接口变更
  T13  goal block 分配        ~2h   cyclic scan 起点

第四轮：性能优化（T15 → T16 → T17）
  T15  dir lookup hint        ~1h
  T16  quadratic probing      ~2h
  T17  统一 evict pipeline    ~3h

第五轮：新子系统（T19 → T21 → T20）
  T19  orphan inode           ~8h   核心 crash recovery
  T21  ioctl                  ~4h   需 VFS 层配合
  T20  xattr                  ~16h  独立子系统，可后置

T18（PageCache bug）：非 ext2 模块问题，需 OSTD 团队修复。
T11（fast symlink EA）：等 T20 xattr 实现后再修。
```

---

## 总览

| Stage | 项数 | 预估工作量 | 预期 xfstests 收益 |
|-------|------|-----------|-------------------|
| 0 阻塞性 | 1 | ~1h | 消除 panic 风险 |
| 1 功能正确性 | 6 | ~10h | statfs/ENOSPC/link/sync 测试通过 |
| 2 语义完整性 | 5 | ~7h | 边界用例 + 大文件 + 目录时间戳 |
| 3 性能与健壮性 | 4 | ~7h | 大目录/并发/inode 耗尽场景 |
| 4 新子系统 | 3 | ~28h | orphan/ioctl/xattr 测试集 |
| **合计** | **19** | **~53h** | |

Stage 0-2（12 项，~18h）完成后，预期可通过 xfstests generic 中大部分基础测试。
Stage 3 进一步提升并发和大规模场景的通过率。
Stage 4 是长期目标，xattr/ioctl 测试多数会被 skip 而非 fail。
