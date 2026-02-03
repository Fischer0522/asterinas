# Phase 5: 目录遍历与查找

## 目标

实现目录项的遍历和文件名查找，支持 FILETYPE 特性。

## Module

```
ext2/
└── dir.rs           # 目录操作
    ├── DirEntry     # 目录项结构
    ├── readdir()    # 目录遍历
    └── find_entry() # 文件名查找
```

## Dependencies

| Asterinas 模块 | 用途 |
|----------------|------|
| `inode.rs` | 目录 inode |
| `block_ptr.rs` | 读取目录数据块 |

## Linux Reference

| 功能 | Linux 函数 | 文件:行号 |
|------|-----------|-----------|
| 目录项结构 | `struct ext2_dir_entry_2` | `ext2.h:592-598` |
| 目录遍历 | `ext2_readdir` | `dir.c:256-323` |
| 文件名查找 | `ext2_find_entry` | `dir.c:342-400` |
| rec_len 处理 | `ext2_rec_len_from_disk` | `dir.c:38-47` |
| 目录项验证 | `ext2_check_folio` | `dir.c:99-170` |

## 关键逻辑索引

### 1. 目录项结构 (`ext2.h:592-598`)

```c
struct ext2_dir_entry_2 {
    __le32  inode;      /* Inode number */
    __le16  rec_len;    /* Directory entry length */
    __u8    name_len;   /* Name length */
    __u8    file_type;  /* File type (if FILETYPE feature) */
    char    name[];     /* File name */
};
```

### 2. 目录遍历 (`dir.c:256-323`)

```c
for ( ; n < npages; n++, offset = 0) {
    kaddr = ext2_get_folio(inode, n, 0, &folio);
    de = (ext2_dirent *)(kaddr + offset);
    for ( ; (char*)de <= limit; de = ext2_next_entry(de)) {
        if (de->inode) {
            dir_emit(ctx, de->name, de->name_len,
                     le32_to_cpu(de->inode), d_type);
        }
    }
}
```

### 3. rec_len 计算 (`ext2.h:607-608`)

```c
#define EXT2_DIR_REC_LEN(name_len) \
    (((name_len) + 8 + EXT2_DIR_ROUND) & ~EXT2_DIR_ROUND)
// 8 = sizeof(inode) + sizeof(rec_len) + sizeof(name_len) + sizeof(file_type)
// EXT2_DIR_ROUND = 3 (4字节对齐)
```

### 4. FILETYPE 处理 (`dir.c:272-273`)

```c
has_filetype = EXT2_HAS_INCOMPAT_FEATURE(sb, EXT2_FEATURE_INCOMPAT_FILETYPE);
```

## Asterinas Adaptation

| 问题 | Linux 做法 | Asterinas 做法 |
|------|-----------|----------------|
| 目录数据读取 | `folio` + `kmap` | `PageCache` |
| 迭代器 | 指针遍历 | Rust 迭代器 |
| 文件类型 | `file_type` 字段 | `InodeType` enum |

### 特殊难点

1. **变长目录项**: rec_len 可能大于实际需要，用于填充空洞
2. **删除标记**: inode=0 表示已删除的目录项

## Verification

- [ ] 能遍历根目录所有条目
- [ ] 能通过文件名查找 inode 号
- [ ] 正确处理 FILETYPE 特性
- [ ] 正确跳过已删除的目录项
