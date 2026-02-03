// SPDX-License-Identifier: MPL-2.0

use super::prelude::*;

#[derive(Clone, Copy, Debug)]
pub struct FilePerm(u16);

impl FilePerm {
    pub fn from_bits_truncate(bits: u16) -> Self {
        Self(bits)
    }
}

#[derive(Debug)]
pub struct Inode;

impl Inode {
    pub fn create(&self, _name: &str, _type_: InodeType, _perm: FilePerm) -> Result<Arc<Inode>> {
        return_errno!(Errno::ENOSYS);
    }

    pub fn write_at(&self, _offset: usize, _data: &[u8]) -> Result<usize> {
        return_errno!(Errno::ENOSYS);
    }
}
