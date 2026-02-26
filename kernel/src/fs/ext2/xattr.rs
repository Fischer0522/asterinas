// SPDX-License-Identifier: MPL-2.0

use core::{cmp::Ordering, mem::size_of};

use ostd::mm::io_util::HasVmReaderWriter;

use super::{fs::Ext2, prelude::*};
use crate::fs::utils::XattrNamespace;

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

impl TryFrom<XattrNamespace> for XattrNameIndex {
    type Error = Error;
    fn try_from(ns: XattrNamespace) -> Result<Self> {
        match ns {
            XattrNamespace::User => Ok(Self::User),
            XattrNamespace::Trusted => Ok(Self::Trusted),
            XattrNamespace::Security => Ok(Self::Security),
            XattrNamespace::System => {
                // POSIX ACL xattrs are not implemented in Phase 10.1 yet, so we default to the access ACL index.
                return_errno_with_message!(Errno::EOPNOTSUPP, "system namespace is not supported");
            }
        }
    }
}

impl XattrNameIndex {
    pub(super) fn from_raw(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::User),
            2 => Some(Self::PosixAclAccess),
            3 => Some(Self::PosixAclDefault),
            4 => Some(Self::Trusted),
            5 => Some(Self::Lustre),
            6 => Some(Self::Security),
            _ => None,
        }
    }

    pub(super) fn from_vfs_namespace(ns: XattrNamespace) -> Result<Self> {
        match ns {
            XattrNamespace::User => Ok(Self::User),
            XattrNamespace::Trusted => Ok(Self::Trusted),
            XattrNamespace::Security => Ok(Self::Security),
            XattrNamespace::System => {
                // POSIX ACL xattrs are not implemented in Phase 10.1 yet.
                return_errno_with_message!(Errno::EOPNOTSUPP, "system namespace is not supported");
            }
        }
    }

    pub(super) fn to_vfs_namespace(self) -> Option<XattrNamespace> {
        match self {
            Self::User => Some(XattrNamespace::User),
            Self::Trusted => Some(XattrNamespace::Trusted),
            Self::PosixAclAccess | Self::PosixAclDefault => Some(XattrNamespace::System),
            Self::Security => Some(XattrNamespace::Security),
            Self::Lustre => None,
        }
    }

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
pub(super) struct ParsedXattrEntry {
    pub name_index: u8,
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

pub(super) fn cmp_name_key(
    lhs_name_index: u8,
    lhs_name: &[u8],
    rhs_name_index: u8,
    rhs_name: &[u8],
) -> Ordering {
    lhs_name_index
        .cmp(&rhs_name_index)
        .then(lhs_name.len().cmp(&rhs_name.len()))
        .then(lhs_name.cmp(rhs_name))
}

pub(super) fn cmp_entry(
    name_index: u8,
    name_suffix: &[u8],
    entry: &XattrEntryRaw,
    entry_name: &[u8],
) -> Ordering {
    cmp_name_key(name_index, name_suffix, entry.e_name_index, entry_name)
}

pub(super) fn read_xattr_block(fs: &Ext2, block_id: u32) -> Result<Vec<u8>> {
    let block_size = fs.block_size();
    if block_size == 0 || block_size > BLOCK_SIZE {
        return_errno_with_message!(Errno::EIO, "invalid filesystem block size for xattr");
    }

    let read_segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let read_status = fs
        .block_device()
        .read_blocks(Bid::new(block_id as u64), read_segment.clone())
        .map_err(|_| Error::with_message(Errno::EIO, "failed to read xattr block"))?;
    if read_status != BioStatus::Complete {
        return_errno_with_message!(Errno::EIO, "failed to read xattr block");
    }

    let mut data = vec![0u8; block_size];
    let mut segment_reader = read_segment
        .reader()
        .map_err(|_| Error::with_message(Errno::EIO, "failed to access xattr read segment"))?;
    let mut data_writer = VmWriter::from(data.as_mut_slice()).to_fallible();
    segment_reader.read_fallible(&mut data_writer)?;

    Ok(data)
}

pub(super) fn write_xattr_block(fs: &Ext2, block_id: u32, data: &[u8]) -> Result<()> {
    let block_size = fs.block_size();
    if block_size == 0 || block_size > BLOCK_SIZE {
        return_errno_with_message!(Errno::EIO, "invalid filesystem block size for xattr");
    }
    if data.len() != block_size {
        return_errno_with_message!(Errno::EINVAL, "xattr block write size mismatch");
    }

    let write_segment = BioSegment::alloc(1, BioDirection::ToDevice);
    {
        let mut segment_writer = write_segment
            .writer()
            .map_err(|_| Error::with_message(Errno::EIO, "failed to access xattr write segment"))?;
        let mut data_reader = VmReader::from(data).to_fallible();
        segment_writer.write_fallible(&mut data_reader)?;
    }

    let write_status = fs
        .block_device()
        .write_blocks(Bid::new(block_id as u64), write_segment)
        .map_err(|_| Error::with_message(Errno::EIO, "failed to write xattr block"))?;
    if write_status != BioStatus::Complete {
        return_errno_with_message!(Errno::EIO, "failed to write xattr block");
    }

    Ok(())
}

pub(super) fn read_header(data: &[u8]) -> Result<XattrHeader> {
    if data.len() < XATTR_HEADER_SIZE {
        return_errno_with_message!(Errno::EIO, "xattr block too small for header");
    }

    Ok(XattrHeader {
        h_magic: read_u32_le(data, 0)?,
        h_refcount: read_u32_le(data, 4)?,
        h_blocks: read_u32_le(data, 8)?,
        h_hash: read_u32_le(data, 12)?,
        h_reserved: [
            read_u32_le(data, 16)?,
            read_u32_le(data, 20)?,
            read_u32_le(data, 24)?,
            read_u32_le(data, 28)?,
        ],
    })
}

pub(super) fn validate_header(header: &XattrHeader) -> Result<()> {
    if header.h_magic != XATTR_MAGIC || header.h_blocks != 1 {
        return_errno_with_message!(Errno::EIO, "invalid xattr header");
    }

    Ok(())
}

pub(super) fn validate_entry(entry_offset: usize, block_size: usize, data: &[u8]) -> Result<()> {
    if entry_offset
        .checked_add(XATTR_ENTRY_HEADER_SIZE)
        .is_none_or(|end| end > block_size || end > data.len())
    {
        return_errno_with_message!(Errno::EIO, "xattr entry header out of range");
    }

    let entry = read_entry_raw(data, entry_offset)?;
    let entry_len = xattr_entry_len(entry.e_name_len as usize);
    let next = entry_offset
        .checked_add(entry_len)
        .ok_or_else(|| Error::with_message(Errno::EIO, "xattr entry length overflow"))?;
    if next >= block_size || next > data.len() {
        return_errno_with_message!(Errno::EIO, "xattr entry exceeds block boundary");
    }

    if entry.e_value_block != 0 {
        return_errno_with_message!(Errno::EIO, "external xattr value blocks are not supported");
    }

    let value_offs = entry.e_value_offs as usize;
    let value_size = entry.e_value_size as usize;
    if value_size > block_size
        || value_offs
            .checked_add(value_size)
            .is_none_or(|end| end > block_size || end > data.len())
    {
        return_errno_with_message!(Errno::EIO, "xattr value range out of bounds");
    }

    let name_start = entry_offset + XATTR_ENTRY_HEADER_SIZE;
    let name_end = name_start
        .checked_add(entry.e_name_len as usize)
        .ok_or_else(|| Error::with_message(Errno::EIO, "xattr name length overflow"))?;
    if name_end > block_size || name_end > data.len() {
        return_errno_with_message!(Errno::EIO, "xattr name exceeds block boundary");
    }

    Ok(())
}

pub(super) fn xattr_hash_entry(entry: &XattrEntryRaw, name: &[u8], value: &[u8]) -> u32 {
    let mut hash = 0u32;
    for byte in name {
        hash = hash.rotate_left(NAME_HASH_SHIFT) ^ u32::from(*byte);
    }

    if entry.e_value_block == 0 && !value.is_empty() {
        let padded_len = xattr_value_size(value.len());
        for chunk_start in (0..padded_len).step_by(size_of::<u32>()) {
            let mut word_bytes = [0u8; size_of::<u32>()];
            if chunk_start < value.len() {
                let chunk_end = (chunk_start + size_of::<u32>()).min(value.len());
                let copy_len = chunk_end - chunk_start;
                word_bytes[..copy_len].copy_from_slice(&value[chunk_start..chunk_end]);
            }
            let word = u32::from_le_bytes(word_bytes);
            hash = hash.rotate_left(VALUE_HASH_SHIFT) ^ word;
        }
    }

    hash
}

pub(super) fn xattr_rehash(data: &mut [u8]) -> Result<()> {
    let header = read_header(data)?;
    validate_header(&header)?;

    let block_size = data.len();
    let mut block_hash = 0u32;
    let mut entry_offset = XATTR_HEADER_SIZE;
    while !is_last_entry(data, entry_offset)? {
        validate_entry(entry_offset, block_size, data)?;

        let entry = read_entry_raw(data, entry_offset)?;
        let name_len = entry.e_name_len as usize;
        let name_start = entry_offset + XATTR_ENTRY_HEADER_SIZE;
        let name_end = name_start + name_len;
        let name = &data[name_start..name_end];

        let value_offs = entry.e_value_offs as usize;
        let value_size = entry.e_value_size as usize;
        let value = &data[value_offs..value_offs + value_size];

        let entry_hash = xattr_hash_entry(&entry, name, value);
        write_u32_le(data, entry_offset + 12, entry_hash)?;

        if entry_hash == 0 {
            block_hash = 0;
            break;
        }
        block_hash = block_hash.rotate_left(BLOCK_HASH_SHIFT) ^ entry_hash;

        entry_offset += xattr_entry_len(name_len);
    }

    write_u32_le(data, 12, block_hash)?;
    Ok(())
}

pub(super) fn parse_xattr_block(data: &[u8]) -> Result<Vec<ParsedXattrEntry>> {
    let header = read_header(data)?;
    validate_header(&header)?;

    let block_size = data.len();
    let mut entries = Vec::new();
    let mut entry_offset = XATTR_HEADER_SIZE;
    while !is_last_entry(data, entry_offset)? {
        validate_entry(entry_offset, block_size, data)?;

        let entry = read_entry_raw(data, entry_offset)?;
        let name_len = entry.e_name_len as usize;
        let name_start = entry_offset + XATTR_ENTRY_HEADER_SIZE;
        let name_end = name_start + name_len;
        let value_offs = entry.e_value_offs as usize;
        let value_size = entry.e_value_size as usize;
        let value_end = value_offs + value_size;

        entries.push(ParsedXattrEntry {
            name_index: entry.e_name_index,
            name: data[name_start..name_end].to_vec(),
            value: data[value_offs..value_end].to_vec(),
        });

        entry_offset += xattr_entry_len(name_len);
    }

    Ok(entries)
}

pub(super) fn build_xattr_block(
    block_size: usize,
    entries: &[ParsedXattrEntry],
) -> Result<Vec<u8>> {
    if block_size < XATTR_HEADER_SIZE + XATTR_TERMINATOR_SIZE {
        return_errno_with_message!(Errno::EIO, "filesystem block too small for xattr");
    }

    let mut data = vec![0u8; block_size];
    write_header(
        &mut data,
        &XattrHeader {
            h_magic: XATTR_MAGIC,
            h_refcount: 1u32,
            h_blocks: 1u32,
            h_hash: 0,
            h_reserved: [0; 4],
        },
    )?;

    let mut entry_offset = XATTR_HEADER_SIZE;
    let mut value_offset = block_size;
    for entry in entries {
        let name_len = entry.name.len();
        if name_len > u8::MAX as usize {
            return_errno_with_message!(Errno::ERANGE, "xattr name too long");
        }
        let value_len = entry.value.len();
        if value_len > u32::MAX as usize {
            return_errno_with_message!(Errno::ERANGE, "xattr value too long");
        }

        let entry_len = xattr_entry_len(name_len);
        let value_slot_len = xattr_value_size(value_len);
        let new_value_offset = value_offset
            .checked_sub(value_slot_len)
            .ok_or_else(|| Error::with_message(Errno::ENOSPC, "xattr block has no free space"))?;
        let needed = entry_offset
            .checked_add(entry_len)
            .and_then(|next| next.checked_add(XATTR_TERMINATOR_SIZE))
            .ok_or_else(|| Error::with_message(Errno::ENOSPC, "xattr metadata area overflow"))?;
        if needed > new_value_offset {
            return_errno_with_message!(Errno::ENOSPC, "xattr block has no free space");
        }

        let value_offs = if value_len == 0 {
            0u16
        } else {
            u16::try_from(new_value_offset).map_err(|_| {
                Error::with_message(Errno::EIO, "xattr value offset exceeds on-disk format")
            })?
        };
        let raw = XattrEntryRaw {
            e_name_len: name_len as u8,
            e_name_index: entry.name_index,
            e_value_offs: value_offs,
            e_value_block: 0,
            e_value_size: value_len as u32,
            e_hash: 0,
        };
        write_entry_raw(&mut data, entry_offset, &raw)?;

        let name_start = entry_offset + XATTR_ENTRY_HEADER_SIZE;
        let name_end = name_start + name_len;
        data[name_start..name_end].copy_from_slice(&entry.name);

        if value_len > 0 {
            data[new_value_offset..new_value_offset + value_len].copy_from_slice(&entry.value);
        }

        entry_offset += entry_len;
        value_offset = new_value_offset;
    }

    xattr_rehash(&mut data)?;
    Ok(data)
}

fn is_last_entry(data: &[u8], entry_offset: usize) -> Result<bool> {
    if entry_offset
        .checked_add(XATTR_TERMINATOR_SIZE)
        .is_none_or(|end| end > data.len())
    {
        return_errno_with_message!(Errno::EIO, "xattr entry terminator is out of range");
    }

    Ok(read_u32_le(data, entry_offset)? == 0)
}

fn read_u16_le(data: &[u8], offset: usize) -> Result<u16> {
    if offset
        .checked_add(size_of::<u16>())
        .is_none_or(|end| end > data.len())
    {
        return_errno_with_message!(Errno::EIO, "xattr u16 read out of range");
    }
    Ok(u16::from_le_bytes([data[offset], data[offset + 1]]))
}

fn read_u32_le(data: &[u8], offset: usize) -> Result<u32> {
    if offset
        .checked_add(size_of::<u32>())
        .is_none_or(|end| end > data.len())
    {
        return_errno_with_message!(Errno::EIO, "xattr u32 read out of range");
    }
    Ok(u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]))
}

fn write_u16_le(data: &mut [u8], offset: usize, value: u16) -> Result<()> {
    if offset
        .checked_add(size_of::<u16>())
        .is_none_or(|end| end > data.len())
    {
        return_errno_with_message!(Errno::EIO, "xattr u16 write out of range");
    }
    let bytes = value.to_le_bytes();
    data[offset] = bytes[0];
    data[offset + 1] = bytes[1];
    Ok(())
}

fn write_u32_le(data: &mut [u8], offset: usize, value: u32) -> Result<()> {
    if offset
        .checked_add(size_of::<u32>())
        .is_none_or(|end| end > data.len())
    {
        return_errno_with_message!(Errno::EIO, "xattr u32 write out of range");
    }
    let bytes = value.to_le_bytes();
    data[offset] = bytes[0];
    data[offset + 1] = bytes[1];
    data[offset + 2] = bytes[2];
    data[offset + 3] = bytes[3];
    Ok(())
}

fn write_header(data: &mut [u8], header: &XattrHeader) -> Result<()> {
    if data.len() < XATTR_HEADER_SIZE {
        return_errno_with_message!(Errno::EIO, "xattr header write out of range");
    }
    write_u32_le(data, 0, header.h_magic)?;
    write_u32_le(data, 4, header.h_refcount)?;
    write_u32_le(data, 8, header.h_blocks)?;
    write_u32_le(data, 12, header.h_hash)?;
    write_u32_le(data, 16, header.h_reserved[0])?;
    write_u32_le(data, 20, header.h_reserved[1])?;
    write_u32_le(data, 24, header.h_reserved[2])?;
    write_u32_le(data, 28, header.h_reserved[3])?;
    Ok(())
}

fn read_entry_raw(data: &[u8], entry_offset: usize) -> Result<XattrEntryRaw> {
    if entry_offset
        .checked_add(XATTR_ENTRY_HEADER_SIZE)
        .is_none_or(|end| end > data.len())
    {
        return_errno_with_message!(Errno::EIO, "xattr entry read out of range");
    }

    Ok(XattrEntryRaw {
        e_name_len: data[entry_offset],
        e_name_index: data[entry_offset + 1],
        e_value_offs: read_u16_le(data, entry_offset + 2)?,
        e_value_block: read_u32_le(data, entry_offset + 4)?,
        e_value_size: read_u32_le(data, entry_offset + 8)?,
        e_hash: read_u32_le(data, entry_offset + 12)?,
    })
}

fn write_entry_raw(data: &mut [u8], entry_offset: usize, entry: &XattrEntryRaw) -> Result<()> {
    if entry_offset
        .checked_add(XATTR_ENTRY_HEADER_SIZE)
        .is_none_or(|end| end > data.len())
    {
        return_errno_with_message!(Errno::EIO, "xattr entry write out of range");
    }

    data[entry_offset] = entry.e_name_len;
    data[entry_offset + 1] = entry.e_name_index;
    write_u16_le(data, entry_offset + 2, entry.e_value_offs)?;
    write_u32_le(data, entry_offset + 4, entry.e_value_block)?;
    write_u32_le(data, entry_offset + 8, entry.e_value_size)?;
    write_u32_le(data, entry_offset + 12, entry.e_hash)?;
    Ok(())
}
