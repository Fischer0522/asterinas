# Phase 12: 扩展属性

## 目标

实现扩展属性 (xattr) 的读取和写入。

## Module

```
ext2/
└── xattr.rs         # 扩展属性
    ├── get_xattr()      # 读取属性
    └── set_xattr()      # 设置属性
```

## Dependencies

| Asterinas 模块 | 用途 |
|----------------|------|
| `inode.rs` | i_file_acl 字段 |
| `balloc.rs` | 属性块分配 |

## Linux Reference

| 功能 | Linux 函数 | 文件:行号 |
|------|-----------|-----------|
| 读取属性 | `ext2_xattr_get` | `xattr.c:200-280` |
| 设置属性 | `ext2_xattr_set` | `xattr.c:400-600` |
| 属性块结构 | `struct ext2_xattr_header` | `xattr.h:30-50` |

## 关键逻辑索引

### 1. 属性块位置

```c
// 属性块号存储在 i_file_acl 字段
block = EXT2_I(inode)->i_file_acl;
```

### 2. 属性块布局

```
+------------------+
| xattr_header     |  (magic, refcount, blocks, hash)
+------------------+
| xattr_entry 1    |  (name_index, name_len, value_offs, value_size)
| xattr_entry 2    |
| ...              |
+------------------+
| (空闲空间)        |
+------------------+
| value 2          |
| value 1          |  (值从块尾部向前存储)
+------------------+
```

## Asterinas Adaptation

| 问题 | Linux 做法 | Asterinas 做法 |
|------|-----------|----------------|
| 属性缓存 | `mb_cache` | 简化实现，不共享 |

## Verification

- [ ] 能读取扩展属性
- [ ] 能设置扩展属性
- [ ] 能删除扩展属性
