[PROMPT]
Provide additions to `kernel/src/fs/ext2/inode.rs` and `kernel/src/fs/ext2/impl_for_vfs/inode.rs`.
Output Rust code only. No unsafe.
All functions must be methods in `impl` blocks.
Implementation logic MUST follow [SOURCE] Linux code.

[SOURCE]
ext2_iget (device read)       → fs/ext2/inode.c:1493-1500
__ext2_write_inode (device write) → fs/ext2/inode.c:1589-1599
old_valid_dev                 → include/linux/kdev_t.h:24-27
old_encode_dev                → include/linux/kdev_t.h:29-32
old_decode_dev                → include/linux/kdev_t.h:34-37
new_encode_dev                → include/linux/kdev_t.h:39-44
new_decode_dev                → include/linux/kdev_t.h:46-51

[RELY]
```rust
use super::prelude::*;
```

```rust
use super::fs::Ext2;
```

```rust
#[derive(Debug)]
pub struct Inode {
    ino: u32,
    type_: InodeType,
    inner: RwMutex<InodeInner>,
    block_group_idx: usize,
    fs: Weak<Ext2>,
    extension: Extension,
}
```

```rust
pub struct InodeInner {
    desc: Dirty<InodeDesc>,
    is_freed: bool,
    weak_self: Weak<Inode>,
    fs: Weak<Ext2>,
    page_cache: PageCache,
}
```

```rust
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeDesc {
    type_: InodeType,
    perm: FilePerm,
    uid: u32,
    gid: u32,
    size: u64,
    atime: Duration,
    ctime: Duration,
    mtime: Duration,
    dtime: Duration,
    links_count: u16,
    blocks: u32,
    flags: FileFlags,
    file_acl: u32,
    generation: u32,
    block_ptrs: [u32; 15],
}
```

```rust
impl Inode {
    pub(super) fn fs_arc(&self) -> Result<Arc<Ext2>>;
}
```

```rust
impl InodeInner {
    fn persist_inode_and_sync(&mut self, fs: &Ext2) -> Result<()>;
}
```

```rust
// From kernel/libs/device-id/src/lib.rs
pub struct DeviceId(u32);
impl DeviceId {
    pub fn from_encoded_u64(raw: u64) -> Option<Self>;
    pub fn as_encoded_u64(&self) -> u64;
}

// encode_device_numbers uses glibc-style 64-bit encoding:
//   ((major & 0xffff_f000) << 32) | ((major & 0x0000_0fff) << 8)
//   | ((minor & 0xffff_ff00) << 12) | (minor & 0x0000_00ff)
// decode_device_numbers reverses this.
pub fn encode_device_numbers(major: u32, minor: u32) -> u64;
pub fn decode_device_numbers(raw: u64) -> (u32, u32);
```

```rust
// From kernel/src/fs/utils/inode.rs
pub enum MknodType {
    NamedPipe,
    CharDevice(u64),   // encoded u64 device id
    BlockDevice(u64),  // encoded u64 device id
}
```

[GUARANTEE]
```rust
impl Inode {
    /// Returns the device ID for char/block device inodes.
    ///
    /// Decodes from `block_ptrs[0..2]` using Linux old/new encoding.
    /// Returns 0 for non-device inodes.
    ///
    /// Linux: ext2_iget (inode.c:1493-1500)
    pub(super) fn device_id(&self) -> u64;

    /// Sets the device ID for char/block device inodes.
    ///
    /// Encodes into `block_ptrs[0..2]` using Linux old/new encoding.
    /// Persists to disk.
    ///
    /// Linux: __ext2_write_inode (inode.c:1589-1599)
    pub(super) fn set_device_id(&self, device_id: u64) -> Result<()>;
}
```

[SPECIFICATION]

## 8.2.0  Device ID Encoding Helpers (InodeDesc methods)

Context (private helpers on InodeDesc):
- These replicate the Linux `kdev_t.h` old/new encoding scheme for on-disk
  device special files. Ext2 stores device IDs in `i_block[0]` (old format)
  or `i_block[1]` (new format) of the raw inode.

### 8.2.0a  decode_device_id — Read device ID from block_ptrs

Linux reference: `ext2_iget` (inode.c:1493-1500):
```c
if (raw_inode->i_block[0])
    init_special_inode(inode, inode->i_mode,
       old_decode_dev(le32_to_cpu(raw_inode->i_block[0])));
else
    init_special_inode(inode, inode->i_mode,
       new_decode_dev(le32_to_cpu(raw_inode->i_block[1])));
```

Linux `old_decode_dev` (kdev_t.h:34-37):
```c
static __always_inline dev_t old_decode_dev(u16 val)
{
    return MKDEV((val >> 8) & 255, val & 255);
}
// MKDEV(ma,mi) = (ma << 20) | mi
```

Linux `new_decode_dev` (kdev_t.h:46-51):
```c
static __always_inline dev_t new_decode_dev(u32 dev)
{
    unsigned major = (dev & 0xfff00) >> 8;
    unsigned minor = (dev & 0xff) | ((dev >> 12) & 0xfff00);
    return MKDEV(major, minor);
}
```

Algorithm:
1. If `self.block_ptrs[0] != 0`:
   - Extract `val = self.block_ptrs[0]` (treated as old-format 16-bit value stored in u32).
   - `major = (val >> 8) & 0xFF`
   - `minor = val & 0xFF`
   - (Linux: `old_decode_dev(le32_to_cpu(raw_inode->i_block[0]))`)
2. Else:
   - Extract `dev = self.block_ptrs[1]`.
   - `major = (dev & 0xfff00) >> 8`
   - `minor = (dev & 0xff) | ((dev >> 12) & 0xfff00)`
   - (Linux: `new_decode_dev(le32_to_cpu(raw_inode->i_block[1]))`)
3. Convert `(major, minor)` to Asterinas encoded u64 via `encode_device_numbers(major, minor)`.
4. Return the encoded u64.

---

### 8.2.0b  encode_device_id — Write device ID into block_ptrs

Linux reference: `__ext2_write_inode` (inode.c:1589-1599):
```c
if (S_ISCHR(inode->i_mode) || S_ISBLK(inode->i_mode)) {
    if (old_valid_dev(inode->i_rdev)) {
        raw_inode->i_block[0] =
            cpu_to_le32(old_encode_dev(inode->i_rdev));
        raw_inode->i_block[1] = 0;
    } else {
        raw_inode->i_block[0] = 0;
        raw_inode->i_block[1] =
            cpu_to_le32(new_encode_dev(inode->i_rdev));
        raw_inode->i_block[2] = 0;
    }
}
```

Linux `old_valid_dev` (kdev_t.h:24-27):
```c
static __always_inline bool old_valid_dev(dev_t dev)
{
    return MAJOR(dev) < 256 && MINOR(dev) < 256;
}
```

Linux `old_encode_dev` (kdev_t.h:29-32):
```c
static __always_inline u16 old_encode_dev(dev_t dev)
{
    return (MAJOR(dev) << 8) | MINOR(dev);
}
```

Linux `new_encode_dev` (kdev_t.h:39-44):
```c
static __always_inline u32 new_encode_dev(dev_t dev)
{
    unsigned major = MAJOR(dev);
    unsigned minor = MINOR(dev);
    return (minor & 0xff) | (major << 8) | ((minor & ~0xff) << 12);
}
```

Algorithm:
1. Decode the Asterinas encoded u64 device_id into `(major, minor)` via
   `decode_device_numbers(device_id)`.
2. If `major < 256 && minor < 256` (old_valid_dev):
   - `self.block_ptrs[0] = (major << 8) | minor` (old_encode_dev, as u32)
   - `self.block_ptrs[1] = 0`
   - (Linux: `raw_inode->i_block[0] = old_encode_dev(...)`, `i_block[1] = 0`)
3. Else:
   - `self.block_ptrs[0] = 0`
   - `self.block_ptrs[1] = (minor & 0xff) | (major << 8) | ((minor & ~0xff) << 12)`
     (new_encode_dev, as u32)
   - `self.block_ptrs[2] = 0`
   - (Linux: `i_block[0] = 0`, `i_block[1] = new_encode_dev(...)`, `i_block[2] = 0`)

---

## 8.2.1  device_id — Read Device ID from Inode

Pre (device_id):
- `self` refers to a valid, non-freed `Inode`.

Post (device_id: device inode):
- If `self.type_` is `InodeType::CharDevice` or `InodeType::BlockDevice`:
  1. Acquires read lock on `self.inner`.
  2. Calls `decode_device_id()` on the `InodeDesc`.
  3. Returns the Asterinas encoded u64 device ID.

Post (device_id: non-device inode):
- If `self.type_` is not `CharDevice` or `BlockDevice`:
  - Returns `0`.

---

## 8.2.2  set_device_id — Write Device ID to Inode

Pre (set_device_id):
- `self` refers to a valid, non-freed `Inode`.
- `self.type_` is `InodeType::CharDevice` or `InodeType::BlockDevice`.
- `self.fs` can be upgraded to a live `Arc<Ext2>`.

Post (set_device_id: success):
- Mirrors Linux `__ext2_write_inode` device encoding (inode.c:1589-1599).
- Algorithm:
  1. Acquires write lock on `self.inner`.
  2. Calls `encode_device_id(device_id)` on the `InodeDesc`.
  3. Updates `ctime` to current time.
  4. Marks desc dirty and persists inode to disk via `persist_inode_and_sync`.

Post (set_device_id: failure):
- `Err(EINVAL)` if `self.type_` is not `CharDevice` or `BlockDevice`.
- `Err(EIO)` if filesystem reference is lost or inode persistence fails.

---

## 8.2.3  metadata fix — Return rdev for device inodes

The existing `Inode::metadata()` method currently returns `rdev: 0`.
For device inodes, it must return the decoded device ID.

Algorithm:
- In `metadata()`, replace `rdev: 0` with:
  `rdev: self.device_id()` (which returns 0 for non-device inodes).

Note: `device_id()` acquires a read lock on `self.inner`. Since `metadata()`
already holds a read lock, `device_id()` must NOT be called while the lock
is held. Instead, compute `rdev` before or after the inner lock scope, or
inline the decode logic within the existing lock scope.

Preferred approach: inline `desc.decode_device_id()` inside the existing
read lock scope in `metadata()`, guarded by a type check.

---

## 8.2.4  VFS Integration — impl_for_vfs/inode.rs

The `mknod` stub currently returns `EOPNOTSUPP`. It must be implemented to:

1. Determine `InodeType` and device_id from `MknodType`:
   - `MknodType::CharDevice(dev_id)` → `InodeType::CharDevice`, `dev_id`
   - `MknodType::BlockDevice(dev_id)` → `InodeType::BlockDevice`, `dev_id`
   - `MknodType::NamedPipe` → `InodeType::NamedPipe`, `0`
2. Call `Inode::create(self, name, inode_type, mode.into())` to create the inode.
3. For char/block devices: call `new_inode.set_device_id(dev_id)?` on the
   returned inode (downcast to `ext2::Inode`).
4. Return the new inode.

Linux reference: `ext2_mknod` (namei.c:270-296) creates the inode then calls
`init_special_inode` which sets `i_rdev`. The device encoding happens at
`__ext2_write_inode` time when the inode is persisted.

[DIFF]
Linux: Device ID is stored in `inode->i_rdev` (a `dev_t` = u32 with MKDEV
  encoding: `(major << 20) | minor`) and encoded to `i_block[0]`/`i_block[1]`
  only at write-inode time via `__ext2_write_inode`.
  → Asterinas: No separate `i_rdev` field. Device ID is encoded directly into
  `block_ptrs[0..2]` at `set_device_id` time and decoded from `block_ptrs`
  at `device_id` time. The on-disk format is identical to Linux.

Linux: Uses `dev_t` (u32) internally with `MKDEV(major << 20 | minor)`.
  → Asterinas: Uses `u64` encoded device IDs (glibc-style via
  `encode_device_numbers`/`decode_device_numbers`). Conversion between
  the two happens at the encode/decode boundary in `InodeDesc` methods.

Linux: `old_decode_dev` takes a `u16` parameter.
  → Asterinas: `block_ptrs[0]` is `u32`. The old-format value occupies only
  the low 16 bits (major 8 bits + minor 8 bits), but we read the full u32
  and apply the same bit masks, matching Linux behavior since
  `le32_to_cpu(raw_inode->i_block[0])` also reads a full 32-bit value
  that is then passed to `old_decode_dev` (which truncates to u16 in Linux,
  but the ext2 on-disk value in i_block[0] only has meaningful bits in the
  low 16 bits for old-format devices).
