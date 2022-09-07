//! Common types and functionality for the n3ds-controller project

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Copy, Clone, Debug)]
pub enum InputMessage {
    Button {
        action: ButtonAction,
        button: Button,
    },
    CirclePadPosition(i16, i16),
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug)]
pub enum ButtonAction {
    Pressed,
    Released,
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug)]
pub enum Button {
    A,
    B,
    X,
    Y,
    L,
    R,
    ZL,
    ZR,
    Up,
    Down,
    Left,
    Right,
    Start,
    Select,
}
