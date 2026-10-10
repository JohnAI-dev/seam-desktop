//! Seam core: everything that talks to the phone, kept free of any UI code so it can be
//! tested on its own.

pub mod adb;
pub mod link;
pub mod scrcpy;
pub mod tools;
pub mod wireless;
