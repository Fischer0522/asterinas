# Phase 7: 块分配器

## 目标

实现块的分配和释放，支持文件写入和扩展。

## Module

```
ext2/
└── balloc.rs        # 块分配
    ├── alloc_block()    # 分配单个块
    ├── alloc_blocks()   # 批量分配
    └── free_blocks()    # 释放块
```

## Dependencies

| Asterinas 模块 | 用途 |
|----------------|------|
| `block_group.rs` | 块位图访问 |
| `super_block.rs` | 空闲块计数 |

## Linux Reference

| 功能 | Linux 函数 | 文件:行号 |
|------|-----------|-----------|
| 分配块 | `ext2_new_blocks` | `balloc.c:1200-1400` |
| 释放块 | `ext2_free_blocks` | `balloc.c:280-380` |
| 读取位图 | `read_block_bitmap` | `balloc.c:128-170` |
| 位图验证 | `ext2_valid_block_bitmap` | `balloc.c:71-120` |

## 关键逻辑索引

### 1. 块分配策略

```c
// 优先在目标块组分配，失败则搜索其他块组
goal_group = (goal - le32_to_cpu(es->s_first_data_block)) /
             EXT2_BLOCKS_PER_GROUP(sb);
```

### 2. 位图操作

```c
// 查找空闲位
bit = ext2_find_next_zero_bit(bitmap, end, start);
// 设置位
ext2_set_bit(bit, bitmap);
```

## Asterinas Adaptation

| 问题 | Linux 做法 | Asterinas 做法 |
|------|-----------|----------------|
| 位图缓存 | `buffer_head` | `PageCache` |
| 位操作 | 内核位操作宏 | `bitvec` 或手写 |
| 并发控制 | `bgl_lock` | `RwLock` |

### 特殊难点

1. **预留窗口**: 暂不支持，使用简单的首次适配算法
2. **元数据保护**: 不能分配已被位图/inode表占用的块

## Verification

- [ ] 能分配单个块
- [ ] 能批量分配连续块
- [ ] 能释放块并更新位图
- [ ] 正确更新空闲块计数
