# Phase 6: 文件读取与符号链接

## 目标

实现文件内容读取和符号链接解析，完成只读挂载的全部功能。

## Module

```
ext2/
├── file.rs          # 文件读取
└── symlink.rs       # 符号链接处理
```

## Dependencies

| Asterinas 模块 | 用途 |
|----------------|------|
| `block_ptr.rs` | 数据块定位 |
| `PageCache` | 文件数据缓存 |

## Linux Reference

| 功能 | Linux 函数 | 文件:行号 |
|------|-----------|-----------|
| 文件读取 | `ext2_file_read_iter` | `file.c` (使用通用实现) |
| 块映射 | `ext2_get_block` | `inode.c:744-870` |
| 快速符号链接 | `ext2_inode_is_fast_symlink` | `inode.c:48-55` |
| 符号链接读取 | `ext2_symlink_inode_operations` | `symlink.c` |

## 关键逻辑索引

### 1. 快速符号链接判断 (`inode.c:48-55`)

```c
static inline int ext2_inode_is_fast_symlink(struct inode *inode)
{
    int ea_blocks = EXT2_I(inode)->i_file_acl ?
        (inode->i_sb->s_blocksize >> 9) : 0;
    return (S_ISLNK(inode->i_mode) &&
            inode->i_blocks - ea_blocks == 0);
}
```

### 2. 快速符号链接数据位置 (`inode.c:1484`)

```c
inode->i_link = (char *)ei->i_data;
// 链接目标直接存储在 i_block[0..14] 中，最多 60 字节
```

### 3. 文件读取流程

```
read_at(offset, buf)
  → 计算起始逻辑块号: offset / BLOCK_SIZE
  → 对每个逻辑块:
      → block_to_path() 获取路径
      → get_branch() 获取物理块号
      → 从 PageCache 读取数据
```

## Asterinas Adaptation

| 问题 | Linux 做法 | Asterinas 做法 |
|------|-----------|----------------|
| 文件读取 | `generic_file_read_iter` | VFS `read_at` trait |
| 页缓存 | `address_space` | `PageCache` |
| 符号链接 | `i_link` 指针 | 直接从 `i_data` 解析 |

### 特殊难点

1. **快速符号链接**: 目标存储在 i_block 中，需要特殊处理
2. **稀疏文件**: 空洞区域读取返回零

## Verification

- [ ] 能读取小文件 (直接块)
- [ ] 能读取大文件 (间接块)
- [ ] 能解析快速符号链接
- [ ] 能解析普通符号链接
- [ ] 稀疏文件空洞返回零
