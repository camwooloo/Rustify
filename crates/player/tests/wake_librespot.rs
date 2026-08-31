//! Sign in a real Spotify Connect receiver, end to end.
//!
//! librespot's discovery server is the receiving half of this protocol — the
//! same code a Raspberry Pi speaker, spotifyd or go-librespot runs — so
//! waking one is the closest thing to a real device short of buying one.
//! Nothing is faked here: the server generates its own keys, and the only
//! thing it is given is the request Rustify would send to a speaker.
//!
//! Ignored by default because it binds a port and advertises itself over
//! mDNS, which not every machine or build agent allows.
//!
//!   cargo test -p spotify-player-core --test wake_librespot -- --ignored

use futures_util::StreamExt;
use librespot_protocol::authentication::AuthenticationType;

#[tokio::test]
#[ignore = "binds a port and advertises over mDNS"]
async fn a_librespot_speaker_accepts_our_sign_in() {
    let device_id = "0123456789abcdef0123456789abcdef01234567";
    let port = 4472;

    let mut speaker = librespot_discovery::Discovery::builder(device_id, "test-client")
        .name("Test Speaker")
        .port(port)
        .launch()
        .expect("starting a librespot receiver");

    let endpoint = format!("http://127.0.0.1:{port}/");
    let token = "BQAyXsNF0-a-perfectly-ordinary-access-token";

    spotify_player_core::zeroconf::wake(&endpoint, "vkarmahdv", token)
        .await
        .expect("the speaker should have accepted the sign-in");

    // The receiver hands whoever it was signed in as to its owner, which is
    // what Rustify is really asking it to do.
    let credentials = tokio::time::timeout(std::time::Duration::from_secs(5), speaker.next())
        .await
        .expect("the speaker should have reported a login")
        .expect("the discovery stream should still be open");

    assert_eq!(credentials.username.as_deref(), Some("vkarmahdv"));
    assert_eq!(credentials.auth_data, token.as_bytes());
    assert_eq!(
        credentials.auth_type,
        AuthenticationType::AUTHENTICATION_SPOTIFY_TOKEN
    );
}
