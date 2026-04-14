// SPDX-License-Identifier: MPL-2.0

use super::{
    SyscallReturn,
    setxattr::{XattrFileCtx, lookup_path_for_xattr},
};
use crate::{
    fs::{
        file::file_table::{FileDesc, get_file_fast},
        vfs::xattr::{XATTR_LIST_MAX_LEN, XattrNamespace},
    },
    prelude::*,
    process::credentials::capabilities::CapSet,
    syscall::constants::MAX_FILENAME_LEN,
};

pub fn sys_listxattr(
    path_ptr: Vaddr,
    list_ptr: Vaddr, // The given list is used to place xattr (null-terminated) names.
    list_len: usize,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let user_space = ctx.user_space();
    let path = user_space.read_cstring(path_ptr, MAX_FILENAME_LEN)?;

    let len = listxattr(
        XattrFileCtx::Path(path),
        list_ptr,
        list_len,
        &user_space,
        ctx,
    )?;

    Ok(SyscallReturn::Return(len as _))
}

pub fn sys_llistxattr(
    path_ptr: Vaddr,
    list_ptr: Vaddr, // The given list is used to place xattr (null-terminated) names.
    list_len: usize,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let user_space = ctx.user_space();
    let path = user_space.read_cstring(path_ptr, MAX_FILENAME_LEN)?;

    let len = listxattr(
        XattrFileCtx::PathNoFollow(path),
        list_ptr,
        list_len,
        &user_space,
        ctx,
    )?;

    Ok(SyscallReturn::Return(len as _))
}

pub fn sys_flistxattr(
    fd: FileDesc,
    list_ptr: Vaddr, // The given list is used to place xattr (null-terminated) names.
    list_len: usize,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let mut file_table = ctx.thread_local.borrow_file_table_mut();
    let file = get_file_fast!(&mut file_table, fd);

    let user_space = ctx.user_space();
    let len = listxattr(
        XattrFileCtx::FileHandle(file),
        list_ptr,
        list_len,
        &user_space,
        ctx,
    )?;

    Ok(SyscallReturn::Return(len as _))
}

fn listxattr(
    file_ctx: XattrFileCtx,
    list_ptr: Vaddr,
    list_len: usize,
    user_space: &CurrentUserSpace,
    ctx: &Context,
) -> Result<usize> {
    if list_len > XATTR_LIST_MAX_LEN {
        return_errno_with_message!(Errno::E2BIG, "xattr list too long");
    }

    let namespaces = get_visible_xattr_namespaces(ctx);
    let mut list_writer = user_space.writer(list_ptr, list_len)?;

    let path = lookup_path_for_xattr(&file_ctx, ctx)?;
    let mut total = 0;
    for &ns in namespaces {
        total += path.list_xattr(ns, &mut list_writer)?;
    }
    Ok(total)
}

/// Returns the set of xattr namespaces visible to the current process.
///
/// In Linux, `listxattr` enumerates entries from all namespaces the caller
/// is permitted to see. User and Security namespaces are always visible;
/// Trusted is visible only to processes with `CAP_SYS_ADMIN`.
fn get_visible_xattr_namespaces(ctx: &Context) -> &'static [XattrNamespace] {
    let credentials = ctx.posix_thread.credentials();
    let permitted_capset = credentials.permitted_capset();
    let effective_capset = credentials.effective_capset();

    if permitted_capset.contains(CapSet::SYS_ADMIN) && effective_capset.contains(CapSet::SYS_ADMIN)
    {
        &[
            XattrNamespace::User,
            XattrNamespace::Trusted,
            XattrNamespace::Security,
        ]
    } else {
        &[XattrNamespace::User, XattrNamespace::Security]
    }
}
