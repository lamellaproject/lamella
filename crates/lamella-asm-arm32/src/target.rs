//! The Cortex-M architecture profiles and their instruction-set capabilities.

/// An M-profile architecture variant, naming the Cortex-M parts that implement it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Profile {
    /// ARMv6-M: Cortex-M0, M0+, M1. The smallest Thumb subset.
    V6M,
    /// ARMv7-M: Cortex-M3.
    V7M,
    /// ARMv7E-M: Cortex-M4 and M7 (ARMv7-M plus the DSP extension).
    V7EM,
    /// ARMv8-M Baseline: Cortex-M23.
    V8MBaseline,
    /// ARMv8-M Mainline: Cortex-M33 and M35P.
    V8MMainline,
    /// ARMv8.1-M Mainline: Cortex-M55 and M85.
    V81MMainline,
}

impl Profile {
    /// Whether this is a Mainline-class profile. This is the axis that decides
    /// whether the wide 32-bit Thumb-2 encodings and `IT` blocks are available:
    /// the Baseline profiles (ARMv6-M, ARMv8-M Baseline) carry only a small fixed
    /// set of 32-bit instructions (`BL`, the memory barriers, and the
    /// system-register moves), not the general 32-bit group.
    #[must_use]
    pub const fn is_mainline(self) -> bool {
        matches!(
            self,
            Profile::V7M | Profile::V7EM | Profile::V8MMainline | Profile::V81MMainline
        )
    }

    /// Whether the general 32-bit Thumb-2 data-processing and load/store
    /// encodings are available. The Baseline profiles lack them: the entire
    /// 32-bit Thumb space of ARMv6-M is branch and miscellaneous control
    /// (Armv6-M ARM (DDI 0419E), A5.3, Table A5-9), so a 32-bit constant comes
    /// from a literal pool or a short instruction sequence rather than the
    /// Mainline-only `MOVW`/`MOVT` pair.
    #[must_use]
    pub const fn has_wide_thumb2(self) -> bool {
        self.is_mainline()
    }

    /// Whether the `IT` (if-then) block, predicating up to four following
    /// instructions, is available. The Baseline profiles lack it and use
    /// conditional branches instead.
    #[must_use]
    pub const fn has_it_blocks(self) -> bool {
        self.is_mainline()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_profiles_lack_the_wide_encodings() {
        assert!(!Profile::V6M.has_wide_thumb2());
        assert!(!Profile::V8MBaseline.has_wide_thumb2());
        assert!(!Profile::V6M.has_it_blocks());
    }

    #[test]
    fn mainline_profiles_have_the_wide_encodings() {
        for profile in [
            Profile::V7M,
            Profile::V7EM,
            Profile::V8MMainline,
            Profile::V81MMainline,
        ] {
            assert!(profile.has_wide_thumb2());
            assert!(profile.has_it_blocks());
        }
    }
}
