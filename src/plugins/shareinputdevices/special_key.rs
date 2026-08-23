//! keysym → mousepad `specialKey` code.
//!
//! The producer emits `kdeconnect.mousepad.request` bodies carrying
//! `{key: text, specialKey: <code>, shift, ctrl, alt, super}`. Named
//! keys (Escape, Backspace, the arrows, F1-F12, …) have no printable
//! text, so `specialKey` is the only field that carries them; without
//! this table they reach the wire as `{key: "", specialKey: 0}`,
//! which neither phone consumer can act on. Until it landed, `ei.rs`
//! suppressed those events at the source rather than send noise.
//!
//! # Oracle
//!
//! Upstream composes two steps, and this module folds them into one:
//!
//! 1. `inputcapturesession.cpp:435` —
//!    `QXkbCommon::keysymToQtKey(sym, modifiers)` → `Qt::Key`.
//! 2. `shareinputdevicesplugin.cpp:28` — `QMap<int,int>
//!    specialKeysMap` → the wire code, via `specialKeysMap.value(key)`
//!    which yields **0** for any unmapped key (`QMap::value`'s default).
//!
//! Step 1's table is Qt's `KeyTbl` in
//! `qtbase/src/gui/platform/unix/qxkbcommon.cpp`, read at the 6.10
//! branch. Two of its behaviours are not guessable and are reproduced
//! deliberately here:
//!
//! - **The F-key range is checked BEFORE the table**
//!   (`keysymToQtKey_internal` :512-514): `F1..=F35` map by arithmetic,
//!   `Qt::Key_F1 + (keysym - XKB_KEY_F1)`. Only F1-F12 appear in
//!   `specialKeysMap`, so F13-F35 correctly yield 0.
//! - **Keypad variants fold onto their main key** (`KP_Left` →
//!   `Qt::Key_Left`, `KP_Enter` → `Qt::Key_Enter`, …), so they earn
//!   the same code. `XKB_KEY_Clear` folds onto `Qt::Key_Delete`
//!   (qxkbcommon.cpp :57), which is why it maps to 13.
//!
//! The codes themselves are the same 1-32 space `mousepad.rs`'s
//! `special_key_code` decodes in the RECEIVING direction; that
//! function is this table's mirror and the two must stay consistent.
//! Gaps are upstream's: 3 (Linefeed) and 17-20 (the four modifier
//! keys) are commented out in `specialKeysMap`, so nothing maps to
//! them here either.

use xkbcommon::xkb;
use xkbcommon::xkb::keysyms as ks;

/// Lowest `specialKey` code in the F-key run (`Qt::Key_F1` → 21).
const F1_CODE: i32 = 21;
/// Number of F-keys `specialKeysMap` covers: F1-F12.
const F_KEY_COUNT: u32 = 12;

/// The mousepad `specialKey` code for `keysym`, or 0 when the key has
/// none — matching `specialKeysMap.value(key)`'s default for every
/// key upstream leaves unmapped (printable characters, modifiers,
/// Insert, Print, Pause, F13 and up).
pub(crate) fn special_key_for_keysym(keysym: xkb::Keysym) -> i32 {
    let raw = keysym.raw();

    // Qt checks the F-key range first (qxkbcommon.cpp :512-514), so
    // this branch precedes the table here too. F13-F35 are real
    // Qt::Key values with no specialKey code: they fall through to 0.
    if (ks::KEY_F1..=ks::KEY_F35).contains(&raw) {
        let offset = raw - ks::KEY_F1;
        if offset < F_KEY_COUNT {
            #[allow(clippy::cast_possible_wrap)]
            return F1_CODE + offset as i32;
        }
        return 0;
    }

    match raw {
        ks::KEY_BackSpace => 1,
        ks::KEY_Tab | ks::KEY_KP_Tab => 2,
        // 3 is XK_Linefeed — commented out upstream, deliberately absent.
        ks::KEY_Left | ks::KEY_KP_Left => 4,
        ks::KEY_Up | ks::KEY_KP_Up => 5,
        ks::KEY_Right | ks::KEY_KP_Right => 6,
        ks::KEY_Down | ks::KEY_KP_Down => 7,
        // Prior/Next are the X11 names for Page_Up/Page_Down.
        ks::KEY_Prior | ks::KEY_KP_Prior => 8,
        ks::KEY_Next | ks::KEY_KP_Next => 9,
        ks::KEY_Home | ks::KEY_KP_Home => 10,
        ks::KEY_End | ks::KEY_KP_End => 11,
        // Qt::Key_Return and Qt::Key_Enter are distinct Qt keys that
        // specialKeysMap sends to the SAME code (plugin.cpp :41-42).
        ks::KEY_Return | ks::KEY_KP_Enter => 12,
        // Clear folds onto Qt::Key_Delete upstream (qxkbcommon.cpp :57).
        ks::KEY_Delete | ks::KEY_Clear | ks::KEY_KP_Delete => 13,
        ks::KEY_Escape => 14,
        // Qt also routes two vendor keysyms to Qt::Key_SysReq
        // (qxkbcommon.cpp :60-61): Sun and X386 SysReq.
        ks::KEY_Sys_Req | SUN_SYS_REQ | X386_SYS_REQ => 15,
        ks::KEY_Scroll_Lock => 16,
        // 17-20 are the four modifier keys, commented out upstream.
        _ => 0,
    }
}

/// Sun keyboards' SysReq keysym, hardcoded in Qt's table
/// (qxkbcommon.cpp :60).
const SUN_SYS_REQ: u32 = 0x1005_FF60;
/// X386 SysReq keysym, hardcoded in Qt's table (qxkbcommon.cpp :61).
const X386_SYS_REQ: u32 = 0x1007_FF00;

#[cfg(test)]
mod tests {
    use super::*;

    fn code(raw: u32) -> i32 {
        special_key_for_keysym(xkb::Keysym::new(raw))
    }

    /// Every code upstream's `specialKeysMap` defines, pinned to the
    /// keysym that must produce it.
    #[test]
    fn maps_every_upstream_special_key() {
        let expected: &[(u32, i32)] = &[
            (ks::KEY_BackSpace, 1),
            (ks::KEY_Tab, 2),
            (ks::KEY_Left, 4),
            (ks::KEY_Up, 5),
            (ks::KEY_Right, 6),
            (ks::KEY_Down, 7),
            (ks::KEY_Prior, 8),
            (ks::KEY_Next, 9),
            (ks::KEY_Home, 10),
            (ks::KEY_End, 11),
            (ks::KEY_Return, 12),
            (ks::KEY_Delete, 13),
            (ks::KEY_Escape, 14),
            (ks::KEY_Sys_Req, 15),
            (ks::KEY_Scroll_Lock, 16),
        ];
        for &(sym, want) in expected {
            assert_eq!(code(sym), want, "keysym {sym:#x} must map to {want}");
        }
    }

    /// F1-F12 occupy 21-32 by arithmetic, not by enumeration.
    #[test]
    fn f_keys_one_through_twelve_map_to_21_through_32() {
        for n in 0..12u32 {
            #[allow(clippy::cast_possible_wrap)]
            let want = 21 + n as i32;
            assert_eq!(code(ks::KEY_F1 + n), want, "F{} must map to {want}", n + 1);
        }
        assert_eq!(code(ks::KEY_F12), 32, "F12 is the last mapped F-key");
    }

    /// F13-F35 are valid Qt::Key values with no specialKey code. Qt's
    /// range check catches them before the table, and specialKeysMap
    /// has no entry, so they must yield 0 rather than running off the
    /// end of the 21-32 run.
    #[test]
    fn f_keys_thirteen_and_up_have_no_code() {
        assert_eq!(code(ks::KEY_F13), 0, "F13 is beyond specialKeysMap");
        assert_eq!(code(ks::KEY_F35), 0, "F35 is beyond specialKeysMap");
        for n in 12..35u32 {
            assert_eq!(code(ks::KEY_F1 + n), 0, "F{} must have no code", n + 1);
        }
    }

    /// Keypad variants fold onto their main key in Qt's KeyTbl, so
    /// they carry the same code. Guessing this wrong would silently
    /// drop every keypad navigation key.
    #[test]
    fn keypad_variants_share_their_main_key_code() {
        let pairs: &[(u32, u32)] = &[
            (ks::KEY_KP_Tab, ks::KEY_Tab),
            (ks::KEY_KP_Left, ks::KEY_Left),
            (ks::KEY_KP_Up, ks::KEY_Up),
            (ks::KEY_KP_Right, ks::KEY_Right),
            (ks::KEY_KP_Down, ks::KEY_Down),
            (ks::KEY_KP_Prior, ks::KEY_Prior),
            (ks::KEY_KP_Next, ks::KEY_Next),
            (ks::KEY_KP_Home, ks::KEY_Home),
            (ks::KEY_KP_End, ks::KEY_End),
            (ks::KEY_KP_Delete, ks::KEY_Delete),
        ];
        for &(kp, main) in pairs {
            assert_eq!(
                code(kp),
                code(main),
                "keypad {kp:#x} must share the code of {main:#x}"
            );
            assert_ne!(code(kp), 0, "keypad {kp:#x} must have a code at all");
        }
        // KP_Enter is Qt::Key_Enter, a DIFFERENT Qt key from
        // Qt::Key_Return that specialKeysMap sends to the same code.
        assert_eq!(code(ks::KEY_KP_Enter), 12);
    }

    /// Clear folds onto Qt::Key_Delete upstream — not obvious, and
    /// wrong in the other direction would send a stray delete.
    #[test]
    fn clear_folds_onto_delete() {
        assert_eq!(code(ks::KEY_Clear), 13);
    }

    /// The vendor SysReq keysyms Qt hardcodes.
    #[test]
    fn vendor_sysreq_keysyms_map_to_sysreq() {
        assert_eq!(code(SUN_SYS_REQ), 15);
        assert_eq!(code(X386_SYS_REQ), 15);
    }

    /// The gaps are upstream's, and must stay gaps: Linefeed (3) and
    /// the four modifier keys (17-20) are commented out in
    /// specialKeysMap, so nothing may claim those codes.
    #[test]
    fn upstream_gaps_stay_unmapped() {
        assert_eq!(code(ks::KEY_Linefeed), 0, "3 is commented out upstream");
        for sym in [
            ks::KEY_Control_L,
            ks::KEY_Control_R,
            ks::KEY_Alt_L,
            ks::KEY_Alt_R,
            ks::KEY_Shift_L,
            ks::KEY_Shift_R,
            ks::KEY_Super_L,
            ks::KEY_Super_R,
        ] {
            assert_eq!(code(sym), 0, "modifier {sym:#x} has no specialKey");
        }
    }

    /// Printable keys carry their text, never a code. NoSymbol is 0
    /// and must not collide with a real mapping.
    #[test]
    fn printable_and_nosymbol_have_no_code() {
        for sym in [ks::KEY_a, ks::KEY_A, ks::KEY_0, ks::KEY_space, ks::KEY_plus] {
            assert_eq!(code(sym), 0, "printable {sym:#x} must have no code");
        }
        assert_eq!(code(0), 0, "NoSymbol must have no code");
    }

    /// Keys upstream deliberately left out of specialKeysMap even
    /// though Qt maps them to real Qt::Key values.
    #[test]
    fn qt_mapped_but_unlisted_keys_have_no_code() {
        for sym in [ks::KEY_Insert, ks::KEY_Print, ks::KEY_Pause, ks::KEY_Menu] {
            assert_eq!(code(sym), 0, "{sym:#x} is absent from specialKeysMap");
        }
    }

    /// No two distinct codes may be produced for one keysym, and every
    /// code produced must be inside the 1-32 wire space that
    /// `mousepad::special_key_code` decodes.
    #[test]
    fn all_produced_codes_are_in_the_decodable_range() {
        for raw in 0..0xffff_u32 {
            let c = code(raw);
            assert!(
                c == 0 || (1..=32).contains(&c),
                "keysym {raw:#x} produced out-of-range code {c}"
            );
        }
    }
}
