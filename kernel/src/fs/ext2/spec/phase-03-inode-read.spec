[PROMPT]
Provide `kernel/src/fs/ext2/inode.rs`. Output Rust code only. No unsafe. Follow Linux
`fs/ext2/ext2.h` (struct ext2_inode, struct ext2_inode_info) and `fs/ext2/inode.c`
(`ext2_get_inode`, `ext2_iget`, `ext2_inode_is_fast_symlink`) logic, adapted to Asterinas:
- Block size is fixed to 4096 bytes.
- Use PageCache for inode table reads (no direct raw block pointer arithmetic in callers).
- Use Asterinas types (PageCache, InodeType, RwMutex, Arc).
- No C-style global hash tables; if caching is needed, use BTreeMap.
- Document every Linux source reference (file:line) in comments for each struct/logic block.

[RELY]
use alloc::sync::Arc;
use core::mem::size_of;
use core::time::Duration;

pub trait Pod: Copy + 'static {}
pub trait VmIo { fn read_val<T: Pod>(&self, offset: usize) -> Result<T>; }

pub type Result<T> = core::result::Result<T, Error>;
pub struct Error;
pub enum Errno { EINVAL, EIO, ESTALE, EUCLEAN, EROFS }

pub const BLOCK_SIZE: usize = 4096;
pub const SECTOR_SIZE: usize = 512;

pub struct Vmo;
impl VmIo for Vmo { fn read_val<T: Pod>(&self, offset: usize) -> Result<T>; }

pub struct PageCache;
impl PageCache {
    pub fn pages(&self) -> &Arc<Vmo>;
}

pub struct RwMutexReadGuard<'a, T>(&'a T);

pub struct SuperBlock;
impl SuperBlock {
    pub fn inode_size(&self) -> usize;
    pub fn inodes_per_group(&self) -> u32;
    pub fn total_inodes(&self) -> u32;
    pub fn first_ino(&self) -> u32;
}

pub struct BlockGroupDescTable;
impl BlockGroupDescTable {
    pub fn groups_count(&self) -> u32;
    pub fn group_desc(&self, idx: usize) -> Result<RwMutexReadGuard<'_, BlockGroupDesc>>;
}

pub struct BlockGroupDesc { pub inode_table: u32; }

#[repr(u16)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum InodeType {
    Unknown = 0o000000,
    NamedPipe = 0o010000,
    CharDevice = 0o020000,
    Dir = 0o040000,
    BlockDevice = 0o060000,
    File = 0o100000,
    SymLink = 0o120000,
    Socket = 0o140000,
}
impl InodeType {
    pub fn from_raw_mode(mode: u16) -> Result<Self>;
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, Pod)]
pub struct UnixTime {
    sec: u32,
}
impl From<UnixTime> for Duration;

bitflags! {
    pub struct FilePerm: u16 {
        const S_ISUID = 0o4000;
        const S_ISGID = 0o2000;
        const S_ISVTX = 0o1000;
        const S_IRUSR = 0o0400;
        const S_IWUSR = 0o0200;
        const S_IXUSR = 0o0100;
        const S_IRGRP = 0o0040;
        const S_IWGRP = 0o0020;
        const S_IXGRP = 0o0010;
        const S_IROTH = 0o0004;
        const S_IWOTH = 0o0002;
        const S_IXOTH = 0o0001;
    }
}
impl FilePerm {
    pub fn from_raw_mode(mode: u16) -> Result<Self>;
}

pub const EXT2_N_BLOCKS: usize = 15;
pub type BlockPtrs = [u32; EXT2_N_BLOCKS];

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, Pod)]
pub struct Osd2 {
    pub frag: u8,
    pub fsize: u8,
    pub pad1: u16,
    pub uid_high: u16,
    pub gid_high: u16,
    pub reserved2: u32,
}

/// On-disk inode (Linux struct ext2_inode).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, Pod)]
pub struct RawInode {
    pub mode: u16,
    pub uid_low: u16,
    pub size_low: u32,
    pub atime: UnixTime,
    pub ctime: UnixTime,
    pub mtime: UnixTime,
    pub dtime: UnixTime,
    pub gid_low: u16,
    pub links_count: u16,
    pub blocks: u32,
    pub flags: u32,
    pub osd1: u32,
    pub block_ptrs: BlockPtrs,
    pub generation: u32,
    pub file_acl: u32,
    pub dir_acl: u32,
    pub faddr: u32,
    pub osd2: Osd2,
}

/// In-memory inode descriptor (Asterinas adaptation of ext2_inode_info).
#[derive(Clone, Copy, Debug)]
pub struct InodeDesc {
    pub type_: InodeType,
    pub perm: FilePerm,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: Duration,
    pub ctime: Duration,
    pub mtime: Duration,
    pub dtime: Duration,
    pub links_count: u16,
    pub blocks: u32,
    pub flags: u32,
    pub block_ptrs: BlockPtrs,
    pub generation: u32,
    pub file_acl: u32,
    pub dir_acl: u32,
    pub faddr: u32,
    pub frag: u8,
    pub fsize: u8,
    pub block_group: u32,
}

/// Provides PageCache-backed inode tables for block groups.
pub trait InodeTableProvider: Send + Sync {
    fn inode_table_cache(
        &self,
        group_idx: usize,
        inode_table_block: u32,
        inodes_per_group: u32,
        inode_size: usize,
    ) -> Result<Arc<PageCache>>;
}

/// Validate a data block range (Linux ext2_data_block_valid equivalent).
pub fn data_block_valid(sb: &SuperBlock, block: u32, count: u32) -> bool;

[GUARANTEE]
pub const ROOT_INO: u32 = 2;

pub fn ext2_iget(
    sb: &SuperBlock,
    bg_table: &BlockGroupDescTable,
    inode_tables: &dyn InodeTableProvider,
    ino: u32,
    no_uid32: bool,
) -> Result<InodeDesc>;

impl InodeDesc {
    pub fn from_raw(
        sb: &SuperBlock,
        raw: RawInode,
        block_group: u32,
        no_uid32: bool,
    ) -> Result<Self>;
    pub fn is_fast_symlink(&self) -> bool;
}

[SPECIFICATION]
Pre:
- `sb` has been validated by Phase 1 (fixed 4KB block size, inode_size in [128, 4096]).
- `bg_table` has been validated by Phase 2 and covers all block groups in `sb`.
- `inode_tables` returns a PageCache that maps the inode table bytes of the
  requested block group, starting at `inode_table_block` and sized at
  `inodes_per_group * inode_size`.

Post (success) for `ext2_iget`:
- Validates inode number exactly as Linux `ext2_get_inode`:
  - If `ino != ROOT_INO` and `ino < sb.first_ino()`, return `Err(Errno::EINVAL)`.
  - If `ino > sb.total_inodes()`, return `Err(Errno::EINVAL)`.
- Computes:
  - `block_group = (ino - 1) / sb.inodes_per_group()`.
  - `inode_idx = (ino - 1) % sb.inodes_per_group()`.
  - `offset = inode_idx * sb.inode_size()` (checked; overflow => `Err(Errno::EINVAL)`).
- Obtains the block group descriptor (`bg_table.group_desc(block_group)`) and
  its `inode_table` block. If missing, return `Err(Errno::EIO)` (Linux Egdp path).
- Obtains the PageCache for this inode table by calling `inode_tables.inode_table_cache(...)`.
- Reads `RawInode` from the PageCache at byte `offset`.
- Returns `InodeDesc::from_raw(sb, raw, block_group, no_uid32)`.

Post (success) for `InodeDesc::from_raw`:
- Parses `mode` into:
  - `type_ = InodeType::from_raw_mode(raw.mode)`.
  - `perm = FilePerm::from_raw_mode(raw.mode)`.
- UID/GID (Linux `ext2_iget`, `inode.c:1414-1421`):
  - `uid = raw.uid_low`, `gid = raw.gid_low`.
  - If `no_uid32 == false`, then `uid |= raw.osd2.uid_high << 16`,
    `gid |= raw.osd2.gid_high << 16`.
- Timestamps (seconds):
  - `atime`, `ctime`, `mtime` from `raw.atime/ctime/mtime`.
- Deleted inode detection (Linux `inode.c:1433-1437`):
  - If `raw.links_count == 0` and (`raw.mode == 0` or `raw.dtime` != 0),
    return `Err(Errno::ESTALE)`.
- Blocks/flags:
  - `blocks = raw.blocks` (512-byte sectors).
  - `flags = raw.flags` (no masking at read time).
- Large file handling (Linux `inode.c:1455-1458`):
  - If `type_ == InodeType::File`, `size = raw.size_low | (raw.dir_acl << 32)`.
  - Else `size = raw.size_low` and `dir_acl = raw.dir_acl`.
- Size validity (Linux `i_size_read` negative check):
  - If computed `size > i64::MAX`, return `Err(Errno::EUCLEAN)`.
- Extended attribute block validation (Linux `inode.c:1449-1453`):
  - If `raw.file_acl != 0` and `data_block_valid(sb, raw.file_acl, 1) == false`,
    return `Err(Errno::EUCLEAN)`.
- Copies remaining fields:
  - `links_count`, `generation`, `file_acl`, `faddr`, `frag`, `fsize`,
    `block_ptrs` (no byte swap; keep raw order).
  - `block_group = block_group` argument.
- Sets `dtime = Duration::ZERO` on success (Linux `inode.c:1461`).

Post (success) for `InodeDesc::is_fast_symlink`:
- Returns true iff:
  - `type_ == InodeType::SymLink`, and
  - `blocks - ea_blocks == 0`, where
    `ea_blocks = if file_acl != 0 { BLOCK_SIZE / SECTOR_SIZE } else { 0 }`.

Post (failure):
- `ext2_iget` returns `Err(Errno::EINVAL)` for invalid inode number or overflow.
- `ext2_iget` returns `Err(Errno::EIO)` for missing group descriptor or PageCache read error.
- `InodeDesc::from_raw` returns `Err(Errno::ESTALE)` for deleted inodes.
- `InodeDesc::from_raw` returns `Err(Errno::EUCLEAN)` for corrupted inode data
  (invalid file_acl block or size overflow).
- No persistent side effects; PageCache may allocate/commit pages.

Invariant (InodeDesc):
- `block_ptrs.len() == EXT2_N_BLOCKS`.
- `blocks` counts 512-byte sectors, not filesystem blocks.
