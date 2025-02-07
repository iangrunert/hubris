// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![no_std]

use core::mem;
use lpc55_pac::PUF;
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;
use unwrap_lite::UnwrapLite;

/// Used to represent valid states for PUF index blocking bits in IDXBLK_L &
/// IDXBLK_H registers. We derive FromPrimivite for this type to enable use
/// 'from_u32'. This will map invalid / reserved register states to `None`.
#[derive(FromPrimitive)]
enum LockState {
    WritesEnabled = 2,
    Locked = 1,
}

/// Set the disable bit and clear the enable bit. The format of the 'bits'
/// parameter is defined by the lpc55 puf index blocking regsiters.
fn disable_index_bits(bits: u32, index: u32) -> u32 {
    let index = index % 8;

    bits & !(2 << (index * 2)) | 1 << (index * 2)
}

/// Set the enable bit and clear the disable bit. The format of the 'bits'
/// parameter is defined by the lpc55 puf index blocking regsiters.
fn enable_index_bits(bits: u32, index: u32) -> u32 {
    let index = index % 8;

    bits & !(1 << (index * 2)) | 2 << (index * 2)
}

/// The Puf structure wraps the lpc55 PUF peripheral in a slightly more
/// user-friendly interface.
pub struct Puf<'a> {
    puf: &'a PUF,
}

impl<'a> Puf<'a> {
    /// Given the length of the key return the size of the required PUF keycode.
    pub const fn key_to_keycode_len(key_len: usize) -> usize {
        if key_len % 8 != 0 {
            // TODO: This function should return an Option / None instead of
            // panicking here. We can't however because const_option is still
            // unstable. When https://github.com/rust-lang/rust/issues/67441
            // is merged this should be updated.
            panic!("key length not a multiple of 8");
        }

        // This is a simplified version of the formula from NXP LPC55 UM11126
        // section 48.11.7.3
        20 + ((key_len + 31) & !31)
    }

    pub fn new(puf: &'a PUF) -> Self {
        Self { puf }
    }

    /// Generate a new key code for a key with the provided PUF index &
    /// length.
    /// NOTE: The PUF doesn't return the key immediately. Instead it
    /// returns a keycode through the second param. This keycode can later
    /// be used to create and return the key to the caller.
    pub fn generate_keycode(
        &self,
        index: u32,
        key_len: usize,
        keycode: &mut [u32],
    ) -> bool {
        if !self.is_generatekey_allowed() {
            panic!("PufCmdDisallowed");
        }

        // devide by sizeof u32 here because keycode param is an array of u32
        let keycode_len =
            Self::key_to_keycode_len(key_len) / mem::size_of::<u32>();
        if keycode.len() < keycode_len {
            panic!("PufKeyCode");
        }

        self.set_key_index(index);
        self.set_key_size(key_len);

        self.puf.ctrl.write(|w| w.generatekey().set_bit());
        if !self.wait_for_cmd_accept() {
            panic!("PufCmdAccept");
        }

        // while PUF is busy, read out whatever part of the KC is available
        let mut idx = 0;
        while self.is_busy() {
            if idx > keycode.len() - 1 {
                panic!("PufKCTooLong");
            }
            if self.is_keycode_part_avail() {
                let keycode_part = self.puf.codeoutput.read().bits();
                keycode[idx] = keycode_part;
                idx += 1;
            }
        }

        self.is_success()
    }

    /// Get the key associated with the given keycode from the PUF. The
    /// keycode should be a value generated by the 'GENERATEKEY' PUF
    /// function.
    /// WARNING: If the key index associated with the keycode parameter is
    /// blocked by one of the IDXBLK registers this function will return
    /// false. The PUF will not produce errors if asked to execute commands
    /// like GETKEY for blocked key indices, instead the PUF seems (based
    /// on experimentation) to just fill the KEYOUTPUT register with 0's.
    /// We check for this condition explicitly to prevent the inadvertent
    /// creation of cryptographic keys from bad seed values.
    pub fn get_key(&self, keycode: &[u32], key: &mut [u8]) -> bool {
        if !self.is_getkey_allowed() {
            return false;
        }

        // If key index is blocked the PUF won't produce an error when we
        // generate or get the key but it will return a key that's all 0's.
        // To prevent this we check that the key index is not blocked before
        // we get our key.
        let index = index_from_keycode(keycode);
        if self.is_index_blocked(index) {
            return false;
        }

        // execute CTRL function / set GETKEY bit in CTRL register, no params
        self.puf.ctrl.write(|w| w.getkey().set_bit());

        self.wait_for_cmd_accept();

        let mut kc_idx = 0;
        let mut key_idx = 0;

        while self.is_busy() && !self.is_error() {
            if self.is_keycode_part_req() {
                self.puf
                    .codeinput
                    .write(|w| unsafe { w.bits(keycode[kc_idx]) });
                kc_idx += 1;
            }
            if self.is_key_part_avail() {
                for byte in self.puf.keyoutput.read().bits().to_ne_bytes() {
                    key[key_idx] = byte;
                    key_idx += 1;
                }
            }
        }

        self.is_success()
    }

    /// Set key index (between 0 & 15) for a key generated by the PUF or set
    /// through the API. This value is ignored for the GetKey command as the
    /// index is baked into the KeyCode.
    ///
    /// NOTE: TL;DR: Don't use index 0 or 15. Use indices 1-7 if possible so
    /// they can be disabled through IDXBLK_L once you're done.
    ///
    /// The longer version:
    /// Key indices are used to identify the destination for a key when it
    /// is loaded by the PUF. Keys with index 0, when loaded, are *not*
    /// available through the KEYOUTPUT register. They are instead transferred
    /// to the AES / PRINCE hardware engine through an internal bus. Keys with
    /// index != 0 are returned for general use through the KEYOUTPUT register
    /// ... except for index 15!
    ///
    /// Key index 15 is a special case: The ROM uses it for the DICE UDS.
    /// Using this index for the UDS would be very bad given the semantics
    /// above however NXP mitigates this special case by having the ROM
    /// block use of index 15 in IDXBLK_H register which it then locks.
    /// IDXBLK_H can only be unlocked by POR. This is an effective mitigation
    /// that protects the UDS though it does have side effects.
    ///
    /// The ROM doesn't block indices 8 - 14 before IDXBLK_H is locked so they
    /// can be used. With the IDXBLK_H register locked by the ROM however
    /// they cannot be blocked which implies that code with access to the
    /// associated key code and the PUF will be able to access the key.
    pub fn set_key_index(&self, index: u32) -> bool {
        if index > 15 {
            return false;
        }

        // SAFETY: The PAC crate can't prevent us from setting the reserved
        // bits (the top 28) so this interface is unsafe. We ensure safety by
        // making index an unsigned type and the check above.
        self.puf.keyindex.write(|w| unsafe { w.bits(index) });

        true
    }

    /// Set the size (in bytes) of the key generated by the PUF or set through
    /// the API. Ths value is ignored for the GetKey command as the key size is
    /// baked into the KeyCode.
    pub fn set_key_size(&self, size: usize) -> bool {
        let size: u32 = ((size * 8) >> 6).try_into().unwrap_lite();
        if size < 32 {
            // SAFETY: The PAC crate can't prevent us from setting the reserved
            // bits (the top 27) so this interface is unsafe. We ensure safety
            // by using  the type system (index is an unsigned type) and the
            // check above.
            self.puf.keysize.write(|w| unsafe { w.bits(size) });

            true
        } else {
            false
        }
    }

    // wait for puf to accept last command submitted
    fn wait_for_cmd_accept(&self) -> bool {
        // cmd has been accepted if either the PUF becomes busy or there's an error
        while !self.is_busy() && !self.is_error() {}

        // if there was an error the cmd was rejected
        !self.is_error()
    }

    /// Block use of a particular PUF key index by setting and clearing the
    /// appropriate bits in either the IDXBLK_L or IDXBLK_H register. This
    /// function keeps the appropriate IDXBLK DP register in sync as well.
    pub fn block_index(&self, index: u32) -> bool {
        // SAFETY: The PAC crate can't prevent us from setting the IDXBLK
        // registers to an invalid state & so this interface is unsafe.
        // We ensure safety using the match statement. This mitigation
        // covers all 'unsafe' statements in this function.
        match index {
            0 => false,
            1..=7 => {
                let idxblk = self.puf.idxblk_l.read().bits();
                let idxblk = disable_index_bits(idxblk, index);
                self.puf.idxblk_l.write(|w| unsafe { w.bits(idxblk) });
                let idxblk = idxblk & 0xffff;
                self.puf.idxblk_l_dp.write(|w| unsafe { w.bits(idxblk) });

                true
            }
            8..=15 => {
                let idxblk = self.puf.idxblk_h.read().bits();
                let idxblk = disable_index_bits(idxblk, index);
                self.puf.idxblk_h.write(|w| unsafe { w.bits(idxblk) });
                let idxblk = idxblk & 0xffff;
                self.puf.idxblk_h_dp.write(|w| unsafe { w.bits(idxblk) });

                true
            }
            16.. => false,
        }
    }

    /// Unblock use of a particular PUF key index by setting and clearing
    /// the appropriate bits in either the IDXBLK_L or IDXBLK_H register.
    /// This function keeps the appropriate IDXBLK DP register in sync as
    /// well.
    pub fn unblock_index(&self, index: u32) -> bool {
        // SAFETY: The PAC crate can't prevent us from setting the IDXBLK
        // registers to an invalid state & so this interface is unsafe.
        // We ensure safety using the match statement. This mitigation
        // covers all 'unsafe' statements in this function.
        match index {
            0 => false,
            1..=7 => {
                let idxblk = self.puf.idxblk_l.read().bits();
                let idxblk = enable_index_bits(idxblk, index);
                self.puf.idxblk_l.write(|w| unsafe { w.bits(idxblk) });
                let idxblk = idxblk & 0xffffu32;
                self.puf.idxblk_l_dp.write(|w| unsafe { w.bits(idxblk) });

                true
            }
            8..=15 => {
                let idxblk = self.puf.idxblk_h.read().bits();
                let idxblk = enable_index_bits(idxblk, index);
                self.puf.idxblk_h.write(|w| unsafe { w.bits(idxblk) });
                let idxblk = idxblk & 0xffff_u32;
                self.puf.idxblk_h_dp.write(|w| unsafe { w.bits(idxblk) });

                true
            }
            16.. => false,
        }
    }

    /// Lock the IDXBLK_L register. This prevents changes to the PUF key
    /// index blocking registers until POR.
    pub fn lock_indices_low(&self) {
        self.puf
            .idxblk_l
            .modify(|r, w| unsafe { w.bits(r.bits() & !(1 << 31) | 1 << 30) });
    }

    /// Lock the IDXBLK_H register. This prevents changes to the PUF key
    /// index blocking registers until POR.
    pub fn lock_indices_high(&self) {
        self.puf
            .idxblk_h
            .modify(|r, w| unsafe { w.bits(r.bits() & !(1 << 31) | 1 << 30) });
    }

    pub fn get_idxblk_l(&self) -> u32 {
        self.puf.idxblk_l.read().bits()
    }

    pub fn get_idxblk_h(&self) -> u32 {
        self.puf.idxblk_h.read().bits()
    }

    pub fn is_index_blocked(&self, index: u32) -> bool {
        match index {
            1..=7 => self.puf.idxblk_l.read().bits() & (1 << (index * 2)) != 0,
            8..=15 => {
                let index = index - 8;
                self.puf.idxblk_h.read().bits() & (1 << (index * 2)) != 0
            }
            _ => panic!("invalid index"),
        }
    }

    fn get_lock_state(&self, idxblk: u32) -> Option<LockState> {
        LockState::from_u32(idxblk >> 30)
    }

    fn is_locked(&self, idxblk: u32) -> bool {
        match self.get_lock_state(idxblk) {
            Some(LockState::Locked) => true,
            _ => false,
        }
    }

    pub fn is_idxblk_l_locked(&self) -> bool {
        self.is_locked(self.puf.idxblk_l.read().bits())
    }

    pub fn is_idxblk_h_locked(&self) -> bool {
        self.is_locked(self.puf.idxblk_h.read().bits())
    }

    pub fn is_index_locked(&self, index: u32) -> bool {
        match index {
            1..=7 => self.is_idxblk_l_locked(),
            8..=15 => self.is_idxblk_h_locked(),
            _ => panic!("invalid index"),
        }
    }

    /// Read the contents of the 'busy' bit from the PUF 'stat' register.
    pub fn is_busy(&self) -> bool {
        self.puf.stat.read().busy().bit()
    }

    /// Read the contents of the 'error' bit from the PUF 'stat' register.
    pub fn is_error(&self) -> bool {
        self.puf.stat.read().error().bit()
    }

    /// Read the contents of the 'success' bit from the PUF 'stat' register.
    pub fn is_success(&self) -> bool {
        self.puf.stat.read().success().bit()
    }

    /// Read the contents of the 'error' bit from the PUF 'ifstat' register.
    pub fn is_ifstat_error(&self) -> bool {
        self.puf.ifstat.read().error().bit()
    }

    /// Read the contents of the 'allowenroll' bit from the PUF 'allow'
    /// register. This tells us whether or not the PUF ENROLL command is
    /// currently allowed.
    pub fn is_enroll_allowed(&self) -> bool {
        self.puf.allow.read().allowenroll().bit()
    }

    /// Read the contents of the 'allowstart' bit from the PUF 'allow'
    /// register. This tells us whether or not the PUF START command is
    /// currently allowed.
    pub fn is_start_allowed(&self) -> bool {
        self.puf.allow.read().allowstart().bit()
    }

    /// Read the contents of the 'allowsetkey' bit from the PUF 'allow'
    /// register. This tells us whether or not the PUF GENERATEKEY command
    /// is currently allowed.
    pub fn is_generatekey_allowed(&self) -> bool {
        // allowsetkey controls both 'setkey' and 'generatekey' operation
        self.puf.allow.read().allowsetkey().bit()
    }

    /// Read the contents of the 'allowgetkey' bit from the PUF 'allow'
    /// register. This tells us whether or not the PUF GETKEY command is
    /// currently allowed.
    pub fn is_getkey_allowed(&self) -> bool {
        self.puf.allow.read().allowgetkey().bit()
    }

    fn is_keycode_part_avail(&self) -> bool {
        self.puf.stat.read().codeoutavail().bit()
    }

    fn is_key_part_avail(&self) -> bool {
        self.puf.stat.read().keyoutavail().bit()
    }

    fn is_keycode_part_req(&self) -> bool {
        self.puf.stat.read().codeinreq().bit()
    }

    /// Return the state of the 'ramon' bit from the PUF 'pwrctrl'
    /// register. This tells us whether or not the PUF SRAM is powered.
    pub fn is_sram_on(&self) -> bool {
        self.puf.pwrctrl.read().ramon().bit()
    }

    /// Return the state of the 'ramstat' bit from the PUF 'pwrctrl'
    /// register. This tells us whether or not the PUF SRAM has been
    /// initialized.
    pub fn is_sram_ready(&self) -> bool {
        self.puf.pwrctrl.read().ramstat().bit()
    }

    /// Clear the 'ramon' bit from the PUF 'pwrctrl' register. This removes
    /// power from the PUF SRAM. In testing this also appears to cause the
    /// PUF to reset: ENROLL & START allowed, GENERATEKEY & GETKEY
    /// disallowed.
    pub fn disable_sram(&self) {
        self.puf.pwrctrl.write(|w| w.ramon().clear_bit());

        // Wait till hardware confirms PUF SRAM has been powered off.
        while self.is_sram_ready() {}
    }

    /// Enable PUF SRAM. UM11126 48.11.4 says once disabled, the PUF SRAM
    /// requires up to 400ms delay before it can be turned on again.
    pub fn enable_sram(&self) {
        self.puf.pwrctrl.write(|w| w.ramon().set_bit());
    }
}

// The PUF keycode holds some metadata including the key index. This
// function extracts the key index from the provided keycode.
fn index_from_keycode(keycode: &[u32]) -> u32 {
    if keycode.is_empty() {
        panic!("invalid keycode");
    }

    keycode[0] >> 8 & 0xf
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn index_from_kc1() {
        assert_eq!(index_from_keycode(&[0x4000101_u32]), 1);
    }

    #[test]
    fn index_from_kc3() {
        assert_eq!(index_from_keycode(&[0x4000301_u32]), 3);
    }

    #[test]
    fn index_from_kc9() {
        assert_eq!(index_from_keycode(&[0x4000901_u32]), 9);
    }

    #[test]
    fn key_8_bytes() {
        assert_eq!(Puf::key_to_keycode_len(8), 52)
    }

    #[test]
    fn key_32_bytes() {
        assert_eq!(Puf::key_to_keycode_len(32), 52)
    }

    #[test]
    fn key_40_bytes() {
        assert_eq!(Puf::key_to_keycode_len(40), 84)
    }

    #[test]
    fn key_64_bytes() {
        assert_eq!(Puf::key_to_keycode_len(64), 84)
    }

    #[test]
    fn key_72_bytes() {
        assert_eq!(Puf::key_to_keycode_len(72), 116)
    }

    #[test]
    fn key_96_bytes() {
        assert_eq!(Puf::key_to_keycode_len(96), 116)
    }
}
