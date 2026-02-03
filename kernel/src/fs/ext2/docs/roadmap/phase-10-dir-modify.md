# Phase 10: 目录项创建与删除

## 目标

实现目录项的添加和删除操作。

## Module

```
ext2/
└── dir.rs           # 目录修改 (扩展 Phase 5)
    ├── add_entry()      # 添加目录项
    └── delete_entry()   # 删除目录项
```

## Dependencies

| Asterinas 模块 | 用途 |
|----------------|------|
| `balloc.rs` | 目录块扩展 |
| `inode.rs` | 更新目录 inode |

## Linux Reference

| 功能 | Linux 函数 | 文件:行号 |
|------|-----------|-----------|
| 添加目录项 | `ext2_add_link` | `dir.c:430-530` |
| 删除目录项 | `ext2_delete_entry` | `dir.c:560-600` |
| 空目录检查 | `ext2_empty_dir` | `dir.c:620-670` |

## 关键逻辑索引

### 1. 添加目录项 (`dir.c:430-530`)

```c
// 查找空闲空间或扩展目录
// rec_len 可能大于实际需要，用于合并空洞
```

### 2. 删除目录项

```c
// 将 inode 设为 0，合并到前一个条目的 rec_len
de->inode = 0;
pde->rec_len += de->rec_len;
```

## Asterinas Adaptation

| 问题 | Linux 做法 | Asterinas 做法 |
|------|-----------|----------------|
| 目录锁 | `i_rwsem` | `RwLock` |

## Verification

- [ ] 能添加新目录项
- [ ] 能删除目录项
- [ ] 正确处理目录扩展
