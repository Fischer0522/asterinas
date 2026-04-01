// SPDX-License-Identifier: MPL-2.0

//! Implements the Ext2 filesystem in safe Rust.
//!
//! The Second Extended File System (Ext2) is a major rewrite of the original
//! Ext filesystem. It was the predominant filesystem used by Linux from the
//! early 1990s to the early 2000s. Ext3 and Ext4 build on the Ext2 on-disk
//! format and add features such as journaling.
//!
//! This implementation has the following properties:
//! 1. It contains no `unsafe` Rust.
//! 2. It integrates deeply with `PageCache` for both data and metadata.
//! 3. It supports queue-based block devices and can submit multiple BIO
//!    requests in one operation.
//!
//! # Example
//!
//! ```no_run
//! // Opens an `Ext2` filesystem from the block device.
//! let ext2 = Ext2::open(block_device)?;
//! // Looks up the root inode.
//! let root = ext2.root_inode();
//! // Creates a file inside the root directory.
//! let file = root.create("file", InodeType::File, FilePerm::from_bits_truncate(0o666))?;
//! // Writes data into the file.
//! const WRITE_DATA: &[u8] = b"Hello, World";
//! let len = file.write_at(0, WRITE_DATA)?;
//! assert!(len == WRITE_DATA.len());
//! ```
//!
//! # Limitations
//!
//! The following improvements are still planned:
//! 1. Merge small read and write operations more efficiently.
//! 2. Handle intermediate failure cases more robustly.

pub use fs::Ext2;
pub use inode::{FilePerm, Inode};

use self::fs_type::Ext2Type;
use crate::fs::vfs::registry;

pub(super) fn init() {
    registry::register(&Ext2Type).unwrap();
}
mod block_group;
mod dir;
mod fs;
mod fs_type;
mod impl_for_vfs;
mod indirect_block_manager;
mod inode;
mod inode_block_map;
mod io_range_mapper;
mod prelude;
mod super_block;
mod utils;
mod xattr;

#[cfg(ktest)]
mod testkit;
