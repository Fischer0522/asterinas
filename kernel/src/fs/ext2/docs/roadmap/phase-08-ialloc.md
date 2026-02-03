# Phase 8: Inode 分配器

## 目标

实现 inode 的分配和释放，支持文件和目录创建。

## Module

```
ext2/
└── ialloc.rs        # inode 分配
    ├── alloc_inode()    # 分配 inode
    └── free_inode()     # 释放 inode
```

## Dependencies

| Asterinas 模块 | 用途 |
|----------------|------|
| `block_group.rs` | inode 位图访问 |
| `super_block.rs` | 空闲 inode 计数 |

## Linux Reference

| 功能 | Linux 函数 | 文件:行号 |
|------|-----------|-----------|
| 分配 inode | `ext2_new_inode` | `ialloc.c:400-580` |
| 释放 inode | `ext2_free_inode` | `ialloc.c:70-130` |
| 读取位图 | `read_inode_bitmap` | `ialloc.c:40-65` |

## 关键逻辑索引

### 1. Inode 分配策略 (`ialloc.c:400-450`)

```c
// 目录: 使用 Orlov 分配器，分散到不同块组
// 文件: 优先在父目录所在块组分配
if (S_ISDIR(mode))
    group = find_group_orlov(sb, dir);
else
    group = find_group_other(sb, dir);
```

## Asterinas Adaptation

| 问题 | Linux 做法 | Asterinas 做法 |
|------|-----------|----------------|
| Orlov 分配器 | 复杂启发式 | 简化版本 |
| 位图缓存 | `buffer_head` | `PageCache` |

### 特殊难点

1. **Orlov 分配器**: 可简化为优先父目录块组

## Verification

- [ ] 能分配新 inode
- [ ] 能释放 inode 并更新位图
- [ ] 正确更新空闲 inode 计数
