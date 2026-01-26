// SPDX-License-Identifier: MPL-2.0

//! This module offers `/proc/partitions` file support, which tells the user space
//! about the block devices and partitions in the system.
//!
//! Reference: <https://man7.org/linux/man-pages/man5/proc.5.html>

use aster_block::SECTOR_SIZE;
use aster_util::printer::VmPrinter;

use crate::{
    fs::{
        procfs::template::{FileOps, ProcFileBuilder},
        utils::{Inode, mkmod},
    },
    prelude::*,
};

/// Represents the inode at `/proc/partitions`.
pub struct PartitionsFileOps;

impl PartitionsFileOps {
    pub fn new_inode(parent: Weak<dyn Inode>) -> Arc<dyn Inode> {
        ProcFileBuilder::new(Self, mkmod!(a+r))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for PartitionsFileOps {
    fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        let mut printer = VmPrinter::new_skip(writer, offset);

        // Output header (matching Linux format)
        writeln!(printer, "major minor  #blocks  name\n")?;

        // Iterate over all block devices
        for device in aster_block::collect_all() {
            let id = device.id();
            let major = id.major().get();
            let minor = id.minor().get();
            // Convert sectors to 1KB blocks
            let blocks = device.metadata().nr_sectors * SECTOR_SIZE / 1024;
            let name = device.name();

            writeln!(printer, "{:4} {:8} {:10} {}", major, minor, blocks, name)?;
        }

        Ok(printer.bytes_written())
    }
}
