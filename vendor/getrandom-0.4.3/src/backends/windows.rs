//! Implementation for Windows 10 and later
//!
//! On Windows 10 and later, ProcessPrng "is the primary interface to the
//! user-mode per-processor PRNGs" and only requires bcryptprimitives.dll,
//! making it a better option than the other Windows RNG APIs:
//!   - BCryptGenRandom: https://learn.microsoft.com/en-us/windows/win32/api/bcrypt/nf-bcrypt-bcryptgenrandom
//!     - Requires bcrypt.dll (which loads bcryptprimitives.dll anyway)
//!     - Can cause crashes/hangs as BCrypt accesses the Windows Registry:
//!       https://github.com/rust-lang/rust/issues/99341
//!     - Causes issues inside sandboxed code:
//!       https://issues.chromium.org/issues/40277768
//!   - CryptGenRandom: https://learn.microsoft.com/en-us/windows/win32/api/wincrypt/nf-wincrypt-cryptgenrandom
//!     - Deprecated and not available on UWP targets
//!     - Requires advapi32.lib/advapi32.dll (in addition to bcryptprimitives.dll)
//!     - Thin wrapper around ProcessPrng
//!   - RtlGenRandom: https://learn.microsoft.com/en-us/windows/win32/api/ntsecapi/nf-ntsecapi-rtlgenrandom
//!     - Deprecated and not available on UWP targets
//!     - Requires advapi32.dll (in addition to bcryptprimitives.dll)
//!     - Requires using name "SystemFunction036"
//!     - Thin wrapper around ProcessPrng
//!
//! For more information see the Windows RNG Whitepaper: https://aka.ms/win10rng
use crate::Error;
use core::mem::MaybeUninit;

pub use crate::util::{inner_u32, inner_u64};

// 上游用 raw-dylib 直调 ProcessPrng：bcryptprimitives.dll 无导入库，需要
// rustc 调 dlltool 现场生成，而部分 Windows GNU 工具链缺 as/ar 导致失败。
// 这里改用 windows-sys 静态链接 bcrypt.dll 的 BCryptGenRandom，并指定
// BCRYPT_USE_SYSTEM_PREFERRED_RNG（与 ProcessPrng 同为系统 RNG，语义等价）。
use windows_sys::Win32::Security::Cryptography::BCryptGenRandom;

const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x2;

#[inline]
pub fn fill_inner(dest: &mut [MaybeUninit<u8>]) -> Result<(), Error> {
    // BCryptGenRandom 的 cbBuffer 是 u32，按 i32::MAX 分块防止截断溢出
    let chunk_size = usize::try_from(i32::MAX).expect("Windows 不支持 16 位目标");
    for chunk in dest.chunks_mut(chunk_size) {
        let chunk_len = u32::try_from(chunk.len()).expect("块大小受 i32::MAX 限制");
        let result = unsafe {
            BCryptGenRandom(
                core::ptr::null_mut(),
                chunk.as_mut_ptr().cast::<u8>(),
                chunk_len,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        // NTSTATUS 为 0 表示成功；非 0 时返回 UNEXPECTED（与原版 ProcessPrng 语义一致）
        if result != 0 {
            return Err(Error::UNEXPECTED);
        }
    }
    Ok(())
}
