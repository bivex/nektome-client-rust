//! Reusable Rust implementation of the NektoMe client.
//!
//! The binary is intentionally a thin launcher; protocol, persistence, audio,
//! transport, and session code live here so they can be tested and reused by
//! alternate frontends.

pub mod app;
pub mod audio;
pub mod auth;
pub mod domain;
pub mod persistence;
pub mod protocol;
pub mod proxy_seller;
pub mod session_recorder;
pub mod transport;
pub mod turn_forwarder;
pub mod ui;
pub mod voice;
pub mod voice_changer;
pub mod voice_dsp;
pub mod voice_session;
pub mod webrtc_backend;
