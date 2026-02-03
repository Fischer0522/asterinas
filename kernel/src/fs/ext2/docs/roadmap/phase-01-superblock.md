# Phase 1: 超级块读取与验证

## 目标

实现超级块的读取、解析和验证，这是挂载 Ext2 文件系统的第一步。

## Module

```
ext2/
├── mod.rs           # 模块入口，定义 BLOCK_SIZE = 4096
├── prelude.rs       # 公共导入
└── super_block.rs   # 超级块定义与解析
```

## Dependencies

| Asterinas 模块 | 用途 |
|----------------|------|
| `aster_block` | 块设备读取 |
| `ostd::Pod` | 安全的磁盘结构转换 |
| `bitflags` | Feature flags 定义 |

## Linux Reference

| 功能 | Linux 函数 | 文件:行号 |
|------|-----------|-----------|
| 超级块结构 | `struct ext2_super_block` | `ext2.h:411-481` |
| 魔数验证 | `ext2_fill_super` | `super.c:950-951` |
| Block size 验证 | `ext2_fill_super` | `super.c:985-991` |
| Feature 检查 | `EXT2_HAS_INCOMPAT_FEATURE` | `super.c:971-983` |
| Rev level 处理 | `ext2_fill_super` | `super.c:1036-1050` |
| 派生值计算 | `ext2_fill_super` | `super.c:1052-1067` |

## 关键逻辑索引

### 1. 魔数验证 (`super.c:950`)

```c
if (sb->s_magic != EXT2_SUPER_MAGIC)
    goto cantfind_ext2;
```

### 2. Block size 验证 (`super.c:985-991`)

```c
if (le32_to_cpu(es->s_log_block_size) >
    (EXT2_MAX_BLOCK_LOG_SIZE - BLOCK_SIZE_BITS)) {
    // Invalid log block size
    goto failed_mount;
}
blocksize = BLOCK_SIZE << le32_to_cpu(sbi->s_es->s_log_block_size);
```

### 3. Feature 检查 (`super.c:971-983`)

```c
// INCOMPAT 未知标志 → 拒绝挂载
features = EXT2_HAS_INCOMPAT_FEATURE(sb, ~EXT2_FEATURE_INCOMPAT_SUPP);
if (features) {
    goto failed_mount;
}
// RO_COMPAT 未知标志 + 读写挂载 → 拒绝挂载
if (!sb_rdonly(sb) &&
    (features = EXT2_HAS_RO_COMPAT_FEATURE(sb, ~EXT2_FEATURE_RO_COMPAT_SUPP))) {
    goto failed_mount;
}
```

### 4. Rev level 处理 (`super.c:1036-1050`)

**重要**: `inode_size` 变长不是由 Feature Flag 决定，而是由 `s_rev_level` 决定。

| Rev Level | 值 | inode_size | first_ino | s_inode_size 字段 |
|-----------|-----|------------|-----------|-------------------|
| `GOOD_OLD_REV` | 0 | 固定 128 字节 | 固定 11 | 忽略 |
| `DYNAMIC_REV` | 1 | 从超级块读取 | 从超级块读取 | 有效 |

```c
if (le32_to_cpu(es->s_rev_level) == EXT2_GOOD_OLD_REV) {
    sbi->s_inode_size = EXT2_GOOD_OLD_INODE_SIZE;  // 128
    sbi->s_first_ino = EXT2_GOOD_OLD_FIRST_INO;    // 11
} else {
    sbi->s_inode_size = le16_to_cpu(es->s_inode_size);
    sbi->s_first_ino = le32_to_cpu(es->s_first_ino);
    // 验证 inode_size: >= 128, 是 2 的幂, <= blocksize
}
```

**inode_size 验证条件** (仅 DYNAMIC_REV):
- `>= EXT2_GOOD_OLD_INODE_SIZE` (128)
- 必须是 2 的幂
- `<= blocksize` (对于 4KB 块，最大 4096)

### 5. 派生值计算 (`super.c:1052-1067`)

```c
sbi->s_inodes_per_block = sb->s_blocksize / EXT2_INODE_SIZE(sb);
sbi->s_itb_per_group = sbi->s_inodes_per_group / sbi->s_inodes_per_block;
sbi->s_desc_per_block = sb->s_blocksize / sizeof(struct ext2_group_desc);
sbi->s_addr_per_block_bits = ilog2(EXT2_ADDR_PER_BLOCK(sb));
sbi->s_desc_per_block_bits = ilog2(EXT2_DESC_PER_BLOCK(sb));
```

## Asterinas Adaptation

| 问题 | Linux 做法 | Asterinas 做法 |
|------|-----------|----------------|
| 磁盘结构读取 | `buffer_head` + 指针转换 | `Pod` trait + `read_val()` |
| Block size | 运行时可变 | **编译时常量 4096**，挂载时验证 |
| Feature flags | 宏 + 位运算 | `bitflags!` crate |
| 错误处理 | `goto failed_mount` | `Result<T, Error>` |

### 特殊难点

1. **Block size 强制 4KB**: 需要在挂载时检查 `s_log_block_size == 2`，否则返回 `EINVAL`

2. **GOOD_OLD_REV 支持**: ext2.old 拒绝了此版本，需要修复以支持旧格式文件系统

3. **超级块位置**: 超级块始终位于字节偏移 1024，对于 4KB 块，它在 block 0 的偏移 1024 处

## Verification

- [ ] 能读取并解析 4KB block size 的 ext2 镜像超级块
- [ ] 正确打印 UUID、Volume Name、blocks_count、inodes_count
- [ ] 拒绝非 4KB block size 的镜像 (返回 EINVAL)
- [ ] 拒绝含未知 INCOMPAT feature 的镜像
- [ ] 正确处理 GOOD_OLD_REV 和 DYNAMIC_REV
