# Phase 11: 文件创建与删除

## 目标

实现文件、目录、符号链接的创建和删除。

## Module

```
ext2/
└── namei.rs         # 路径操作
    ├── create()     # 创建文件
    ├── mkdir()      # 创建目录
    ├── symlink()    # 创建符号链接
    ├── unlink()     # 删除文件
    └── rmdir()      # 删除目录
```

## Dependencies

| Asterinas 模块 | 用途 |
|----------------|------|
| `ialloc.rs` | 分配/释放 inode |
| `dir.rs` | 目录项操作 |

## Linux Reference

| 功能 | Linux 函数 | 文件:行号 |
|------|-----------|-----------|
| 创建文件 | `ext2_create` | `namei.c:100-130` |
| 创建目录 | `ext2_mkdir` | `namei.c:200-260` |
| 创建符号链接 | `ext2_symlink` | `namei.c:130-190` |
| 删除文件 | `ext2_unlink` | `namei.c:270-300` |
| 删除目录 | `ext2_rmdir` | `namei.c:300-340` |

## 关键逻辑索引

### 1. 创建文件流程

```
create(dir, name, mode)
  → ext2_new_inode() 分配 inode
  → 初始化 inode 属性
  → ext2_add_link() 添加目录项
```

### 2. 删除文件流程

```
unlink(dir, name)
  → ext2_find_entry() 查找目录项
  → ext2_delete_entry() 删除目录项
  → 减少 i_nlink
  → 如果 nlink=0，标记删除
```

## Verification

- [ ] 能创建普通文件
- [ ] 能创建目录
- [ ] 能创建符号链接
- [ ] 能删除文件
- [ ] 能删除空目录
