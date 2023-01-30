//! Common types and functionality for the n3ds-controller project

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default)]
pub struct InputState {
    pub key_pad: KeyPad,
    pub circle_pad: CirclePad,
    pub c_stick: CStick,
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct CirclePad {
    pub x: i16,
    pub y: i16,
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct CStick {
    pub x: u16,
    pub y: u16,
}

bitflags::bitflags! {
    /// A set of flags corresponding to the button and directional pad
    /// inputs on the 3DS.
    ///
    /// This is a modified copy of `KeyPad` from ctru-rs.
    #[derive(Serialize, Deserialize, Default)]
    pub struct KeyPad: u32 {
        const KEY_A             = 1u32 << 0;
        const KEY_B             = 1u32 << 1;
        const KEY_SELECT        = 1u32 << 2;
        const KEY_START         = 1u32 << 3;
        const KEY_DRIGHT        = 1u32 << 4;
        const KEY_DLEFT         = 1u32 << 5;
        const KEY_DUP           = 1u32 << 6;
        const KEY_DDOWN         = 1u32 << 7;
        const KEY_R             = 1u32 << 8;
        const KEY_L             = 1u32 << 9;
        const KEY_X             = 1u32 << 10;
        const KEY_Y             = 1u32 << 11;
        const KEY_ZL            = 1u32 << 14;
        const KEY_ZR            = 1u32 << 15;
    }
}
