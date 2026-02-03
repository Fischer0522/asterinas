# Phase 3: Inode 读取

## 目标

实现 inode 的定位、读取和解析，支持所有文件类型的基本属性。

## Module

```
ext2/
└── inode.rs         # Inode 结构定义与读取
    ├── RawInode     # 磁盘结构 (128+ bytes)
    ├── InodeDesc    # 内存结构
    └── ext2_iget()  # 读取入口
```

## Dependencies

| Asterinas 模块 | 用途 |
|----------------|------|
| `block_group.rs` | 定位 inode table |
| `PageCache` | inode 数据缓存 |

## Linux Reference

| 功能 | Linux 函数 | 文件:行号 |
|------|-----------|-----------|
| Inode 磁盘结构 | `struct ext2_inode` | `ext2.h:290-342` |
| Inode 内存结构 | `struct ext2_inode_info` | `ext2.h:632-680` |
| 获取原始 inode | `ext2_get_inode` | `inode.c:1358-1392` |
| 读取 inode | `ext2_iget` | `inode.c:1394-1510` |
| 快速符号链接判断 | `ext2_inode_is_fast_symlink` | `inode.c:48-55` |
| 32位 UID/GID | `ext2_iget` | `inode.c:1414-1421` |

## 关键逻辑索引

### 1. Inode 定位 (`inode.c:1358-1392`)

```c
// 计算 inode 所在块组
block_group = (ino - 1) / EXT2_INODES_PER_GROUP(sb);
// 计算组内偏移
offset = ((ino - 1) % EXT2_INODES_PER_GROUP(sb)) * EXT2_INODE_SIZE(sb);
// 计算所在块
block = le32_to_cpu(gdp->bg_inode_table) + (offset >> EXT2_BLOCK_SIZE_BITS(sb));
```

### 2. 删除 inode 检测 (`inode.c:1433-1437`)

```c
if (inode->i_nlink == 0 && (inode->i_mode == 0 || ei->i_dtime)) {
    ret = -ESTALE;
    goto bad_inode;
}
```

### 3. LARGE_FILE 处理 (`inode.c:1455-1458`)

```c
if (S_ISREG(inode->i_mode))
    inode->i_size |= ((__u64)le32_to_cpu(raw_inode->i_size_high)) << 32;
else
    ei->i_dir_acl = le32_to_cpu(raw_inode->i_dir_acl);
```

### 4. 快速符号链接 (`inode.c:48-55`)

```c
static inline int ext2_inode_is_fast_symlink(struct inode *inode)
{
    int ea_blocks = EXT2_I(inode)->i_file_acl ?
        (inode->i_sb->s_blocksize >> 9) : 0;
    return (S_ISLNK(inode->i_mode) && inode->i_blocks - ea_blocks == 0);
}
```

### 5. 32位 UID/GID (`inode.c:1414-1421`)

```c
i_uid = (uid_t)le16_to_cpu(raw_inode->i_uid_low);
i_gid = (gid_t)le16_to_cpu(raw_inode->i_gid_low);
if (!(test_opt(inode->i_sb, NO_UID32))) {
    i_uid |= le16_to_cpu(raw_inode->i_uid_high) << 16;
    i_gid |= le16_to_cpu(raw_inode->i_gid_high) << 16;
}
```

## Asterinas Adaptation

| 问题 | Linux 做法 | Asterinas 做法 |
|------|-----------|----------------|
| Inode 缓存 | `inode_hashtable` | Asterinas VFS inode cache |
| i_data 存储 | `__le32 i_data[15]` | `[u32; 15]` |
| 文件类型判断 | `S_ISREG()` 宏 | `InodeType` enum |

### 特殊难点

1. **Inode size 由 rev_level 决定** (非 Feature Flag):

   | Rev Level | inode_size | 说明 |
   |-----------|------------|------|
   | `GOOD_OLD_REV` (0) | 固定 128 字节 | 忽略 `s_inode_size` 字段 |
   | `DYNAMIC_REV` (1) | 从 `s_inode_size` 读取 | 常见值: 128, 256 |

   **影响 inode 定位计算**:
   ```c
   offset = ((ino - 1) % inodes_per_group) * inode_size;
   ```

2. **i_blocks 单位**: 512 字节扇区，不是文件系统块

## Verification

- [ ] 能读取 root inode (ino=2)
- [ ] 正确解析 mode, uid, gid, size, timestamps
- [ ] 正确处理 LARGE_FILE (size > 4GB)
- [ ] 正确识别快速符号链接
- [ ] 拒绝已删除的 inode (返回 ESTALE)
