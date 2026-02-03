# Phase 4: 块寻址 (间接块)

## 目标

实现从逻辑块号到物理块号的转换，支持直接块、一级/二级/三级间接块。

## Module

```
ext2/
└── block_ptr.rs     # 块指针与间接块逻辑
    ├── block_to_path()   # 逻辑块号 → 路径
    ├── get_branch()      # 路径 → 物理块号
    └── Indirect          # 间接块链结构
```

## Dependencies

| Asterinas 模块 | 用途 |
|----------------|------|
| `inode.rs` | 获取 i_data[15] |
| `PageCache` | 间接块缓存 |

## Linux Reference

| 功能 | Linux 函数 | 文件:行号 |
|------|-----------|-----------|
| 块号转路径 | `ext2_block_to_path` | `inode.c:163-203` |
| 读取间接块链 | `ext2_get_branch` | `inode.c:234-272` |
| 间接块结构 | `typedef Indirect` | `inode.c:114-118` |
| 链验证 | `verify_chain` | `inode.c:126-131` |

## 关键逻辑索引

### 1. 块号转路径 (`inode.c:163-203`)

```c
// 4KB block: ptrs = 1024, ptrs_bits = 10
int ptrs = EXT2_ADDR_PER_BLOCK(inode->i_sb);  // 4096/4 = 1024
int ptrs_bits = EXT2_ADDR_PER_BLOCK_BITS(inode->i_sb);  // 10

if (i_block < direct_blocks) {           // 0-11: 直接块
    offsets[n++] = i_block;
} else if ((i_block -= 12) < 1024) {     // 12-1035: 一级间接
    offsets[n++] = EXT2_IND_BLOCK;       // 12
    offsets[n++] = i_block;
} else if ((i_block -= 1024) < 1024*1024) {  // 二级间接
    offsets[n++] = EXT2_DIND_BLOCK;      // 13
    offsets[n++] = i_block >> 10;
    offsets[n++] = i_block & 1023;
} else {                                  // 三级间接
    offsets[n++] = EXT2_TIND_BLOCK;      // 14
    // ...
}
```

### 2. 读取间接块链 (`inode.c:234-272`)

```c
add_chain(chain, NULL, EXT2_I(inode)->i_data + *offsets);
if (!p->key)
    goto no_block;
while (--depth) {
    bh = sb_bread(sb, le32_to_cpu(p->key));
    add_chain(++p, bh, (__le32*)bh->b_data + *++offsets);
    if (!p->key)
        goto no_block;
}
```

### 3. 4KB 块的寻址能力

| 级别 | 块范围 | 文件大小范围 |
|------|--------|--------------|
| 直接块 | 0-11 | 0-48KB |
| 一级间接 | 12-1035 | 48KB-4MB |
| 二级间接 | 1036-1049611 | 4MB-4GB |
| 三级间接 | 1049612+ | 4GB-4TB |

## Asterinas Adaptation

| 问题 | Linux 做法 | Asterinas 做法 |
|------|-----------|----------------|
| 间接块缓存 | `buffer_head` | `PageCache` |
| 链验证 | `verify_chain` + 锁 | `RwLock` 保护 |
| 路径表示 | `int offsets[4]` | `Vec<usize>` 或 `[usize; 4]` |

### 特殊难点

1. **并发安全**: 读取间接块链时需要防止并发修改
2. **稀疏文件**: 块号为 0 表示空洞，读取返回零

## Verification

- [ ] 能正确解析直接块 (0-11)
- [ ] 能正确解析一级间接块
- [ ] 能正确解析二级间接块
- [ ] 能处理稀疏文件 (块号为 0)
