use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::null_mut;

use interprocess::os::windows::security_descriptor::{
    AsSecurityDescriptor, AsSecurityDescriptorExt, BorrowedSecurityDescriptor, SecurityDescriptor,
};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, GetSecurityInfo, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, DACL_SECURITY_INFORMATION, GetAce, GetTokenInformation,
    INHERIT_ONLY_ACE, IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES,
    TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid, WinCreatorOwnerRightsSid,
    WinCreatorOwnerSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

pub(super) fn prepare(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let descriptor = private_descriptor()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_sd().cast_mut(),
        bInheritHandle: 0,
    };
    let path_wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { CreateDirectoryW(path_wide.as_ptr(), &attributes) } == 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    if !std::fs::symlink_metadata(path)?.is_dir() {
        return Err(private_error());
    }
    let mut raw = null_mut();
    let result = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut raw,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let allocation = LocalAllocation(raw);
    let descriptor = unsafe { BorrowedSecurityDescriptor::from_ptr(allocation.0) };
    validate_descriptor(&descriptor)
}

pub(super) fn open_private_file(path: &Path, create: bool) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(path)?;
    check_regular_private(&file)?;
    Ok(file)
}

pub(super) fn create_new_private_file(path: &Path) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    check_regular_private(&file)?;
    Ok(file)
}

fn check_regular_private(file: &File) -> io::Result<()> {
    if !file.metadata()?.is_file() {
        return Err(private_error());
    }
    check_file(file)
}

fn check_file(file: &File) -> io::Result<()> {
    let mut raw = null_mut();
    let result = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut raw,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let allocation = LocalAllocation(raw);
    let descriptor = unsafe { BorrowedSecurityDescriptor::from_ptr(allocation.0) };
    validate_descriptor(&descriptor)
}

pub(super) fn private_descriptor() -> io::Result<SecurityDescriptor> {
    let user = current_user()?;
    from_sddl(&format!(
        "O:{user}D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
    ))
}

fn validate_descriptor(descriptor: &impl AsSecurityDescriptor) -> io::Result<()> {
    let current_user = current_user()?;
    let (owner, _) = descriptor.owner()?;
    if owner.is_null() || !trusted_sid(owner.cast_mut(), &current_user)? {
        return Err(private_error());
    }
    let Some((acl, _)) = descriptor.dacl()? else {
        return Err(private_error());
    };
    if acl.is_null() {
        return Err(private_error());
    }
    // Also inspect inheritable grants: Windows can bypass directory traversal checks.
    for index in 0..unsafe { (*acl).AceCount } {
        let mut raw = null_mut();
        if unsafe { GetAce(acl, u32::from(index), &mut raw) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let header = unsafe { &*raw.cast::<ACE_HEADER>() };
        match u32::from(header.AceType) {
            ACCESS_DENIED_ACE_TYPE => {}
            ACCESS_ALLOWED_ACE_TYPE => {
                let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
                let sid = (&raw const ace.SidStart).cast::<c_void>().cast_mut();
                let owner_rights = unsafe { IsWellKnownSid(sid, WinCreatorOwnerRightsSid) } != 0;
                let inherited_owner = u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0
                    && unsafe { IsWellKnownSid(sid, WinCreatorOwnerSid) } != 0;
                if !owner_rights && !inherited_owner && !trusted_sid(sid, &current_user)? {
                    return Err(private_error());
                }
            }
            _ => return Err(private_error()),
        }
    }
    Ok(())
}

fn trusted_sid(sid: PSID, user: &str) -> io::Result<bool> {
    Ok(unsafe { IsWellKnownSid(sid, WinLocalSystemSid) } != 0
        || unsafe { IsWellKnownSid(sid, WinBuiltinAdministratorsSid) } != 0
        || sid_string(sid)? == user)
}

fn current_user() -> io::Result<String> {
    let mut raw_token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(raw_token) };
    let mut size = 0;
    unsafe {
        GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut size);
    }
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    sid_string(user.User.Sid)
}

fn sid_string(sid: PSID) -> io::Result<String> {
    let mut raw = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _allocation = LocalAllocation(raw.cast());
    let mut len = 0;
    while unsafe { *raw.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16(unsafe { std::slice::from_raw_parts(raw, len) }).map_err(io::Error::other)
}

fn from_sddl(text: &str) -> io::Result<SecurityDescriptor> {
    let text: Vec<_> = text.encode_utf16().chain(Some(0)).collect();
    let mut raw = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(text.as_ptr(), 1, &mut raw, null_mut())
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let allocation = LocalAllocation(raw);
    unsafe { BorrowedSecurityDescriptor::from_ptr(allocation.0) }.to_owned_sd()
}

fn private_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Service namespace and state must allow access only to the current user, SYSTEM and Administrators",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
    use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;

    #[test]
    fn rejects_shared_grants_including_inheritance_only_and_null_dacl() {
        let user = current_user().unwrap();
        assert!(validate_descriptor(&private_descriptor().unwrap()).is_ok());
        for dacl in [
            "D:(A;;FA;;;WD)",
            "D:(A;OICIIO;GR;;;WD)",
            "D:NO_ACCESS_CONTROL",
        ] {
            let descriptor = from_sddl(&format!("O:{user}{dacl}")).unwrap();
            assert!(validate_descriptor(&descriptor).is_err(), "accepted {dacl}");
        }
    }

    #[test]
    fn checks_existing_state_file_acl_independently_of_directory() {
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("service");
        prepare(&private).unwrap();
        let file = private.join("endpoint.json");
        drop(open_private_file(&file, true).unwrap());
        assert!(open_private_file(&file, false).is_ok());
        let descriptor = from_sddl("D:P(A;;FA;;;WD)").unwrap();
        let (acl, _) = descriptor.dacl().unwrap().unwrap();
        let wide: Vec<_> = file.as_os_str().encode_wide().chain(Some(0)).collect();
        let result = unsafe {
            SetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                acl.cast_mut(),
                null_mut(),
            )
        };
        assert_eq!(result, 0);
        assert!(open_private_file(&file, false).is_err());
        assert!(open_private_file(&file, true).is_err());
        assert!(prepare(&private).is_ok());
    }
}
