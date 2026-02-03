# Phase 2: 块组描述符表

## 目标

实现块组描述符表的加载和验证，为后续的 inode 和块分配提供基础。

## Module

```
ext2/
└── block_group.rs   # 块组描述符定义与加载
```

## Dependencies

| Asterinas 模块 | 用途 |
|----------------|------|
| `super_block.rs` | 获取块组数量、描述符位置 |
| `aster_block` | 读取描述符块 |

## Linux Reference

| 功能 | Linux 函数 | 文件:行号 |
|------|-----------|-----------|
| 描述符结构 | `struct ext2_group_desc` | `ext2.h:191-201` |
| 获取描述符 | `ext2_get_group_desc` | `balloc.c:39-69` |
| 描述符位置 | `descriptor_loc` | `super.c:801-816` |
| 描述符验证 | `ext2_check_descriptors` | `super.c:700-737` |
| 块组数量计算 | `ext2_fill_super` | `super.c:1113-1115` |

## 关键逻辑索引

### 1. 块组数量计算 (`super.c:1113-1115`)

```c
sbi->s_groups_count = ((le32_to_cpu(es->s_blocks_count) -
            le32_to_cpu(es->s_first_data_block) - 1)
                / EXT2_BLOCKS_PER_GROUP(sb)) + 1;
```

### 2. 描述符表位置 (`super.c:801-816`)

```c
// 非 META_BG 模式：描述符紧跟超级块
if (!EXT2_HAS_INCOMPAT_FEATURE(sb, EXT2_FEATURE_INCOMPAT_META_BG) ||
    nr < first_meta_bg)
    return (logic_sb_block + nr + 1);
```

### 3. 描述符验证 (`super.c:700-737`)

```c
// 检查 bitmap 和 inode table 是否在块组范围内
if (le32_to_cpu(gdp->bg_block_bitmap) < first_block ||
    le32_to_cpu(gdp->bg_block_bitmap) > last_block) {
    ext2_error(sb, "ext2_check_descriptors",
        "Block bitmap for group %d not in group (block %lu)!", i, ...);
    return 0;
}
```

### 4. 获取描述符 (`balloc.c:39-69`)

```c
group_desc = block_group >> EXT2_DESC_PER_BLOCK_BITS(sb);
offset = block_group & (EXT2_DESC_PER_BLOCK(sb) - 1);
desc = (struct ext2_group_desc *) sbi->s_group_desc[group_desc]->b_data;
return desc + offset;
```

## Asterinas Adaptation

| 问题 | Linux 做法 | Asterinas 做法 |
|------|-----------|----------------|
| 描述符缓存 | `buffer_head **s_group_desc` 数组 | `Vec<BlockGroupDesc>` |
| 并发访问 | `bgl_lock` 每块组锁 | `RwLock<BlockGroupDesc>` |
| 描述符定位 | 位运算 + 指针 | 方法封装 `get_group_desc(idx)` |

### 特殊难点

1. **META_BG 不支持**: 需要检查并拒绝含 `META_BG` feature 的镜像
2. **描述符数量**: 4KB block = 128 个描述符/块 (32 bytes each)

## Verification

- [ ] 正确计算块组数量
- [ ] 能加载所有块组描述符
- [ ] 验证每个描述符的 bitmap/inode_table 位置合法
- [ ] 能打印每个块组的 free_blocks_count 和 free_inodes_count
