// SPDX-License-Identifier: MPL-2.0

use core::{cmp::Ordering, mem::size_of};

use super::{fs::Ext2, inode_block_map::Ext2Bid, prelude::*};
use crate::fs::{
    ext2::Inode,
    utils::{XattrName, XattrNamespace, XattrSetFlags},
};

pub(super) const XATTR_NBLOCKS: usize = 1;
pub(super) const XATTR_MAGIC: u32 = 0xEA02_0000;
pub(super) const XATTR_PAD_BITS: usize = 2;
pub(super) const XATTR_PAD: usize = 1 << XATTR_PAD_BITS;
pub(super) const XATTR_ROUND: usize = XATTR_PAD - 1;
pub(super) const XATTR_HEADER_SIZE: usize = size_of::<XattrHeader>();
pub(super) const XATTR_ENTRY_HEADER_SIZE: usize = size_of::<XattrEntryRaw>();
pub(super) const XATTR_TERMINATOR_SIZE: usize = size_of::<u32>();
const NAME_HASH_SHIFT: u32 = 5;
const VALUE_HASH_SHIFT: u32 = 16;
const BLOCK_HASH_SHIFT: u32 = 16;

pub(super) fn xattr_entry_len(name_len: usize) -> usize {
    (name_len + XATTR_ENTRY_HEADER_SIZE + XATTR_ROUND) & !XATTR_ROUND
}

pub(super) fn xattr_value_size(size: usize) -> usize {
    (size + XATTR_ROUND) & !XATTR_ROUND
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum XattrNameIndex {
    User = 1,
    PosixAclAccess = 2,
    PosixAclDefault = 3,
    Trusted = 4,
    Lustre = 5,
    Security = 6,
}

impl From<XattrNamespace> for XattrNameIndex {
    fn from(ns: XattrNamespace) -> Self {
        match ns {
            XattrNamespace::User => Self::User,
            XattrNamespace::Trusted => Self::Trusted,
            XattrNamespace::Security => Self::Security,
            XattrNamespace::System => {
                // POSIX ACL xattrs are not implemented in Phase 10.1 yet.
                Self::PosixAclAccess
            }
        }
    }
}

impl TryFrom<u8> for XattrNameIndex {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::User),
            2 => Ok(Self::PosixAclAccess),
            3 => Ok(Self::PosixAclDefault),
            4 => Ok(Self::Trusted),
            5 => Ok(Self::Lustre),
            6 => Ok(Self::Security),
            _ => Err(Error::with_message(
                Errno::EINVAL,
                "invalid xattr name index",
            )),
        }
    }
}

impl XattrNameIndex {
    pub(super) fn prefix(self) -> &'static str {
        match self {
            Self::User => "user.",
            Self::PosixAclAccess => "system.posix_acl_access.",
            Self::PosixAclDefault => "system.posix_acl_default.",
            Self::Trusted => "trusted.",
            Self::Lustre => "lustre.",
            Self::Security => "security.",
        }
    }

    pub(super) fn strip_prefix<'a>(self, full_name: &'a str) -> Result<&'a str> {
        full_name.strip_prefix(self.prefix()).ok_or_else(|| {
            Error::with_message(
                Errno::EINVAL,
                "xattr name does not match namespace-specific prefix",
            )
        })
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod)]
pub(super) struct XattrHeader {
    pub h_magic: u32,
    pub h_refcount: u32,
    pub h_blocks: u32,
    pub h_hash: u32,
    pub h_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod)]
pub(super) struct XattrEntryRaw {
    pub e_name_len: u8,
    pub e_name_index: u8,
    pub e_value_offs: u16,
    pub e_value_block: u32,
    pub e_value_size: u32,
    pub e_hash: u32,
}

#[derive(Clone, Debug)]
struct XattrEntryData {
    name_index: XattrNameIndex,
    name: Vec<u8>,
    value: Vec<u8>,
}

// TODO: add a entry cache to avoid frequent parsing
#[derive(Debug)]
pub(super) struct Xattr {
    block_buf: Option<USegment>,
    bid: Ext2Bid,
    dirty: bool,
    inode: Weak<Inode>,
    fs: Weak<Ext2>,
}

impl Xattr {
    /// Creates a new xattr handle. `bid` comes from `InodeDesc.file_acl`.
    pub(super) fn new(bid: u32, inode: Weak<Inode>, fs: Weak<Ext2>) -> Self {
        Self {
            block_buf: None,
            bid,
            dirty: false,
            inode,
            fs,
        }
    }

    /// Returns the current xattr block number. Caller uses this to update `InodeDesc.file_acl`.
    pub(super) fn bid(&self) -> u32 {
        self.bid
    }

    fn fs_arc(&self) -> Result<Arc<Ext2>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "ext2 instance is unavailable"))
    }

    fn inode_arc(&self) -> Result<Arc<Inode>> {
        self.inode
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "inode instance is unavailable"))
    }

    fn parse_target_name(name: XattrName) -> Result<(XattrNameIndex, Vec<u8>)> {
        let name_index = XattrNameIndex::from(name.namespace());
        let stripped_name = name_index.strip_prefix(name.full_name())?;
        if stripped_name.len() > u8::MAX as usize {
            return_errno_with_message!(Errno::ERANGE, "xattr name is too long");
        }
        Ok((name_index, stripped_name.as_bytes().to_vec()))
    }

    fn namespace_for_index(index: XattrNameIndex) -> XattrNamespace {
        match index {
            XattrNameIndex::User => XattrNamespace::User,
            XattrNameIndex::Trusted => XattrNamespace::Trusted,
            XattrNameIndex::Security => XattrNamespace::Security,
            XattrNameIndex::PosixAclAccess
            | XattrNameIndex::PosixAclDefault
            | XattrNameIndex::Lustre => XattrNamespace::System,
        }
    }

    fn cmp_entry_key(
        lhs_index: XattrNameIndex,
        lhs_name: &[u8],
        rhs_index: XattrNameIndex,
        rhs_name: &[u8],
    ) -> Ordering {
        (lhs_index as u8)
            .cmp(&(rhs_index as u8))
            .then(lhs_name.len().cmp(&rhs_name.len()))
            .then(lhs_name.cmp(rhs_name))
    }

    fn find_entry_position(
        entries: &[XattrEntryData],
        target_index: XattrNameIndex,
        target_name: &[u8],
    ) -> (Option<usize>, usize) {
        let mut found = None;
        let mut insert_at = entries.len();

        for (idx, entry) in entries.iter().enumerate() {
            match Self::cmp_entry_key(entry.name_index, &entry.name, target_index, target_name) {
                Ordering::Less => continue,
                Ordering::Equal => {
                    found = Some(idx);
                    insert_at = idx;
                    break;
                }
                Ordering::Greater => {
                    insert_at = idx;
                    break;
                }
            }
        }

        (found, insert_at)
    }

    fn alloc_block_buffer(block_size: usize) -> Result<USegment> {
        let npages = block_size.div_ceil(BLOCK_SIZE);
        let segment = FrameAllocOptions::new()
            .zeroed(true)
            .alloc_segment(npages)?;
        Ok(segment.into())
    }

    fn validate_header(block_buf: &USegment) -> Result<()> {
        let header = block_buf.read_val::<XattrHeader>(0)?;
        if header.h_magic != XATTR_MAGIC || header.h_blocks != XATTR_NBLOCKS as u32 {
            return_errno_with_message!(Errno::EIO, "invalid xattr header");
        }
        Ok(())
    }

    fn validate_entry(entry: &XattrEntryRaw, offset: usize, block_size: usize) -> Result<()> {
        let entry_len = xattr_entry_len(entry.e_name_len as usize);
        let next = offset
            .checked_add(entry_len)
            .ok_or_else(|| Error::with_message(Errno::EIO, "xattr entry overflow"))?;
        if next >= block_size {
            return_errno_with_message!(Errno::EIO, "xattr entry overflows block");
        }
        if entry.e_value_block != 0 {
            return_errno_with_message!(Errno::EIO, "xattr external value blocks are not supported");
        }

        let value_size = entry.e_value_size as usize;
        let value_off = entry.e_value_offs as usize;
        let value_end = value_off
            .checked_add(value_size)
            .ok_or_else(|| Error::with_message(Errno::EIO, "xattr value range overflow"))?;
        if value_size > block_size || value_end > block_size {
            return_errno_with_message!(Errno::EIO, "xattr value range is out of block bounds");
        }
        Ok(())
    }

    fn validate_block(block_buf: &USegment, block_size: usize) -> Result<()> {
        if block_size < XATTR_HEADER_SIZE + XATTR_TERMINATOR_SIZE {
            return_errno_with_message!(Errno::EIO, "xattr block is too small");
        }
        Self::validate_header(block_buf)?;

        let mut offset = XATTR_HEADER_SIZE;
        loop {
            if offset + XATTR_TERMINATOR_SIZE > block_size {
                return_errno_with_message!(Errno::EIO, "xattr entry terminator is missing");
            }

            let marker = block_buf.read_val::<u32>(offset)?;
            if marker == 0 {
                return Ok(());
            }

            let entry = block_buf.read_val::<XattrEntryRaw>(offset)?;
            let _ = XattrNameIndex::try_from(entry.e_name_index)
                .map_err(|_| Error::with_message(Errno::EIO, "invalid xattr name index on disk"))?;
            Self::validate_entry(&entry, offset, block_size)?;
            offset += xattr_entry_len(entry.e_name_len as usize);
        }
    }

    fn read_loaded_entries(&self, block_size: usize) -> Result<Vec<XattrEntryData>> {
        let block_buf = self
            .block_buf
            .as_ref()
            .ok_or_else(|| Error::with_message(Errno::EIO, "xattr block buffer not loaded"))?;
        Self::validate_header(block_buf)?;

        let mut entries: Vec<XattrEntryData> = Vec::new();
        let mut offset = XATTR_HEADER_SIZE;
        loop {
            if offset + XATTR_TERMINATOR_SIZE > block_size {
                return_errno_with_message!(Errno::EIO, "xattr entry terminator is missing");
            }

            let marker = block_buf.read_val::<u32>(offset)?;
            if marker == 0 {
                break;
            }

            let entry = block_buf.read_val::<XattrEntryRaw>(offset)?;
            let name_index = XattrNameIndex::try_from(entry.e_name_index)
                .map_err(|_| Error::with_message(Errno::EIO, "invalid xattr name index on disk"))?;
            Self::validate_entry(&entry, offset, block_size)?;

            let name_len = entry.e_name_len as usize;
            let mut name = vec![0u8; name_len];
            block_buf.read_bytes(offset + XATTR_ENTRY_HEADER_SIZE, &mut name)?;

            if let Some(prev) = entries.last() {
                if Self::cmp_entry_key(prev.name_index, &prev.name, name_index, &name)
                    != Ordering::Less
                {
                    return_errno_with_message!(Errno::EIO, "xattr entries are not strictly sorted");
                }
            }

            let value_len = entry.e_value_size as usize;
            let mut value = vec![0u8; value_len];
            if value_len > 0 {
                block_buf.read_bytes(entry.e_value_offs as usize, &mut value)?;
            }

            entries.push(XattrEntryData {
                name_index,
                name,
                value,
            });

            offset += xattr_entry_len(name_len);
        }

        Ok(entries)
    }

    fn read_entry_from_slice(block: &[u8], offset: usize) -> Result<XattrEntryRaw> {
        let end = offset
            .checked_add(XATTR_ENTRY_HEADER_SIZE)
            .ok_or_else(|| Error::with_message(Errno::EIO, "xattr entry header out of range"))?;
        if end > block.len() {
            return_errno_with_message!(Errno::EIO, "xattr entry header out of range");
        }
        let mut entry = XattrEntryRaw::new_zeroed();
        entry.as_bytes_mut().copy_from_slice(&block[offset..end]);
        Ok(entry)
    }

    fn write_entry_to_slice(block: &mut [u8], offset: usize, entry: &XattrEntryRaw) -> Result<()> {
        let end = offset
            .checked_add(XATTR_ENTRY_HEADER_SIZE)
            .ok_or_else(|| Error::with_message(Errno::EIO, "xattr entry header out of range"))?;
        if end > block.len() {
            return_errno_with_message!(Errno::EIO, "xattr entry header out of range");
        }
        block[offset..end].copy_from_slice(entry.as_bytes());
        Ok(())
    }

    fn fold_hash(mut hash: u32, shift: u32, input: u32) -> u32 {
        hash = (hash << shift) ^ (hash >> (32 - shift));
        hash ^ input
    }

    fn hash_entry(block: &[u8], offset: usize, entry: &XattrEntryRaw) -> Result<u32> {
        let mut hash = 0u32;

        let name_start = offset + XATTR_ENTRY_HEADER_SIZE;
        let name_end = name_start
            .checked_add(entry.e_name_len as usize)
            .ok_or_else(|| Error::with_message(Errno::EIO, "xattr name range overflow"))?;
        if name_end > block.len() {
            return_errno_with_message!(Errno::EIO, "xattr name range is out of block bounds");
        }
        for byte in &block[name_start..name_end] {
            hash = Self::fold_hash(hash, NAME_HASH_SHIFT, u32::from(*byte));
        }

        if entry.e_value_block == 0 && entry.e_value_size != 0 {
            let value_off = entry.e_value_offs as usize;
            let value_padded = xattr_value_size(entry.e_value_size as usize);
            let value_end = value_off
                .checked_add(value_padded)
                .ok_or_else(|| Error::with_message(Errno::EIO, "xattr value range overflow"))?;
            if value_end > block.len() {
                return_errno_with_message!(Errno::EIO, "xattr value range is out of block bounds");
            }

            for word_off in (value_off..value_end).step_by(size_of::<u32>()) {
                let word = u32::from_le_bytes([
                    block[word_off],
                    block[word_off + 1],
                    block[word_off + 2],
                    block[word_off + 3],
                ]);
                hash = Self::fold_hash(hash, VALUE_HASH_SHIFT, word);
            }
        }

        Ok(hash)
    }

    fn rehash_block(block: &mut [u8]) -> Result<()> {
        if block.len() < XATTR_HEADER_SIZE + XATTR_TERMINATOR_SIZE {
            return_errno_with_message!(Errno::EIO, "xattr block is too small");
        }

        let mut block_hash = 0u32;
        let mut offset = XATTR_HEADER_SIZE;
        loop {
            if offset + XATTR_TERMINATOR_SIZE > block.len() {
                return_errno_with_message!(Errno::EIO, "xattr entry terminator is missing");
            }

            let marker = u32::from_le_bytes([
                block[offset],
                block[offset + 1],
                block[offset + 2],
                block[offset + 3],
            ]);
            if marker == 0 {
                break;
            }

            let mut entry = Self::read_entry_from_slice(block, offset)?;
            let entry_hash = Self::hash_entry(block, offset, &entry)?;
            entry.e_hash = entry_hash;
            Self::write_entry_to_slice(block, offset, &entry)?;

            if entry_hash == 0 {
                block_hash = 0;
                break;
            }
            block_hash = Self::fold_hash(block_hash, BLOCK_HASH_SHIFT, entry_hash);
            offset += xattr_entry_len(entry.e_name_len as usize);
        }

        block[12..16].copy_from_slice(&block_hash.to_le_bytes());
        Ok(())
    }

    fn build_block(entries: &[XattrEntryData], block_size: usize) -> Result<Vec<u8>> {
        let mut entries_bytes = 0usize;
        let mut values_bytes = 0usize;
        for entry in entries {
            if entry.name.len() > u8::MAX as usize {
                return_errno_with_message!(Errno::ERANGE, "xattr name is too long");
            }
            entries_bytes = entries_bytes
                .checked_add(xattr_entry_len(entry.name.len()))
                .ok_or_else(|| Error::with_message(Errno::ENOSPC, "xattr entries overflow"))?;
            values_bytes = values_bytes
                .checked_add(xattr_value_size(entry.value.len()))
                .ok_or_else(|| Error::with_message(Errno::ENOSPC, "xattr values overflow"))?;
        }

        let required = XATTR_HEADER_SIZE
            .checked_add(entries_bytes)
            .and_then(|v| v.checked_add(XATTR_TERMINATOR_SIZE))
            .and_then(|v| v.checked_add(values_bytes))
            .ok_or_else(|| Error::with_message(Errno::ENOSPC, "xattr block size overflow"))?;
        if required > block_size {
            return_errno_with_message!(Errno::ENOSPC, "insufficient xattr block space");
        }

        let mut block = vec![0u8; block_size];
        block[0..4].copy_from_slice(&XATTR_MAGIC.to_le_bytes());
        block[4..8].copy_from_slice(&(1u32).to_le_bytes());
        block[8..12].copy_from_slice(&(XATTR_NBLOCKS as u32).to_le_bytes());

        let mut entry_cursor = XATTR_HEADER_SIZE;
        let mut value_cursor = block_size;

        for entry in entries {
            let padded_value_len = xattr_value_size(entry.value.len());
            value_cursor = value_cursor.checked_sub(padded_value_len).ok_or_else(|| {
                Error::with_message(Errno::ENOSPC, "insufficient xattr value space")
            })?;

            if entry_cursor + xattr_entry_len(entry.name.len()) + XATTR_TERMINATOR_SIZE
                > value_cursor
            {
                return_errno_with_message!(Errno::ENOSPC, "xattr entry/value regions overlap");
            }

            if !entry.value.is_empty() {
                block[value_cursor..value_cursor + entry.value.len()].copy_from_slice(&entry.value);
            }

            let entry_raw = XattrEntryRaw {
                e_name_len: entry.name.len() as u8,
                e_name_index: entry.name_index as u8,
                e_value_offs: if entry.value.is_empty() {
                    0
                } else {
                    u16::try_from(value_cursor).map_err(|_| {
                        Error::with_message(Errno::EIO, "xattr value offset does not fit in u16")
                    })?
                },
                e_value_block: 0,
                e_value_size: u32::try_from(entry.value.len()).map_err(|_| {
                    Error::with_message(Errno::ERANGE, "xattr value length does not fit in u32")
                })?,
                e_hash: 0,
            };
            Self::write_entry_to_slice(&mut block, entry_cursor, &entry_raw)?;

            let name_start = entry_cursor + XATTR_ENTRY_HEADER_SIZE;
            let name_end = name_start + entry.name.len();
            block[name_start..name_end].copy_from_slice(&entry.name);

            entry_cursor += xattr_entry_len(entry.name.len());
        }

        Self::rehash_block(&mut block)?;
        Ok(block)
    }

    fn write_working_block(&mut self, working_block: &[u8], block_size: usize) -> Result<()> {
        if working_block.len() != block_size {
            return_errno_with_message!(Errno::EIO, "xattr working block size mismatch");
        }

        if self.block_buf.is_none() {
            self.block_buf = Some(Self::alloc_block_buffer(block_size)?);
        }

        let block_buf = self
            .block_buf
            .as_ref()
            .ok_or_else(|| Error::with_message(Errno::EIO, "xattr block buffer not allocated"))?;
        block_buf.write_bytes(0, working_block)?;
        Ok(())
    }

    fn alloc_bid_if_needed(&mut self) -> Result<()> {
        if self.bid != 0 {
            return Ok(());
        }

        let fs = self.fs_arc()?;
        let inode = self.inode_arc()?;
        let goal = {
            let sb = fs.super_block();
            sb.first_data_block()
                .saturating_add(inode.block_group_idx() as u32 * sb.blocks_per_group())
        };
        let range = fs.alloc_blocks(1, goal)?;
        if range.start >= range.end {
            return_errno_with_message!(Errno::EIO, "xattr block allocation returned empty range");
        }
        self.bid = range.start;
        Ok(())
    }

    fn ensure_loaded(&mut self) -> Result<()> {
        if self.block_buf.is_some() || self.bid == 0 {
            return Ok(());
        }

        let fs = self.fs_arc()?;
        let block_size = fs.block_size();
        let block_buf = Self::alloc_block_buffer(block_size)?;

        let bio_segment = BioSegment::new_from_segment(block_buf.clone(), BioDirection::FromDevice);
        fs.read_blocks(self.bid, bio_segment)?;

        Self::validate_block(&block_buf, block_size)?;
        self.block_buf = Some(block_buf);
        Ok(())
    }

    /// Creates or replaces one extended attribute. Allocates block if needed.
    ///
    /// Linux: /root/linux/fs/ext2/xattr.c:405-651 (ext2_xattr_set)
    pub(super) fn set_xattr(
        &mut self,
        name: XattrName,
        value_reader: &mut VmReader,
        flags: XattrSetFlags,
    ) -> Result<()> {
        let (target_index, target_name) = Self::parse_target_name(name)?;
        let block_size = self.fs_arc()?.block_size();
        let value_len = value_reader.remain();
        if value_len > block_size {
            return_errno_with_message!(Errno::ERANGE, "xattr value is too large");
        }

        self.ensure_loaded()?;
        let mut entries = if self.bid == 0 {
            Vec::new()
        } else {
            self.read_loaded_entries(block_size)?
        };

        let (found, insert_at) = Self::find_entry_position(&entries, target_index, &target_name);
        if found.is_some() {
            if flags.contains(XattrSetFlags::CREATE_ONLY) {
                return_errno_with_message!(Errno::EEXIST, "the target xattr already exists");
            }
        } else if flags.contains(XattrSetFlags::REPLACE_ONLY) {
            return_errno_with_message!(Errno::ENODATA, "the target xattr does not exist");
        }

        let mut value = vec![0u8; value_len];
        if value_len > 0 {
            value_reader.read_fallible(&mut VmWriter::from(value.as_mut_slice()))?;
        }

        if let Some(idx) = found {
            entries[idx].value = value;
        } else {
            entries.insert(
                insert_at,
                XattrEntryData {
                    name_index: target_index,
                    name: target_name,
                    value,
                },
            );
        }

        let working_block = Self::build_block(&entries, block_size)?;
        // TODO: maybe add a rollback?
        self.alloc_bid_if_needed()?;
        self.write_working_block(&working_block, block_size)?;
        self.dirty = true;
        self.flush()
    }

    /// Reads one extended-attribute value. Size query if `vm_writer.avail() == 0`.
    ///
    /// Linux: /root/linux/fs/ext2/xattr.c:195-275 (ext2_xattr_get)
    pub(super) fn get_xattr(&mut self, name: XattrName, vm_writer: &mut VmWriter) -> Result<usize> {
        let (target_index, target_name) = Self::parse_target_name(name)?;

        self.ensure_loaded()?;
        if self.bid == 0 {
            return_errno_with_message!(Errno::ENODATA, "the target xattr does not exist");
        }

        let block_size = self.fs_arc()?.block_size();
        let entries = self.read_loaded_entries(block_size)?;
        let value = entries
            .iter()
            .find(|entry| {
                Self::cmp_entry_key(entry.name_index, &entry.name, target_index, &target_name)
                    == Ordering::Equal
            })
            .map(|entry| entry.value.as_slice())
            .ok_or_else(|| {
                Error::with_message(Errno::ENODATA, "the target xattr does not exist")
            })?;

        if vm_writer.avail() == 0 {
            return Ok(value.len());
        }
        if value.len() > vm_writer.avail() {
            return_errno_with_message!(Errno::ERANGE, "the xattr value buffer is too small");
        }

        vm_writer.write_fallible(&mut VmReader::from(value))?;
        Ok(value.len())
    }

    /// Lists extended-attribute names in one namespace. Size query if `list_writer.avail() == 0`.
    ///
    /// Linux: /root/linux/fs/ext2/xattr.c:287-364 (ext2_xattr_list)
    pub(super) fn list_xattr(
        &mut self,
        namespace: XattrNamespace,
        list_writer: &mut VmWriter,
    ) -> Result<usize> {
        self.ensure_loaded()?;
        if self.bid == 0 {
            return Ok(0);
        }

        let block_size = self.fs_arc()?.block_size();
        let entries = self.read_loaded_entries(block_size)?;

        let mut listed_names = Vec::new();
        let mut total_size = 0usize;
        for entry in entries {
            if Self::namespace_for_index(entry.name_index) != namespace {
                continue;
            }

            let prefix = entry.name_index.prefix().as_bytes();
            let name_size = prefix
                .len()
                .checked_add(entry.name.len())
                .and_then(|v| v.checked_add(1))
                .ok_or_else(|| Error::with_message(Errno::ERANGE, "xattr list size overflow"))?;
            total_size = total_size
                .checked_add(name_size)
                .ok_or_else(|| Error::with_message(Errno::ERANGE, "xattr list size overflow"))?;

            let mut full_name = Vec::with_capacity(name_size);
            full_name.extend_from_slice(prefix);
            full_name.extend_from_slice(&entry.name);
            full_name.push(0);
            listed_names.push(full_name);
        }

        if list_writer.avail() == 0 {
            return Ok(total_size);
        }
        if total_size > list_writer.avail() {
            return_errno_with_message!(Errno::ERANGE, "the xattr list buffer is too small");
        }

        for full_name in listed_names {
            list_writer.write_fallible(&mut VmReader::from(full_name.as_slice()))?;
        }
        Ok(total_size)
    }

    /// Removes one extended attribute. Frees block if last entry removed.
    ///
    /// Linux: /root/linux/fs/ext2/xattr.c:405-651 (ext2_xattr_set with value == NULL)
    pub(super) fn remove_xattr(&mut self, name: XattrName) -> Result<()> {
        let (target_index, target_name) = Self::parse_target_name(name)?;

        self.ensure_loaded()?;
        if self.bid == 0 {
            return_errno_with_message!(Errno::ENODATA, "the target xattr does not exist");
        }

        let block_size = self.fs_arc()?.block_size();
        let mut entries = self.read_loaded_entries(block_size)?;
        let (found, _) = Self::find_entry_position(&entries, target_index, &target_name);
        let Some(found_idx) = found else {
            return_errno_with_message!(Errno::ENODATA, "the target xattr does not exist");
        };
        entries.remove(found_idx);

        if entries.is_empty() {
            let fs = self.fs_arc()?;
            fs.free_blocks(self.bid, 1)?;
            self.bid = 0;
            self.block_buf = None;
            self.dirty = false;
            return Ok(());
        }

        let working_block = Self::build_block(&entries, block_size)?;
        self.write_working_block(&working_block, block_size)?;
        self.dirty = true;
        self.flush()
    }

    /// Frees the xattr block entirely (called during inode eviction).
    ///
    /// Linux: /root/linux/fs/ext2/xattr.c:816-861 (ext2_xattr_delete_inode)
    pub(super) fn delete_xattr_block(&mut self) -> Result<()> {
        if self.bid == 0 {
            self.block_buf = None;
            self.dirty = false;
            return Ok(());
        }

        let fs = self.fs_arc()?;
        fs.free_blocks(self.bid, 1)?;
        self.bid = 0;
        self.block_buf = None;
        self.dirty = false;
        Ok(())
    }

    /// Writes the dirty xattr block back to disk.
    pub(super) fn flush(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if self.bid == 0 {
            self.dirty = false;
            return Ok(());
        }

        let block_buf = match &self.block_buf {
            Some(block_buf) => block_buf.clone(),
            None => {
                self.dirty = false;
                return Ok(());
            }
        };

        let fs = self.fs_arc()?;
        let bio_segment = BioSegment::new_from_segment(block_buf, BioDirection::ToDevice);
        fs.write_blocks(self.bid, bio_segment)?;
        self.dirty = false;
        Ok(())
    }
}
