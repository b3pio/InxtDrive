use std::ffi::c_void;

use windows::core::w;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::PSECURITY_DESCRIPTOR;

/// Full access for SYSTEM, Administrators and Everyone: the drive has no
/// per-file permissions of its own, access is governed by the account.
const SDDL: windows::core::PCWSTR = w!("O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)");

pub fn default_descriptor() -> Vec<u8> {
    let mut psd = PSECURITY_DESCRIPTOR::default();
    let mut len = 0u32;
    // SAFETY: valid constant SDDL; the returned buffer is freed with LocalFree.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(SDDL, SDDL_REVISION_1, &mut psd, Some(&mut len))
            .expect("static SDDL is valid");
        let bytes = std::slice::from_raw_parts(psd.0 as *const u8, len as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(psd.0)));
        bytes
    }
}

/// Copies the descriptor into WinFsp's buffer. Returns false if it is too small.
pub fn copy_to(sd: &[u8], buf: Option<&mut [c_void]>) -> bool {
    match buf {
        Some(b) if b.len() >= sd.len() => {
            // SAFETY: `b` has at least `sd.len()` bytes.
            unsafe { std::ptr::copy_nonoverlapping(sd.as_ptr(), b.as_mut_ptr() as *mut u8, sd.len()) };
            true
        }
        Some(_) => false,
        None => true,
    }
}
