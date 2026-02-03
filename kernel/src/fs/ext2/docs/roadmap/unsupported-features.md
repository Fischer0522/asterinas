# 暂不支持的特性

本文档记录所有暂不支持但为未来预留的 Ext2 特性。

## Feature Flags

### COMPAT (兼容特性)

| Flag | 值 | 说明 | 状态 |
|------|-----|------|------|
| `DIR_PREALLOC` | 0x0001 | 目录块预分配 | 暂不支持 |
| `IMAGIC_INODES` | 0x0002 | AFS 服务器 inode | 暂不支持 |
| `HAS_JOURNAL` | 0x0004 | Ext3 日志 | 暂不支持 |
| `RESIZE_INO` | 0x0010 | 在线扩容 | 暂不支持 |
| `DIR_INDEX` | 0x0020 | HTree 目录索引 | 暂不支持 |

### RO_COMPAT (只读兼容特性)

| Flag | 值 | 说明 | 状态 |
|------|-----|------|------|
| `BTREE_DIR` | 0x0004 | B树目录 | 暂不支持 |

### INCOMPAT (不兼容特性)

| Flag | 值 | 说明 | 状态 |
|------|-----|------|------|
| `COMPRESSION` | 0x0001 | 压缩 | 永不支持 |
| `RECOVER` | 0x0004 | 需要日志恢复 | 暂不支持 |
| `JOURNAL_DEV` | 0x0008 | 日志设备 | 暂不支持 |
| `META_BG` | 0x0010 | 元块组布局 | 暂不支持 |

## Mount Options

| 选项 | 说明 | 状态 |
|------|------|------|
| `RESERVATION` | 预留窗口分配 | 暂不支持 |
| `DAX` | 直接访问 | 暂不支持 |
| `USRQUOTA` | 用户配额 | 暂不支持 |
| `GRPQUOTA` | 组配额 | 暂不支持 |
| `POSIX_ACL` | POSIX ACL | 暂不支持 |
| `OLDALLOC` | 旧分配器 | 暂不支持 |
| `GRPID` | BSD 组语义 | 暂不支持 |
| `NO_UID32` | 禁用 32位 UID | 暂不支持 |

## 其他限制

| 限制 | 说明 | 原因 |
|------|------|------|
| Block Size | 仅支持 4KB | 简化实现，与 PageCache 对齐 |
| Creator OS | 仅 Linux | 其他 OS 的 inode 布局不同 |
| 错误处理 | 仅 ERRORS_CONTINUE | 简化实现 |

## Linux 源码参考

Feature 定义位置: `ext2.h:526-554`

```c
#define EXT2_FEATURE_COMPAT_SUPP    EXT2_FEATURE_COMPAT_EXT_ATTR
#define EXT2_FEATURE_INCOMPAT_SUPP  (EXT2_FEATURE_INCOMPAT_FILETYPE| \
                                     EXT2_FEATURE_INCOMPAT_META_BG)
#define EXT2_FEATURE_RO_COMPAT_SUPP (EXT2_FEATURE_RO_COMPAT_SPARSE_SUPER| \
                                     EXT2_FEATURE_RO_COMPAT_LARGE_FILE| \
                                     EXT2_FEATURE_RO_COMPAT_BTREE_DIR)
```
