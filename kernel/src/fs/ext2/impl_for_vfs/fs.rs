// SPDX-License-Identifier: MPL-2.0

use crate::{
    fs::{
        ext2::{Ext2, MAGIC_NUM},
        utils::{FileSystem, FsEventSubscriberStats, Inode as VfsInode, NAME_MAX, SuperBlock},
    },
    prelude::*,
};

impl FileSystem for Ext2 {
    fn name(&self) -> &'static str {
        // Linux: /root/linux/fs/ext2/super.c:1698 (ext2_fs_type)
        "ext2"
    }

    fn sync(&self) -> Result<()> {
        // Linux: /root/linux/fs/ext2/super.c:1308 (ext2_sync_fs)
        self.sync_metadata()?;
        self.block_device().sync()?;
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn VfsInode> {
        // Linux: /root/linux/fs/ext2/super.c:877 (ext2_fill_super root inode setup)
        self.root_inode()
    }

    fn sb(&self) -> SuperBlock {
        // Linux: /root/linux/fs/ext2/super.c:1446 (ext2_statfs)
        let ext2_sb = self.super_block();
        SuperBlock {
            magic: MAGIC_NUM as u64,
            bsize: ext2_sb.block_size(),
            blocks: ext2_sb.total_blocks() as usize,
            bfree: ext2_sb.free_blocks_count() as usize,
            bavail: ext2_sb.free_blocks_count() as usize,
            files: ext2_sb.total_inodes() as usize,
            ffree: ext2_sb.free_inodes_count() as usize,
            fsid: 0,
            namelen: NAME_MAX,
            frsize: ext2_sb.fragment_size(),
            flags: 0,
        }
    }

    fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        self.fs_event_subscriber_stats()
    }
}
