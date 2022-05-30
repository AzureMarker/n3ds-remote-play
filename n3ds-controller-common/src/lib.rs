//! Common types and functionality for the n3ds-controller project

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub enum InputMessage {
    Button {
        action: ButtonAction,
        button: Button,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ButtonAction {
    Pressed,
    Released,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Button {
    A,
}
