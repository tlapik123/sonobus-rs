//! Flush denormals to zero while the audio thread runs (JUCE `ScopedNoDenormals`, P.cpp:7332):
//! MXCSR FTZ + DAZ on x86/x86_64 (SSE), FPCR FZ on aarch64, nothing elsewhere.

/// Sets the mode on creation and restores the previous one on drop.
pub(crate) struct NoDenormals(usize);

#[cfg(any(
    target_arch = "x86_64",
    all(target_arch = "x86", target_feature = "sse")
))]
mod imp {
    /// MXCSR flush-to-zero (bit 15) and denormals-are-zero (bit 6).
    pub const BITS: usize = 0x8040;

    pub fn get() -> usize {
        let mut v: u32 = 0;
        // SAFETY: `stmxcsr` only stores the 32-bit MXCSR into the local `v`.
        unsafe {
            core::arch::asm!(
                "stmxcsr dword ptr [{}]",
                in(reg) core::ptr::addr_of_mut!(v),
                options(nostack, preserves_flags)
            );
        }
        v as usize
    }

    pub fn set(v: usize) {
        let v = v as u32;
        // SAFETY: `ldmxcsr` loads a value that came from `stmxcsr` with only the FTZ/DAZ bits
        // changed (both valid on every SSE2 CPU), which changes rounding of denormals only.
        unsafe {
            core::arch::asm!(
                "ldmxcsr dword ptr [{}]",
                in(reg) core::ptr::addr_of!(v),
                options(nostack, preserves_flags)
            );
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    /// FPCR flush-to-zero (bit 24).
    pub const BITS: usize = 1 << 24;

    pub fn get() -> usize {
        let v: u64;
        // SAFETY: reading FPCR has no side effects.
        unsafe {
            core::arch::asm!("mrs {}, fpcr", out(reg) v, options(nomem, nostack, preserves_flags));
        }
        v as usize
    }

    pub fn set(v: usize) {
        // SAFETY: writes back a value read from FPCR with only the FZ bit changed, which
        // changes the treatment of denormals only.
        unsafe {
            core::arch::asm!("msr fpcr, {}", in(reg) v as u64, options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(not(any(
    target_arch = "x86_64",
    all(target_arch = "x86", target_feature = "sse"),
    target_arch = "aarch64"
)))]
mod imp {
    pub const BITS: usize = 0;

    pub fn get() -> usize {
        0
    }

    pub fn set(_: usize) {}
}

pub(crate) use imp::BITS;

/// The current floating-point control word (0 where not supported).
#[cfg(test)]
pub(crate) fn current() -> usize {
    imp::get()
}

impl NoDenormals {
    pub fn new() -> Self {
        let old = imp::get();
        if old & BITS != BITS {
            imp::set(old | BITS);
        }
        Self(old)
    }
}

impl Drop for NoDenormals {
    fn drop(&mut self) {
        if self.0 & BITS != BITS {
            imp::set(self.0);
        }
    }
}
