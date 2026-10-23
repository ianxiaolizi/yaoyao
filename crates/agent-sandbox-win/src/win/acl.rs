//! DACL edits: the package SID, the capability SIDs, and the grants that make
//! the workspace reachable from inside the container.
//!
//! Every grant here is an **additive, idempotent** ACE. `SetEntriesInAclW` with
//! `GRANT_ACCESS` merges into an existing ACE for the same trustee rather than
//! appending a second one, and nothing in this module removes an ACE except the
//! test that put one there. Setup is meant to be re-runnable; a design that
//! needed "undo" to stay correct would make a second run a change.
//!
//! The one object whose DACL is edited through a handle is `\\.\NUL`: it is a
//! device, not a file, so the path-based security APIs do not reach it. The
//! grant there is what makes `git` and the MSVC toolchain able to open the null
//! device inside the container (the B0 spike measured both before and after it).

use std::ffi::c_void;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, GetSecurityInfo, ProgressInvokeNever,
    SE_FILE_OBJECT, SE_KERNEL_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, SetSecurityInfo,
    TREE_SEC_INFO_RESET_KEEP_EXPLICIT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TreeSetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION, FreeSid, GetAce,
    GetAclInformation, NO_INHERITANCE, PSECURITY_DESCRIPTOR, PSID, SID_AND_ATTRIBUTES,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
};
use windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED;

use super::{hresult_message, last_error, wide, win32_message};

/// Read, write, execute and delete — the `icacls` "modify" grant, without
/// `WRITE_DAC`/`WRITE_OWNER`. The sandbox may change the files; it may not
/// change who is allowed to.
pub const MODIFY: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;

/// Read and execute only, for a toolchain directory the sandbox uses but does
/// not own.
pub const READ_EXECUTE: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;

/// An AppContainer package SID, freed when the value is dropped.
pub struct Sid(PSID);

// SAFETY: the pointer names an immutable SID this value owns; the security APIs
// only read it, and it is not freed until `Drop` runs.
unsafe impl Send for Sid {}
// SAFETY: as `Send` — sharing a read-only SID across threads is what the
// launcher's `spawn(&self)` does on every call.
unsafe impl Sync for Sid {}

impl Sid {
    /// The SID a profile name derives to. This does **not** create the profile;
    /// it is the deterministic hash the name maps to, which is why a status
    /// probe can read it without touching the machine.
    pub fn derive(profile: &str) -> Result<Self, String> {
        let name = wide(profile);
        let mut sid: PSID = null_mut();
        // SAFETY: `name` is a NUL-terminated UTF-16 string; on success the API
        // allocates the SID and transfers it to the caller, which is the value
        // wrapped here.
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
        if hr != 0 {
            return Err(format!("could not derive the AppContainer SID: {}", hresult_message(hr)));
        }
        if sid.is_null() {
            return Err("the AppContainer SID API reported success without a SID".to_string());
        }
        Ok(Self(sid))
    }

    pub fn as_ptr(&self) -> PSID {
        self.0
    }

    pub fn to_string(&self) -> Result<String, String> {
        let mut raw: *mut u16 = null_mut();
        // SAFETY: `self.0` is a valid SID owned by this value; on success the
        // API returns an allocated NUL-terminated string.
        let ok = unsafe {
            windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW(self.0, &mut raw)
        };
        if ok == 0 {
            return Err(format!("could not format the SID: {}", last_error()));
        }
        let mut length = 0usize;
        // SAFETY: the API above returned a NUL-terminated string.
        while unsafe { *raw.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: the first `length` units are readable and initialized.
        let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(raw, length) });
        // SAFETY: the string came from `ConvertSidToStringSidW`, whose contract
        // is allocation with `LocalAlloc` and release with `LocalFree`.
        unsafe { LocalFree(raw as *mut c_void) };
        Ok(text)
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the SID was allocated by userenv (derive or profile
            // creation), both of which document `FreeSid` as the release.
            unsafe { FreeSid(self.0) };
        }
    }
}

/// The capability SID arrays `DeriveCapabilitySidsFromName` returns.
///
/// Kept alive for as long as the launcher lives: the `SID_AND_ATTRIBUTES`
/// entries point into these blocks, and `CreateProcessW` reads them on every
/// spawn.
pub struct Capabilities {
    /// Arrays of `PSID`, allocated by the API and released with `LocalFree`.
    group_sids: *mut PSID,
    sids: *mut PSID,
    sid_count: u32,
}

// SAFETY: the arrays and the SIDs inside them are immutable after derivation and
// freed only in `Drop`.
unsafe impl Send for Capabilities {}
// SAFETY: as `Send`.
unsafe impl Sync for Capabilities {}

impl Capabilities {
    /// The `internetClient` capability, the only one the backend can be asked
    /// for. No `broadFileSystemAccess`, no private-network capability: each
    /// would widen the boundary past what `AGENT_SANDBOX_NETWORK=allow` says.
    pub fn internet_client() -> Result<Self, String> {
        let name = wide("internetClient");
        let mut group_sids: *mut PSID = null_mut();
        let mut group_count: u32 = 0;
        let mut sids: *mut PSID = null_mut();
        let mut sid_count: u32 = 0;
        // SAFETY: the name is NUL-terminated; the four out-pointers are valid
        // for the duration of the call, and the API allocates the two arrays
        // (each element a `PSID` pointing into the same allocation).
        let ok = unsafe {
            windows_sys::Win32::Security::DeriveCapabilitySidsFromName(
                name.as_ptr(),
                &mut group_sids,
                &mut group_count,
                &mut sids,
                &mut sid_count,
            )
        };
        if ok == 0 {
            return Err(format!(
                "could not derive the internetClient capability: {}",
                last_error()
            ));
        }
        let _ = group_count;
        Ok(Self { group_sids, sids, sid_count })
    }

    /// The capability entries to hand `SECURITY_CAPABILITIES`, with the group
    /// attribute that makes the capability enabled rather than merely present.
    pub fn entries(&self) -> Vec<SID_AND_ATTRIBUTES> {
        let mut entries = Vec::with_capacity(self.sid_count as usize);
        for index in 0..self.sid_count as usize {
            // SAFETY: `sids` has `sid_count` entries allocated by the API.
            let sid = unsafe { *self.sids.add(index) };
            entries.push(SID_AND_ATTRIBUTES { Sid: sid, Attributes: SE_GROUP_ENABLED as u32 });
        }
        entries
    }
}

impl Drop for Capabilities {
    fn drop(&mut self) {
        // SAFETY: both arrays were allocated by `DeriveCapabilitySidsFromName`,
        // which documents `LocalFree` as the release. The SIDs live inside the
        // same blocks.
        unsafe {
            if !self.group_sids.is_null() {
                LocalFree(self.group_sids as *mut c_void);
            }
            if !self.sids.is_null() {
                LocalFree(self.sids as *mut c_void);
            }
        }
    }
}

/// Create the profile if it is not there, and return its SID.
///
/// `created` is false for an existing profile: `ERROR_ALREADY_EXISTS` is the
/// idempotent second run, not a failure.
pub fn create_or_derive(profile: &str) -> Result<(Sid, bool), String> {
    let name = wide(profile);
    let display = wide("yaoyao execution sandbox");
    let description = wide("the AppContainer profile agent runs use");
    let mut sid: PSID = null_mut();
    // SAFETY: all three strings are NUL-terminated and outlive the call; the
    // API allocates the SID on success.
    let hr = unsafe {
        CreateAppContainerProfile(
            name.as_ptr(),
            display.as_ptr(),
            description.as_ptr(),
            null(),
            0,
            &mut sid,
        )
    };
    if hr == 0 {
        if sid.is_null() {
            return Err("the profile API reported success without a SID".to_string());
        }
        return Ok((Sid(sid), true));
    }
    // HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS): the profile is there. Derive the
    // same SID the creation would have returned.
    if hr as u32 == (0x8007_0000 | ERROR_ALREADY_EXISTS) {
        return Ok((Sid::derive(profile)?, false));
    }
    Err(hresult_message(hr))
}

/// Add an inheritable grant for `sid` on `path` and propagate it through the
/// existing tree.
///
/// The propagation step is not optional: inheritance only reaches children
/// created *after* the ACE is set, and a workspace already full of files would
/// stay unreadable to the sandbox without it. `RESET_KEEP_EXPLICIT` recomputes
/// inherited ACEs and keeps every explicit one, which is the same operation
/// `icacls /T` performs.
pub fn grant_tree(path: &Path, sid: PSID, access: u32) -> Result<(), String> {
    let wide_path = wide(&path.to_string_lossy());
    let mut old_dacl: *mut ACL = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: the path is NUL-terminated; the DACL and descriptor out-pointers
    // are valid, and `descriptor` owns the returned allocation until freed.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut old_dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if rc != 0 {
        return Err(format!(
            "could not read the DACL of {}: {}",
            path.display(),
            win32_message(rc)
        ));
    }
    if old_dacl.is_null() {
        // A null DACL grants everyone everything. Replacing it with a DACL that
        // names only the sandbox SID would lock the owner out of their own
        // directory, so this is refused rather than "repaired".
        // SAFETY: `descriptor` was allocated by the call above.
        unsafe { LocalFree(descriptor) };
        return Err(format!(
            "{} has no DACL; refusing to replace an unrestricted DACL",
            path.display()
        ));
    }

    let entry = explicit_access(sid, access, GRANT_ACCESS, SUB_CONTAINERS_AND_OBJECTS_INHERIT);
    let mut new_dacl: *mut ACL = null_mut();
    // SAFETY: `entry` names `sid` and stays alive for the call; `old_dacl` is a
    // valid ACL from the descriptor; `new_dacl` receives an allocation.
    let rc = unsafe { SetEntriesInAclW(1, &entry, old_dacl, &mut new_dacl) };
    if rc != 0 {
        // SAFETY: `descriptor` was allocated by `GetNamedSecurityInfoW`.
        unsafe { LocalFree(descriptor) };
        return Err(format!(
            "could not build a DACL for {}: {}",
            path.display(),
            win32_message(rc)
        ));
    }
    // SAFETY: `new_dacl` is the ACL just built and `descriptor` still owns the
    // old one; both are released below.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new_dacl,
            null_mut(),
        )
    };
    if rc != 0 {
        // SAFETY: `new_dacl` and `descriptor` are allocations owned here.
        unsafe {
            LocalFree(new_dacl as *mut c_void);
            LocalFree(descriptor);
        }
        return Err(format!("could not set the DACL of {}: {}", path.display(), win32_message(rc)));
    }

    // The same DACL is applied down the tree with `RESET_KEEP_EXPLICIT`, which
    // recomputes inherited ACEs and leaves every explicit one in place. This is
    // the `icacls /T` half of the operation. The DACL must be passed here: with
    // a null one the API reports `ERROR_INVALID_ACL` (observed on this
    // toolchain) instead of inferring the parent's.
    // SAFETY: the path is NUL-terminated, `new_dacl` is alive until after this
    // call, and `descriptor` is unchanged.
    let rc = unsafe {
        TreeSetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new_dacl,
            null(),
            TREE_SEC_INFO_RESET_KEEP_EXPLICIT,
            None,
            ProgressInvokeNever,
            null(),
        )
    };
    // SAFETY: `new_dacl` and `descriptor` are allocations owned by this function.
    unsafe {
        LocalFree(new_dacl as *mut c_void);
        LocalFree(descriptor);
    }
    if rc != 0 {
        return Err(format!(
            "could not propagate the grant through {}: {}",
            path.display(),
            win32_message(rc)
        ));
    }
    Ok(())
}

fn explicit_access(sid: PSID, access: u32, mode: i32, inheritance: u32) -> EXPLICIT_ACCESS_W {
    // SAFETY: `EXPLICIT_ACCESS_W` is a plain C struct for which all-zero is the
    // documented empty value; every field is overwritten below.
    let mut entry: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
    entry.grfAccessPermissions = access;
    entry.grfAccessMode = mode;
    entry.grfInheritance = inheritance;
    entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
    entry.Trustee.TrusteeType = TRUSTEE_IS_UNKNOWN;
    entry.Trustee.ptstrName = sid as *mut u16;
    entry
}

/// Grant the package SID read/write on the null device.
pub fn grant_null_device(sid: PSID) -> Result<(), String> {
    update_null_device_dacl(sid, GRANT_ACCESS, 0xC000_0000)
}

/// Whether the package SID already has a read/write ACE on the null device.
pub fn null_device_granted(sid: PSID) -> Result<bool, String> {
    // Read-only: the doctor and the launcher's readiness check both run
    // unprivileged, and asking for `WRITE_DAC` they will not use would make the
    // probe fail for exactly the person who most needs the answer.
    let handle = open_null_device(false)?;
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: the handle is open and owned here; the out-pointers are valid.
    let rc = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if rc != 0 {
        close(handle);
        return Err(format!("could not read the null device DACL: {}", win32_message(rc)));
    }
    let present = dacl_contains(dacl, sid);
    // SAFETY: the descriptor is the allocation from `GetSecurityInfo`.
    unsafe { LocalFree(descriptor) };
    close(handle);
    Ok(present)
}

fn update_null_device_dacl(sid: PSID, mode: i32, access: u32) -> Result<(), String> {
    let handle = open_null_device(true)?;
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: the handle is open and owned here; the out-pointers are valid.
    let rc = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if rc != 0 {
        close(handle);
        return Err(format!("could not read the null device DACL: {}", win32_message(rc)));
    }
    let entry = explicit_access(sid, access, mode, NO_INHERITANCE);
    let mut new_dacl: *mut ACL = null_mut();
    // SAFETY: the handle is open, `entry` names `sid`, and `new_dacl` receives
    // an allocation.
    let rc = unsafe { SetEntriesInAclW(1, &entry, dacl, &mut new_dacl) };
    if rc != 0 {
        // SAFETY: allocated above.
        unsafe { LocalFree(descriptor) };
        close(handle);
        return Err(format!("could not build a null device DACL: {}", win32_message(rc)));
    }
    // SAFETY: the handle is open and owns the object being updated.
    let rc = unsafe {
        SetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new_dacl,
            null_mut(),
        )
    };
    // SAFETY: both are allocations owned here.
    unsafe {
        LocalFree(new_dacl as *mut c_void);
        LocalFree(descriptor);
    }
    close(handle);
    if rc != 0 {
        return Err(format!("could not set the null device DACL: {}", win32_message(rc)));
    }
    Ok(())
}

fn open_null_device(edit: bool) -> Result<HANDLE, String> {
    let path = wide(r"\\.\NUL");
    let mut access = READ_CONTROL;
    if edit {
        access |= WRITE_DAC;
    }
    let handle = unsafe {
        // SAFETY: the path is NUL-terminated; no security attributes, existing
        // object only, no template handle.
        CreateFileW(path.as_ptr(), access, 0, null(), OPEN_EXISTING, 0, null_mut())
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(format!("could not open the null device: {}", last_error()));
    }
    Ok(handle)
}

fn close(handle: HANDLE) {
    // SAFETY: the handle was returned by `CreateFileW` and is closed once.
    unsafe { CloseHandle(handle) };
}

/// Whether an ACL grants anything to `sid`.
fn dacl_contains(dacl: *mut ACL, sid: PSID) -> bool {
    if dacl.is_null() {
        return false;
    }
    // SAFETY: plain C struct; zero is the documented empty value.
    let mut info: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `dacl` is a valid ACL and `info` is the right output shape for
    // `AclSizeInformation`.
    let ok = unsafe {
        GetAclInformation(
            dacl,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        return false;
    }
    for index in 0..info.AceCount {
        let mut ace: *mut c_void = null_mut();
        // SAFETY: `index` is below `AceCount`; `ace` receives a pointer into the
        // ACL that stays valid while `dacl` is.
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
            continue;
        }
        let header = ace as *const windows_sys::Win32::Security::ACE_HEADER;
        // SAFETY: `ace` points at a valid ACE whose first field is the header.
        let ace_type = unsafe { (*header).AceType };
        // ACCESS_ALLOWED_ACE_TYPE is 0; only an allow ACE grants anything.
        if ace_type != 0 {
            continue;
        }
        let allowed = ace as *const windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
        // SAFETY: an allow ACE has the `SidStart` field followed by the SID; the
        // security APIs define the SID as starting there.
        let entry_sid = unsafe { &(*allowed).SidStart as *const u32 as PSID };
        // SAFETY: both pointers name valid SIDs.
        if unsafe { windows_sys::Win32::Security::EqualSid(entry_sid, sid) } != 0 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Remove the ACE for `sid` from one object: the test's cleanup, not
    /// something a run does.
    fn revoke(path: &Path, sid: PSID) -> Result<(), String> {
        let wide_path = wide(&path.to_string_lossy());
        let mut old_dacl: *mut ACL = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: as in `grant_tree`.
        let rc = unsafe {
            GetNamedSecurityInfoW(
                wide_path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut old_dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        if rc != 0 {
            return Err(format!(
                "could not read the DACL of {}: {}",
                path.display(),
                win32_message(rc)
            ));
        }
        let entry = explicit_access(
            sid,
            0,
            windows_sys::Win32::Security::Authorization::REVOKE_ACCESS,
            NO_INHERITANCE,
        );
        let mut new_dacl: *mut ACL = null_mut();
        // SAFETY: as in `grant_tree`.
        let rc = unsafe { SetEntriesInAclW(1, &entry, old_dacl, &mut new_dacl) };
        if rc != 0 {
            // SAFETY: allocated above.
            unsafe { LocalFree(descriptor) };
            return Err(format!(
                "could not build a DACL for {}: {}",
                path.display(),
                win32_message(rc)
            ));
        }
        // SAFETY: as in `grant_tree`.
        let rc = unsafe {
            SetNamedSecurityInfoW(
                wide_path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                new_dacl,
                null_mut(),
            )
        };
        // SAFETY: both are allocations owned here.
        unsafe {
            LocalFree(new_dacl as *mut c_void);
            LocalFree(descriptor);
        }
        if rc != 0 {
            return Err(format!(
                "could not set the DACL of {}: {}",
                path.display(),
                win32_message(rc)
            ));
        }
        Ok(())
    }

    fn ace_count(path: &Path, sid: PSID) -> Result<usize, String> {
        let wide_path = wide(&path.to_string_lossy());
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        let rc = unsafe {
            // SAFETY: as in `grant_tree`.
            GetNamedSecurityInfoW(
                wide_path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        if rc != 0 {
            return Err(win32_message(rc));
        }
        // SAFETY: plain C struct; zeroed.
        let mut info: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            // SAFETY: `dacl` is valid; `info` is the right output shape.
            GetAclInformation(
                dacl,
                &mut info as *mut _ as *mut c_void,
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };
        let mut count = 0;
        if ok != 0 {
            for index in 0..info.AceCount {
                let mut ace: *mut c_void = null_mut();
                // SAFETY: `index` is below `AceCount`.
                if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
                    continue;
                }
                let header = ace as *const windows_sys::Win32::Security::ACE_HEADER;
                // SAFETY: valid ACE header.
                if unsafe { (*header).AceType } != 0 {
                    continue;
                }
                let allowed = ace as *const windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
                // SAFETY: allow ACE layout as above.
                let entry_sid = unsafe { &(*allowed).SidStart as *const u32 as PSID };
                // SAFETY: both are valid SIDs.
                if unsafe { windows_sys::Win32::Security::EqualSid(entry_sid, sid) } != 0 {
                    count += 1;
                }
            }
        }
        // SAFETY: allocated by `GetNamedSecurityInfoW`.
        unsafe { LocalFree(descriptor) };
        Ok(count)
    }

    #[test]
    fn granting_twice_leaves_one_ace() {
        let dir = std::env::temp_dir().join(format!("yaoyao-acl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the test directory");
        let sid = Sid::derive("yaoyao.Agent.Sandbox.AclTest").expect("derive a test SID");

        grant_tree(&dir, sid.as_ptr(), MODIFY).expect("first grant");
        grant_tree(&dir, sid.as_ptr(), MODIFY).expect("second grant");
        assert_eq!(
            ace_count(&dir, sid.as_ptr()).unwrap(),
            1,
            "a re-run must not append a second ACE"
        );

        revoke(&dir, sid.as_ptr()).expect("revoke");
        assert_eq!(ace_count(&dir, sid.as_ptr()).unwrap(), 0);

        std::fs::remove_dir(&dir).expect("remove the test directory");
    }

    #[test]
    fn the_null_device_dacl_can_be_read_without_elevation() {
        // The doctor and the launcher's readiness check run unprivileged. If the
        // read path asked for `WRITE_DAC` this would fail on an ordinary
        // account and "not set up" would be indistinguishable from "cannot
        // look".
        let sid = Sid::derive("yaoyao.Agent.Sandbox.NullTest").expect("derive a test SID");
        let granted = null_device_granted(sid.as_ptr()).expect("the NUL DACL is readable");
        // A derived test SID has no ACE unless someone ran setup against it.
        assert!(!granted);
    }

    #[test]
    fn the_modify_grant_does_not_include_writing_the_dacl() {
        // WRITE_DAC is 0x0004_0000; the sandbox may change the files, not who
        // can reach them.
        assert_eq!(MODIFY & 0x0004_0000, 0);
        assert_eq!(MODIFY & 0x0008_0000, 0, "WRITE_OWNER must not be granted");
        assert_ne!(MODIFY & DELETE, 0);
    }
}
