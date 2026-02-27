# Ext2 Inode 生命周期管理缺陷分析

> 日期: 2026-02-27
> 分支: refactor_ext2
> 分析范围: Ext2 层 (`kernel/src/fs/ext2/`) + VFS 层 (`kernel/src/fs/path/`)

---

## 1. 背景

多个 xfstest 用例在完成测试后，通过 `fsck.ext2` 检查会报告文件系统不一致，典型表现为：

- Inode 已释放（`links_count=0`, `dtime` 已设置）但 bitmap 中对应 bit 仍为 1
- Superblock 与 group descriptor 的 `free_inodes_count` 不一致
- 数据块被占用但无 inode 引用（孤儿块）

本文档从 Ext2 层和 VFS 层两个维度，严格分析当前实现中的缺陷。

---

## 2. 当前 Inode 生命周期概览

```
分配 ──► 使用 ──► unlink/rmdir ──► [等待 sync] ──► eviction ──► bitmap 释放
  │                    │                  │              │
  │  alloc_inode()     │  links_count--   │  deferred    │  free_inode(bit)
  │  create_inode()    │  is_freed=true   │              │  inc_free_inodes()
  │  insert_cache()    │  persist to disk │              │
```

关键设计：bitmap 释放不在 unlink/rmdir 时发生，而是推迟到 `sync_all_inodes()` 的 eviction 阶段。

---

## 3. 问题清单

### 3.1 【P0 严重】缺乏 `iput_final` 等价机制 — bitmap 释放无保障

**位置**: 整体架构缺陷

**Linux 行为**:
Linux 在 `iput()` 中，当引用计数降为 0 且 `i_nlink == 0` 时，立即调用 `iput_final()` → `evict_inode()`，同步完成：
1. 截断数据块
2. 清除 inode descriptor
3. 释放 inode bitmap

**Asterinas 行为**:
- `InodeHandle::drop()` (`inode_handle.rs:481-486`) 只释放 range locks 和 flock
- 没有任何机制在最后一个 VFS 引用释放时触发 inode 回收
- bitmap 释放完全依赖 `sync_all_inodes()` 被显式调用

**触发 `sync_all_inodes()` 的唯一路径**:

```
sys_sync()  ──► mount_namespace.sync() ──► mount.sync() ──► Ext2::sync()
sys_syncfs() ──► file.path().fs().sync() ──► Ext2::sync()
```

参考 `impl_for_vfs/fs.rs:17-23`:
```rust
fn sync(&self) -> Result<()> {
    self.sync_all_inodes()?;   // 这里才触发 eviction
    self.sync_metadata()?;
    self.block_device().sync()?;
    Ok(())
}
```

**后果**: 如果测试程序 unlink 文件后没有显式调用 `sync()`/`syncfs()`，bitmap 永远不会被清理。
fsck 会看到: inode 的 `dtime` 已设置、`links_count=0`，但 bitmap bit 仍为 1。

---

### 3.2 【P0 严重】`sync_all_inodes()` eviction 条件 `strong_count == 1` 过于脆弱

**位置**: `block_group.rs:317-322`

```rust
// Phase 1: remove unreferenced inodes from cache.
let unused_inodes: Vec<Arc<Inode>> = self
    .inode_cache
    .write()
    .extract_if(.., |_, inode| Arc::strong_count(inode) == 1)
    .map(|(_, inode)| inode)
    .collect();
```

即使 `sync_all_inodes()` 被调用，如果任何外部引用仍然存在，inode 就不会被 evict。
以下场景会导致引用无法降为 1：

**场景 A — VFS Dentry 缓存持有引用**:

`Dentry` 结构体 (`dentry.rs:22`) 持有 `Arc<dyn Inode>`。
`DentryChildren::delete()` (`dentry.rs:618`) 将条目置为 `None`（变成 negative dentry），
返回的 `Option<Arc<Dentry>>` 在调用栈中仍然存活。
只要 dentry 存在，其内部的 `Arc<dyn Inode>` 就保持 inode 的 strong_count > 1。

```rust
// dentry.rs:618 — delete 只是 take，不 drop inode
fn delete(&mut self, name: &str) -> Option<Arc<Dentry>> {
    self.dentries.get_mut(name).and_then(Option::take)
}
```

**场景 B — rename 覆盖路径**:

`rename_same_dir()` 和 `rename_inner()` 中，被覆盖的 inode 通过
`fs.read_inode(existing_ino)` 加载到缓存。设置 `is_freed=true` 后，
`Arc<Inode>` 在函数返回后才 drop。但 inode 已在缓存中（`read_inode` 插入缓存），
所以 strong_count 至少为 2。

**场景 C — VFS rename 不清理被覆盖 inode 的 dentry**:

`dentry.rs:474-489` rename 同目录时：
```rust
children.delete(old_name);  // 删除旧名
children.insert(new_name, dentry.clone());  // 插入新名
// 但 new_name 原来指向的 dentry 没有做任何 inode 清理
```

被覆盖的 inode 的 dentry 被 `delete` 返回后直接丢弃，
但如果其他地方还持有该 dentry 的引用，inode 就永远不会被 evict。

**后果**: 即使调用了 `sync()`，`nlink=0` 的 inode 仍可能因为引用泄漏而跳过 eviction，
导致 bitmap 不释放。

---

### 3.3 【P0 严重】eviction 失败导致孤儿 inode — 从缓存移除但 bitmap 未释放

**位置**: `block_group.rs:310-338`

`sync_all_inodes()` 分三个阶段执行：

```rust
// Phase 1: 从缓存移除 strong_count==1 的 inode
let unused_inodes: Vec<Arc<Inode>> = self
    .inode_cache.write()
    .extract_if(.., |_, inode| Arc::strong_count(inode) == 1)
    .map(|(_, inode)| inode).collect();

// Phase 2: evict 移除的 inode（不持有缓存锁）
for inode in &unused_inodes {
    let result = self.evict_inode(inode)?;  // ← 错误直接传播
    // ...
}

// Phase 3: sync 仍在缓存中的 inode
```

**问题**: Phase 1 已经将 inode 从缓存中移除。如果 Phase 2 的 `evict_inode()` 失败
（`truncate_blocks` 或 `persist_inode_and_sync` 返回错误），`?` 直接传播错误，
后续的 `free_inode(bit)` 不会执行。

此时 inode 处于以下状态：
- 不在 inode_cache 中（Phase 1 已移除）
- bitmap bit 仍为 1（Phase 2 未完成）
- 数据块可能部分释放
- 没有目录项指向它

**这个 inode 成为永久孤儿**: 无法通过 `lookup_inode` 找到（不在缓存中，
但 bitmap 标记为已分配所以不会报 ENOENT — 实际上会从磁盘重新加载，见问题 3.6），
也无法通过任何代码路径回收。

---

### 3.4 【P1 中等】`prepare_for_evict` 中错误不回滚已完成的部分操作

**位置**: `inode.rs:1192-1212`

```rust
pub(super) fn prepare_for_evict(&self) -> Result<bool> {
    if self.inner.read().desc.links_count > 0 {
        self.sync_all()?;
        return Ok(false);
    }
    let fs = self.fs_arc()?;
    if let Some(xattr) = self.xattr.as_ref() {
        xattr.write().delete_xattr_block()?;   // ① 可能失败
    }
    let mut inner = self.inner.write();
    inner.desc.dtime = now();
    inner.is_freed = true;
    inner.desc.size = 0;
    inner.desc.file_acl = 0;
    inner.truncate_blocks(0)?;                  // ② 可能失败
    inner.persist_inode_and_sync(&fs)?;         // ③ 可能失败
    Ok(true)
}
```

**问题**: 操作序列中任何一步失败都不会回滚前面已完成的步骤：

- 如果 ① 成功但 ② 失败: xattr block 已释放，但数据块未释放
- 如果 ②（`truncate_blocks`）部分完成后失败: 部分数据块已释放，部分未释放
- 如果 ③ 失败: 内存中 inode 已修改但未持久化，磁盘状态不一致

结合问题 3.3，失败后 inode 从缓存移除，这些部分释放的块成为永久泄漏。

---

### 3.5 【P1 中等】`unlink` 过早设置 `is_freed` 和 `dtime`

**位置**: `inode.rs:3598-3601`

```rust
if child_inner.desc.links_count == 0 {
    child_inner.desc.dtime = now();
    child_inner.is_freed = true;
}
child_inner.persist_inode_and_sync(&fs)?;
```

**Linux 行为**:
Linux 在 `ext2_unlink()` 中只做 `inode_dec_link_count()`，
**不设置 `dtime`，也不设置任何 freed 标记**。
`dtime` 和实际清理只在 `ext2_evict_inode()` 中发生。

**Asterinas 问题**:
1. `is_freed` 在 unlink 时就被设置，但 `prepare_for_evict()` 也会设置。
   两处重复设置本身无害，但语义混乱。

2. `dtime` 被过早持久化到磁盘。如果此时崩溃，磁盘上的 inode 有
   `dtime != 0`、`links_count = 0`，但数据块仍被占用且 bitmap 标记为已分配。
   fsck 会认为这是一个需要清理的孤儿 inode。

3. 如果 unlink 后、eviction 前，有其他代码路径通过 `read_inode` 从缓存获取
   这个 inode，`is_freed` 已经是 true。这个中间状态可能导致意外行为。

---

### 3.6 【P1 中等】eviction Phase 1/2 之间的竞态窗口 — 已删除 inode 可被重新加载

**位置**: `block_group.rs:316-329` 与 `block_group.rs:227-266`

Phase 1 从缓存移除 inode（释放 inode_cache write lock），Phase 2 才释放 bitmap。
在这个窗口期内，如果有并发的 `lookup_inode` 调用：

1. bitmap 仍标记为已分配 → 通过 bitmap 检查
2. 缓存中找不到（已被 Phase 1 移除）
3. 从磁盘加载 inode（此时 `dtime` 已设置，`links_count=0`）
4. 创建新的 `Arc<Inode>` 并插入缓存

关键问题: `is_freed` 是 `InodeInner` 的运行时字段，**不从磁盘恢复**。
重新加载的 inode 的 `is_freed = false`，即使 `links_count = 0`。

```rust
// block_group.rs:261-264 — 从磁盘加载，is_freed 默认 false
let desc = self.read_inode_desc(inode_idx)?;
let desc = Dirty::new(desc);
let inode = Inode::new(ino, desc.type_(), desc, self.idx, fs);
inode_cache.insert(inode_idx, inode.clone());
```

**后果**: 这个 "僵尸" inode 在缓存中，`links_count=0` 但 `is_freed=false`，
后续 `prepare_for_evict` 会再次尝试 truncate 和释放，可能导致 double-free。

---

### 3.7 【P1 中等】`sync_all_inodes()` 中 superblock free_inodes 双重递增

**位置**: `block_group.rs:276-305` 与 `fs.rs:806-826`

eviction 路径中存在两层计数更新：

**第一层 — `evict_inode()` (block_group.rs:295)**:
```rust
self.inc_free_inodes(1);  // 更新 group descriptor
```

**第二层 — `sync_all_inodes()` (fs.rs:818-823)**:
```rust
if total.freed_inodes > 0 {
    let mut sb = self.super_block.write();
    for _ in 0..total.freed_inodes {
        sb.inc_free_inodes();  // 更新 superblock
    }
}
```

这两层分别更新 group descriptor 和 superblock，设计意图是分离职责。
但存在以下风险：

1. `evict_inode()` 内部调用 `prepare_for_evict()` → `persist_inode_and_sync()`，
   后者会调用 `fs.sync_metadata()`，此时 group descriptor 的 `free_inodes_count`
   已经递增并可能被写入磁盘。但 superblock 的 `free_inodes_count` 还没有更新。

2. 如果在 `evict_inode()` 完成后、`sync_all_inodes()` 更新 superblock 之前崩溃，
   磁盘上 group descriptor 的 `free_inodes_count` 之和将大于 superblock 的
   `free_inodes_count`。fsck 会报告不一致。

3. 注意 `Ext2::sync()` 的调用顺序是 `sync_all_inodes()` → `sync_metadata()`，
   superblock 的更新在 `sync_all_inodes()` 内部完成，但写入磁盘在 `sync_metadata()` 中。
   如果 `sync_metadata()` 失败，superblock 更新丢失。

---

### 3.8 【P2 设计缺陷】缺少 orphan inode 列表

**位置**: 整体架构缺失

**Linux 行为**:
Linux ext2 维护一个 orphan inode 链表：
- `superblock.s_last_orphan` 指向链表头
- 每个 orphan inode 的 `i_dtime` 字段（复用）指向下一个 orphan
- 在 `ext2_unlink()` 中，`nlink` 降为 0 时将 inode 加入 orphan 列表
- 在 `ext2_evict_inode()` 中，完成清理后将 inode 从 orphan 列表移除
- 挂载时（`ext2_fill_super`）遍历 orphan 列表，清理所有残留 orphan

**Asterinas 行为**:
完全没有实现 orphan 列表。

**后果**:
- 如果在 unlink 之后、`sync_all_inodes()` 之前崩溃，
  inode 的 bitmap 永远不会被释放
- 重新挂载后没有任何机制发现和清理这些孤儿 inode
- 只能依赖 `fsck.ext2` 外部工具修复

---

## 4. VFS 层问题

### 4.1 VFS Dentry 缓存阻止 Ext2 inode eviction

**位置**: `dentry.rs:20-29`

VFS 的 `Dentry` 持有 `Arc<dyn Inode>`，形成如下引用链：

```
DentryChildren (HashMap)
  └─ Option<Arc<Dentry>>
       └─ Arc<dyn Inode>  ──► Ext2 Arc<Inode> (strong_count +1)
                                    ↑
                          BlockGroup::inode_cache (strong_count +1)
```

当 `unlink()` 被调用时，`DentryChildren::delete()` 将条目置为 `None`，
返回 `Option<Arc<Dentry>>`。但这个返回值的生命周期取决于调用方。

参考 `dentry.rs:348-393` 的 unlink 路径：
```rust
let cached_child = children.delete(name);  // take from cache
let child_inode = match cached_child {
    Some(child) => child.inode().clone(),  // clone Arc<dyn Inode>
    None => { ... }
};
dir_inode.unlink(name)?;
// ... child_inode 在函数结束时 drop
```

`child_inode`（`Arc<dyn Inode>`）在函数结束时 drop，
但 `cached_child`（`Option<Arc<Dentry>>`）也在同一时刻 drop。
正常情况下引用会正确释放。

**但问题在于**: 如果其他线程在 unlink 之前已经通过 lookup 获取了
同一个 dentry 的 `Arc<Dentry>` 克隆（比如正在读取该文件），
那么即使 dentry cache 中的条目被删除，外部持有的 `Arc<Dentry>`
仍然保持 inode 的 strong_count > 1，阻止 eviction。

**这本身是正确行为**（打开的文件不应被回收），但当前实现缺少
在最后一个外部引用释放时触发 eviction 的机制。
Linux 通过 `iput_final()` 解决这个问题。

---

### 4.2 umount 路径不保证 inode 刷盘

**位置**: `syscall/umount.rs` → `fs/path/mod.rs:240-259`

umount 系统调用的实现：
```rust
// fs/path/mod.rs
pub fn unmount(&self, ctx: &Context) -> Result<Arc<Mount>> {
    let parent_mount = self.mount.parent().unwrap().upgrade().unwrap();
    let child_mount = parent_mount.do_unmount(&mountpoint)?;
    Ok(child_mount)
}
```

**问题**: 没有在 unmount 前调用 `fs.sync()`。
Linux 在 `generic_shutdown_super()` 中会调用 `sync_filesystem()`
确保所有脏数据和元数据写回磁盘。

**后果**: xfstest 完成后 umount 文件系统，
如果测试中有 unlink 但没有显式 sync，
脏 inode 和未释放的 bitmap 不会被刷盘，fsck 报告不一致。

---

## 5. 问题根因图

```
                        unlink / rmdir / rename-overwrite
                                    │
                    ┌───────────────┼───────────────┐
                    ▼               ▼               ▼
             links_count--    dtime=now()     is_freed=true
             persist to disk  persist to disk  (过早设置, §3.5)
                    │
                    ▼
            bitmap 不释放 ─────────────────────────────────┐
                                                           │
                    等待 sync_all_inodes()                  │
                    (仅由 sync/syncfs 触发, §3.1)           │
                    (umount 不触发, §4.2)                   │
                                                           │
                    ┌──────────────────────────────────────┘
                    ▼
            Phase 1: extract_if(strong_count==1)
                    │
            ┌───────┴───────┐
            ▼               ▼
      strong_count==1   strong_count>1
      (可 evict)        (跳过, §3.2)
            │               │
            ▼               ▼
      Phase 2: evict    bitmap 永不释放
            │
      ┌─────┴─────┐
      ▼           ▼
    成功        失败 (§3.3, §3.4)
      │           │
      ▼           ▼
  free_inode   inode 成为孤儿
  bitmap 释放  (已从缓存移除,
               bitmap 未释放)
```

---

## 6. 与 Linux ext2 的关键差异对比

| 机制 | Linux ext2 | Asterinas ext2 | 影响 |
|------|-----------|----------------|------|
| iput_final | 最后引用释放时立即 evict | 无等价机制 | bitmap 释放无保障 |
| orphan 列表 | `s_last_orphan` 链表 | 未实现 | 崩溃后无法恢复 |
| unlink 时设置 dtime | 不设置，仅在 evict 时设置 | unlink 时就设置 | 磁盘状态语义不一致 |
| umount sync | `generic_shutdown_super` 调用 `sync_filesystem` | 不调用 sync | 脏数据丢失 |
| eviction 触发 | `iput` → `iput_final` (引用计数驱动) | `sync_all_inodes` (显式调用驱动) | 时机不可控 |
| inode cache | 全局 hash table + LRU | per-group BTreeMap, 无 LRU | 无主动回收压力 |

---

## 7. 修复建议

### 7.1 【短期】umount 时强制 sync

在 unmount 路径中，detach mount 之前调用 `fs.sync()`。
这是最小改动，能解决大部分 xfstest 的 fsck 不一致问题。

**修改位置**: `fs/path/mod.rs` 的 `unmount()` 方法

```rust
pub fn unmount(&self, ctx: &Context) -> Result<Arc<Mount>> {
    // 新增: unmount 前强制 sync
    self.mount.fs().sync()?;
    let parent_mount = self.mount.parent().unwrap().upgrade().unwrap();
    let child_mount = parent_mount.do_unmount(&mountpoint)?;
    Ok(child_mount)
}
```

### 7.2 【短期】eviction 失败时回插缓存

修改 `sync_all_inodes()` 的 Phase 2，eviction 失败时将 inode 回插缓存，
避免产生孤儿 inode。

**修改位置**: `block_group.rs:324-329`

```rust
// Phase 2: evict removed inodes (without holding cache lock).
for inode in unused_inodes {
    match self.evict_inode(&inode) {
        Ok(result) => {
            evicted.freed_inodes += result.freed_inodes;
            evicted.freed_dirs += result.freed_dirs;
        }
        Err(e) => {
            // 回插缓存，避免孤儿 inode
            let inode_idx = (inode.ino() - 1) % self.inodes_per_group;
            self.inode_cache.write().insert(inode_idx, inode);
            log::warn!("evict_inode failed, re-inserted: {:?}", e);
        }
    }
}
```

### 7.3 【短期】unlink/rmdir 中不设置 `dtime` 和 `is_freed`

对齐 Linux 行为：unlink/rmdir 只递减 `links_count`，
`dtime` 和 `is_freed` 推迟到 `prepare_for_evict()` 中设置。

**修改位置**: `inode.rs:3598-3601` (unlink) 和 `inode.rs:1035-1039` (rmdir)

```rust
// unlink — 修改后
let mut child_inner = child.inner.write();
child_inner.desc.ctime = now();
child_inner.desc.links_count = child_inner.desc.links_count.saturating_sub(1);
// 不再设置 dtime 和 is_freed，交给 prepare_for_evict
child_inner.persist_inode_and_sync(&fs)?;
```

### 7.4 【中期】实现 `iput_final` 等价机制

在 Ext2 inode 的 `Arc` 引用计数降为 1（仅缓存持有）时，
如果 `links_count == 0`，立即触发 eviction。

**方案 A — 在 `InodeHandle::drop` 中检查**:

```rust
impl Drop for InodeHandle {
    fn drop(&mut self) {
        self.release_range_locks();
        let _ = self.unlock_flock();
        // 新增: 检查是否需要立即 evict
        if let Some(inode) = self.dentry.inode().downcast_ref::<ext2::Inode>() {
            inode.try_evict_if_dead();
        }
    }
}
```

**方案 B — 在 VFS `Inode` trait 中增加 `drop_inode` 回调**:

更通用的方案，在 VFS trait 中增加一个可选的 `drop_inode()` 方法，
由文件系统实现决定是否需要立即回收。这避免了 VFS 层对 Ext2 的硬编码依赖。

### 7.5 【长期】实现 orphan inode 列表

实现 Linux ext2 的 orphan inode 机制，保证崩溃恢复能力。

**核心改动**:

1. 在 `SuperBlock` 中增加 `s_last_orphan` 字段的读写支持
2. 在 unlink/rmdir 中，当 `nlink` 降为 0 时，将 inode 加入 orphan 链表
   （复用 `dtime` 字段存储下一个 orphan 的 ino）
3. 在 `prepare_for_evict()` 完成后，将 inode 从 orphan 链表移除
4. 在 `Ext2::mount()` 时，遍历 orphan 链表，清理所有残留 orphan

---

## 8. 修复优先级总结

| 优先级 | 问题 | 修复建议 | 预期效果 |
|--------|------|----------|----------|
| P0 | §4.2 umount 不 sync | 7.1 umount 前调用 sync | 解决大部分 xfstest fsck 失败 |
| P0 | §3.3 eviction 失败产生孤儿 | 7.2 失败时回插缓存 | 防止 inode 永久泄漏 |
| P0 | §3.1 缺乏 iput_final | 7.4 实现 drop_inode 回调 | bitmap 及时释放 |
| P1 | §3.5 unlink 过早设置 dtime | 7.3 推迟到 evict | 磁盘状态语义正确 |
| P1 | §3.6 竞态窗口 | 合并 Phase 1/2 或加标记 | 防止僵尸 inode |
| P1 | §3.7 计数双重递增风险 | 统一计数更新路径 | 防止 sb/gd 不一致 |
| P2 | §3.8 缺少 orphan 列表 | 7.5 实现 orphan 机制 | 崩溃恢复能力 |
