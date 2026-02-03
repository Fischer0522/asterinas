# Ext2 重写 - 增量式功能路线图

## 项目概述

本文档定义了 Asterinas Ext2 文件系统的完全重写计划，采用增量式、功能驱动的开发方式。

## 设计决策

| 项目 | 决策 |
|------|------|
| 重写方式 | 完全重写，参考 ext2.old 风格，严禁 unsafe |
| Feature 范围 | Phase 1 (只读) + Phase 2 (读写) |
| Block Size | **固定 4KB**，拒绝其他大小 |
| 错误处理 | 仅 `ERRORS_CONTINUE` |

## 支持的 Feature Flags

| Feature | 类型 | 说明 |
|---------|------|------|
| `SPARSE_SUPER` | RO_COMPAT | 稀疏超级块备份 |
| `LARGE_FILE` | RO_COMPAT | 64位文件大小 |
| `FILETYPE` | INCOMPAT | 目录项含类型字段 |
| `EXT_ATTR` | COMPAT | 扩展属性 |

## 暂不支持的特性

详见 [unsupported-features.md](./unsupported-features.md)

## 模块结构

```
ext2/
├── mod.rs           # 模块入口
├── prelude.rs       # 公共导入
├── super_block.rs   # 超级块
├── block_group.rs   # 块组描述符
├── inode.rs         # inode 核心
├── block_ptr.rs     # 块寻址 (间接块)
├── dir.rs           # 目录操作
├── balloc.rs        # 块分配
├── ialloc.rs        # inode 分配
├── namei.rs         # 路径解析
├── file.rs          # 文件读写
├── symlink.rs       # 符号链接
├── xattr.rs         # 扩展属性
└── impl_for_vfs/    # VFS 适配层
```

## Phase 列表

### 只读挂载 (Phase 1-6)

| Phase | 名称 | 文档 |
|-------|------|------|
| 1 | 超级块读取与验证 | [phase-01-superblock.md](./phase-01-superblock.md) |
| 2 | 块组描述符表 | [phase-02-block-group.md](./phase-02-block-group.md) |
| 3 | Inode 读取 | [phase-03-inode-read.md](./phase-03-inode-read.md) |
| 4 | 块寻址 (间接块) | [phase-04-block-ptr.md](./phase-04-block-ptr.md) |
| 5 | 目录遍历与查找 | [phase-05-directory.md](./phase-05-directory.md) |
| 6 | 文件读取与符号链接 | [phase-06-file-read.md](./phase-06-file-read.md) |

### 读写支持 (Phase 7-12)

| Phase | 名称 | 文档 |
|-------|------|------|
| 7 | 块分配器 | [phase-07-balloc.md](./phase-07-balloc.md) |
| 8 | Inode 分配器 | [phase-08-ialloc.md](./phase-08-ialloc.md) |
| 9 | 文件写入与截断 | [phase-09-file-write.md](./phase-09-file-write.md) |
| 10 | 目录项创建与删除 | [phase-10-dir-modify.md](./phase-10-dir-modify.md) |
| 11 | 文件创建与删除 | [phase-11-namei.md](./phase-11-namei.md) |
| 12 | 扩展属性 | [phase-12-xattr.md](./phase-12-xattr.md) |

## 验收标准

### Phase 1 里程碑 (只读挂载)

- [ ] 能挂载 4KB block size 的 ext2 镜像
- [ ] 能读取根目录
- [ ] 能 lookup 文件
- [ ] 能读取文件内容
- [ ] 能处理符号链接

### Phase 2 里程碑 (读写支持)

- [ ] 能创建/删除文件和目录
- [ ] 能写入文件内容
- [ ] 能正确处理文件截断
- [ ] 能设置/获取扩展属性
- [ ] 元数据正确持久化

## Linux 源码参考

本项目参考的 Linux 内核源码位于 `/root/linux/fs/ext2/`，主要文件：

| 文件 | 功能 |
|------|------|
| `ext2.h` | 数据结构定义 |
| `super.c` | 超级块与挂载 |
| `balloc.c` | 块分配 |
| `ialloc.c` | inode 分配 |
| `inode.c` | inode 操作 |
| `dir.c` | 目录操作 |
| `namei.c` | 路径解析 |
| `file.c` | 文件操作 |
| `xattr.c` | 扩展属性 |
