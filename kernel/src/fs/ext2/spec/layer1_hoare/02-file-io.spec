// SPDX-License-Identifier: MPL-2.0
//
// Layer 1: Crash Hoare Logic -- File I/O (read_at, write_at)
//
// Pure abstract specs for file data read and write. At this layer,
// buffered and direct I/O are UNIFIED -- the abstract model does not
// distinguish I/O paths. Only the data-level contract matters.
//
// Reference: 00-abstract-state.spec for AbstractFS model and notation.
// Notation:
//   FS       -- pre-state  (AbstractFS before operation)
//   FS'      -- post-state (AbstractFS after operation)
//   FS.durable -- last synced persistent state
//   {P} C {Q} | {R} -- Crash Hoare quadruple (P=pre, Q=post, R=crash)

/// =============================================================================
/// HOARE read_at(ino, offset, len)
/// =============================================================================
/// Reads up to `len` bytes of file data starting at `offset`.
///
/// At the abstract level this is a pure observation: it returns a prefix
/// of data[ino] and touches only the volatile atime field. No durable
/// state is modified, so crash recovery always yields the pre-state.
///
/// CODE: kernel/src/fs/ext2/inode.rs:567-590  (buffered)
/// CODE: kernel/src/fs/ext2/inode.rs:664-698  (direct)
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:21-33
/// LINUX_REF: fs/ext2/file.c:283 (ext2_file_read_iter)

HOARE read_at(ino: Ino, offset: usize, len: usize) -> Result<usize> {

    PRE:
        ino in dom(FS.inodes)
        FS.inodes[ino].alive
        FS.inodes[ino].type_ != Dir

    POST (Ok(n)):
        // -- Return value --
        LET file_size = FS.inodes[ino].size
        IF offset >= file_size:
            n = 0
            FS' = FS                        // nothing read, no mutation
        ELSE:
            n = min(len, file_size - offset)
            returned_data = FS.data[ino][offset .. offset + n]

            // -- Metadata --
            FS'.inodes[ino].atime = now()

            // -- FRAME: only atime changed --
            FS'.inodes[ino] \ {atime} = FS.inodes[ino] \ {atime}
            FS'.data[ino]             = FS.data[ino]
            FS'.dirs                  = FS.dirs
            FS'.xattrs                = FS.xattrs
            FS'.sb                    = FS.sb
            forall j != ino: FS'.inodes[j] = FS.inodes[j]

    POST_ERR (Err(e)):
        e in {EISDIR}
            // EISDIR: ino is a directory (violates PRE at runtime)
        FS' = FS

    CRASH:
        // read_at performs no durable mutation. atime is volatile only.
        // On crash at any point during execution:
        FS_recovered = FS.durable

    LINUX_REF: fs/ext2/file.c:283
}

/// =============================================================================
/// HOARE write_at(ino, offset, buf)
/// =============================================================================
/// Writes `buf` bytes into file data starting at `offset`, extending the
/// file if offset+|buf| exceeds the current size.
///
/// This is the most complex single-inode data operation. At the abstract
/// level it is a deterministic splice of the byte sequence. The crash
/// clause reflects ext2's lack of journaling: partial writes are possible.
///
/// CODE: kernel/src/fs/ext2/inode.rs:592-662  (buffered)
/// CODE: kernel/src/fs/ext2/inode.rs:700-780  (direct)
/// VFS:  kernel/src/fs/ext2/impl_for_vfs/inode.rs:35-47
/// LINUX_REF: fs/ext2/file.c:295 (ext2_file_write_iter)

HOARE write_at(ino: Ino, offset: usize, buf: ByteSeq) -> Result<usize> {

    PRE:
        ino in dom(FS.inodes)
        FS.inodes[ino].alive
        FS.inodes[ino].type_ != Dir

    POST (Ok(n)):
        LET old_size = FS.inodes[ino].size
        LET end      = offset + |buf|
        n = |buf|

        // -- Data content --
        // Extend with zeros if offset > old_size (hole), then overwrite.
        LET padded = IF offset > old_size:
                         FS.data[ino] ++ zeros(offset - old_size)
                     ELSE:
                         FS.data[ino]
        FS'.data[ino] = splice(padded, offset, buf)

        // -- Size --
        FS'.inodes[ino].size = max(old_size, end)

        // -- Timestamps --
        FS'.inodes[ino].mtime = now()
        FS'.inodes[ino].ctime = now()

        // -- Block accounting --
        // New blocks may have been allocated; exact count is impl detail.
        FS'.inodes[ino].blocks >= FS.inodes[ino].blocks

        // -- Superblock --
        // free_blocks may decrease (transiently stale until sync_metadata).
        FS'.sb.free_blocks <= FS.sb.free_blocks

        // -- FRAME: only ino's data and metadata changed --
        FS'.inodes[ino] \ {size, mtime, ctime, blocks} =
            FS.inodes[ino] \ {size, mtime, ctime, blocks}
        FS'.dirs   = FS.dirs
        FS'.xattrs = FS.xattrs
        forall j != ino: FS'.inodes[j] = FS.inodes[j]
        forall j != ino: FS'.data[j]   = FS.data[j]

    POST_ERR (Err(e)):
        e in {EISDIR, ENOSPC, EIO, EINVAL}
            // EISDIR:  ino is a directory
            // ENOSPC:  no free blocks for allocation
            // EIO:     block device or internal error
            // EINVAL:  overflow or alignment (direct I/O)
        FS' = FS
            // Complete rollback via write_failed_cleanup:
            // size restored, allocated blocks freed, page cache trimmed.

    CRASH:
        // Ext2 has no journal. On crash during write_at, the recovered
        // state depends on which phase was in progress:
        //
        //   Phase 1 (allocation): blocks allocated, size grown, data
        //       uninitialized. Recovered inode may show new size with
        //       zero/stale data in the extended region.
        //
        //   Phase 2 (data transfer): partial data in page cache (buffered)
        //       or partial device writes (direct). On-disk state may
        //       reflect a prefix of the write.
        //
        //   Phase 3 (metadata persist): if persist completed, metadata
        //       is durable but data pages may not be flushed yet.
        //
        // Formally:
        FS_recovered in {
            FS.durable,                          // crash before any persist
            partial_write(FS.durable, ino,       // crash mid-operation
                          offset, buf[0..k])     //   for some 0 <= k <= |buf|
                where partial_write(D, i, o, b) = {
                    D with {
                        data[i]   = splice(D.data[i], o, b),
                        inodes[i].size = max(D.inodes[i].size, o + |b|),
                        // mtime/ctime may or may not be updated
                        // blocks may reflect partial allocation
                    }
                },
            FS'                                  // crash after full persist
        }
        // fsck will repair structural invariants (INV-04..INV-07).

    LINUX_REF: fs/ext2/file.c:295
}
