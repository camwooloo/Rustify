//! Playback core: OAuth, the librespot session, and the Spotify Connect
//! endpoint. Knows nothing about IPC or UI.

pub mod auth;
mod convert;
pub mod engine;
pub mod eq;
pub mod spectrum;
pub mod zeroconf;

pub use auth::{begin_login, current_token, PendingLogin, StoredToken};
pub use engine::{new_session, Engine, EngineConfig};
