# Phase 9: 文件写入与截断

## 目标

实现文件内容写入和截断操作。

## Module

```
ext2/
└── file.rs          # 文件写入 (扩展 Phase 6)
    ├── write_at()       # 写入数据
    └── truncate()       # 截断文件
```

## Dependencies

| Asterinas 模块 | 用途 |
|----------------|------|
| `balloc.rs` | 分配数据块 |
| `block_ptr.rs` | 更新块指针 |

## Linux Reference

| 功能 | Linux 函数 | 文件:行号 |
|------|-----------|-----------|
| 块分配 | `ext2_alloc_branch` | `inode.c:360-430` |
| 截断 | `ext2_truncate_blocks` | `inode.c:1150-1300` |
| 间接块释放 | `ext2_free_branches` | `inode.c:1050-1140` |

## 关键逻辑索引

### 1. 分配间接块链 (`inode.c:360-430`)

```c
// 从断点处开始分配新块
ext2_alloc_branch(inode, indirect_blks, offsets, partial);
```

### 2. 截断流程

```
truncate(new_size)
  → 计算需要保留的块数
  → 释放多余的直接块
  → 递归释放间接块
  → 更新 i_size 和 i_blocks
```

## Asterinas Adaptation

| 问题 | Linux 做法 | Asterinas 做法 |
|------|-----------|----------------|
| 写入同步 | `truncate_mutex` | `RwLock` |
| 页缓存回写 | `writeback` | `PageCache::flush()` |

### 特殊难点

1. **间接块分配**: 需要原子性地分配整条链
2. **截断一致性**: 先释放数据块，再释放间接块

## Verification

- [ ] 能写入小文件
- [ ] 能写入大文件 (触发间接块分配)
- [ ] 能截断文件
- [ ] 截断后正确释放块
