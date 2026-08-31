//! Spotify Connect devices on the local network.
//!
//! The Web API only lists devices already signed in to the account. A speaker
//! sitting idle — an Echo, a receiver, a phone with Spotify installed —
//! advertises itself over mDNS as `_spotify-connect._tcp` and waits to be
//! told who to log in as. The official client finds them that way; without
//! this, Rustify simply could not see them.
//!
//! Signing one in is a small handshake. Ask it for its public key, agree a
//! shared secret by Diffie-Hellman, encrypt an access token with keys derived
//! from it, and post that back. librespot already implements the receiving
//! half of exactly this — it is how Rustify advertises itself — so what
//! follows is that protocol read from the other end.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aes::cipher::{KeyIvInit, StreamCipher};
use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use hmac::{Hmac, Mac};
use librespot_core::diffie_hellman::DhLocalKeys;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use tracing::debug;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

const SERVICE: &str = "_spotify-connect._tcp.local.";

/// A speaker found on the network, whether or not it is signed in.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    /// Spotify's id for the device, which is what the Web API calls it once
    /// it is signed in — so the two lists can be matched up.
    pub device_id: String,
    pub name: String,
    pub device_type: String,
    /// Where to send the handshake.
    pub endpoint: String,
    /// Already logged in as somebody.
    pub active_user: String,
    /// "accesstoken" devices take a token; the rest want a credentials blob,
    /// which this does not build.
    pub token_type: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Info {
    // Not `deviceId`: Spotify spells this one with both letters capitalised,
    // so camelCase renaming alone reads it as absent and every device looks
    // like it has no id.
    #[serde(default, rename = "deviceID")]
    device_id: String,
    #[serde(default)]
    remote_name: String,
    #[serde(default)]
    device_type: String,
    #[serde(default)]
    public_key: String,
    #[serde(default)]
    active_user: String,
    #[serde(default)]
    token_type: String,
    #[serde(default)]
    brand_display_name: String,
    #[serde(default)]
    model_display_name: String,
    // Devices that take a token want their own client id handed back to
    // them, spelled — like `deviceID` — with both letters capitalised.
    #[serde(default, rename = "clientID")]
    client_id: String,
    // Some devices care that the version they were told matches the one
    // they announced, so it is echoed rather than assumed.
    #[serde(default)]
    version: String,
}

impl Info {
    /// What to call it on screen.
    ///
    /// An idle speaker often answers with an empty `remoteName`, because the
    /// name people know it by belongs to the account rather than the device.
    /// The model is a better answer than a blank line.
    fn display_name(&self) -> String {
        let trimmed = self.remote_name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }

        let model = self.model_display_name.replace('_', " ");
        match (self.brand_display_name.trim(), model.trim()) {
            ("", "") => "Speaker".to_string(),
            ("", model) => model.to_string(),
            (brand, "") => brand.to_string(),
            (brand, model) if model.starts_with(brand) => model.to_string(),
            (brand, model) => format!("{brand} {model}"),
        }
    }
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("building the discovery client")
}

/// Ask one device who it is.
async fn get_info(endpoint: &str) -> Result<Info> {
    let url = format!("{endpoint}?action=getInfo&version=2.9.0");
    let info: Info = client()?
        .get(&url)
        .send()
        .await
        .context("asking the device for its details")?
        .error_for_status()
        .context("the device refused the request")?
        .json()
        .await
        .context("reading the device's details")?;
    Ok(info)
}

/// Choose an address to talk to a speaker on, ready to put in a URL.
///
/// IPv4 first: a link-local IPv6 address needs a zone index to be routable at
/// all, and the same speaker almost always answers on both. A global IPv6
/// address is a fine fallback, but it has to be bracketed or the URL is not a
/// URL — which is how this failed the first time, silently, on every device.
fn pick_address(addresses: &std::collections::HashSet<std::net::IpAddr>) -> Option<String> {
    if let Some(v4) = addresses.iter().find(|a| a.is_ipv4()) {
        return Some(v4.to_string());
    }

    addresses
        .iter()
        .find(|a| match a {
            std::net::IpAddr::V6(v6) => !v6.is_unicast_link_local() && !v6.is_loopback(),
            _ => false,
        })
        .map(|v6| format!("[{v6}]"))
}

/// A standing mDNS browse.
///
/// Started once and left running, because a browse started per click is both
/// slower and worse: speakers answer at their own pace, and a fresh two-second
/// window catches whichever happen to reply inside it — one of four, on a real
/// network. Listening continuously means the list is complete by the time
/// anybody opens it, and reading it costs nothing.
#[derive(Clone)]
pub struct Browser {
    endpoints: Arc<Mutex<HashMap<String, String>>>,
}

impl Browser {
    /// Begin listening. The browse runs for the life of the process.
    pub fn start() -> Result<Self> {
        let daemon = ServiceDaemon::new().context("starting mDNS")?;
        let receiver = daemon.browse(SERVICE).context("browsing for speakers")?;
        let endpoints: Arc<Mutex<HashMap<String, String>>> = Default::default();

        let seen = endpoints.clone();
        tokio::spawn(async move {
            // Held for as long as the browse: dropping the daemon ends it.
            let _daemon = daemon;

            while let Ok(event) = receiver.recv_async().await {
                match event {
                    ServiceEvent::ServiceResolved(service) => {
                        let Some(host) = pick_address(service.get_addresses()) else {
                            continue;
                        };
                        // `CPath` says where the endpoint lives; it is not
                        // always the same path, so it is read rather than
                        // assumed.
                        let path = service
                            .get_property_val_str("CPath")
                            .unwrap_or("/spotifyConnect");
                        let endpoint =
                            format!("http://{host}:{}{path}", service.get_port());

                        if let Ok(mut map) = seen.lock() {
                            map.insert(service.get_fullname().to_string(), endpoint);
                        }
                    }
                    // A speaker that goes away should stop being offered.
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        if let Ok(mut map) = seen.lock() {
                            map.remove(&fullname);
                        }
                    }
                    _ => {}
                }
            }

            debug!("mDNS browse ended");
        });

        Ok(Self { endpoints })
    }

    /// Everything heard from so far, asked who it is.
    ///
    /// Devices that do not answer are left out rather than listed as broken:
    /// an address stale in the mDNS cache is normal, not an error to show.
    pub async fn devices(&self) -> Vec<Found> {
        let endpoints: Vec<String> = match self.endpoints.lock() {
            Ok(map) => map.values().cloned().collect(),
            Err(poisoned) => poisoned.into_inner().values().cloned().collect(),
        };

        // All at once: these are local addresses, and one slow speaker should
        // not hold up the rest of the list.
        let answers = futures_util::future::join_all(
            endpoints
                .into_iter()
                .map(|endpoint| async move { (get_info(&endpoint).await, endpoint) }),
        )
        .await;

        let mut found: Vec<Found> = Vec::new();

        for (info, endpoint) in answers {
            match info {
                Ok(info) if !info.device_id.is_empty() => {
                    if found.iter().any(|f| f.device_id == info.device_id) {
                        continue;
                    }
                    found.push(Found {
                        name: info.display_name(),
                        device_id: info.device_id,
                        device_type: info.device_type.to_lowercase(),
                        endpoint,
                        active_user: info.active_user,
                        token_type: info.token_type,
                    });
                }
                Ok(_) => debug!("{endpoint}: answered without a device id"),
                Err(e) => debug!("{endpoint}: {e:#}"),
            }
        }

        found.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        found
    }
}

/// Encrypt `payload` for a device, given its public key.
///
/// The scheme is librespot's, read from the receiving end: agree a secret by
/// Diffie-Hellman, take SHA-1 of it as a base key, derive separate checksum
/// and encryption keys from that, then send iv ‖ ciphertext ‖ mac.
fn seal(payload: &[u8], device_public_key: &[u8]) -> Result<(String, String)> {
    let keys = DhLocalKeys::random(&mut rand::rng());
    let shared = keys.shared_secret(device_public_key);

    let base_key = Sha1::digest(shared);
    let base_key = &base_key[..16];

    let derive = |label: &[u8]| -> Result<Vec<u8>> {
        let mut mac = Hmac::<Sha1>::new_from_slice(base_key)
            .map_err(|_| anyhow!("bad key length for HMAC"))?;
        mac.update(label);
        Ok(mac.finalize().into_bytes().to_vec())
    };

    let checksum_key = derive(b"checksum")?;
    let encryption_key = derive(b"encryption")?;

    let mut iv = [0u8; 16];
    rand::rng().fill_bytes(&mut iv);

    let mut encrypted = payload.to_vec();
    let mut cipher = Aes128Ctr::new_from_slices(&encryption_key[0..16], &iv)
        .map_err(|e| anyhow!("preparing the cipher: {e}"))?;
    cipher.apply_keystream(&mut encrypted);

    let mac = {
        let mut mac = Hmac::<Sha1>::new_from_slice(&checksum_key)
            .map_err(|_| anyhow!("bad key length for HMAC"))?;
        mac.update(&encrypted);
        mac.finalize().into_bytes()
    };

    let mut blob = Vec::with_capacity(iv.len() + encrypted.len() + mac.len());
    blob.extend_from_slice(&iv);
    blob.extend_from_slice(&encrypted);
    blob.extend_from_slice(&mac);

    Ok((BASE64.encode(blob), BASE64.encode(keys.public_key())))
}

/// Spotify's authentication type for "these bytes are an access token",
/// which is what goes into a credentials blob here.
const AUTH_SPOTIFY_TOKEN: u32 = 3;

/// Append a length-prefixed field, in the varint form the blob uses.
fn write_bytes(out: &mut Vec<u8>, value: &[u8]) {
    write_int(out, value.len() as u32);
    out.extend_from_slice(value);
}

fn write_int(out: &mut Vec<u8>, value: u32) {
    if value < 0x80 {
        out.push(value as u8);
    } else {
        out.push(0x80 | (value & 0x7f) as u8);
        out.push((value >> 7) as u8);
    }
}

/// Build the credentials blob a non-token device expects to find inside the
/// sealed envelope.
///
/// This is librespot's `Credentials::with_blob` run backwards, and it is
/// tested that way: fields with their eye-catchers, block padding, a rolling
/// XOR over the tail, then AES-192 in ECB under a key stretched from the
/// *target device's* id — so a blob built for one speaker means nothing to
/// another.
fn credentials_blob(
    username: &str,
    auth_type: u32,
    auth_data: &[u8],
    device_id: &str,
) -> Result<String> {
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncrypt, KeyInit};

    let mut blob: Vec<u8> = Vec::new();
    blob.push(0x49);
    write_bytes(&mut blob, username.as_bytes());
    blob.push(0x50);
    write_int(&mut blob, auth_type);
    blob.push(0x51);
    write_bytes(&mut blob, auth_data);

    // Pad out to whole blocks, the last byte saying how much was added.
    let zeros = 16 - (blob.len() % 16) - 1;
    blob.extend(std::iter::repeat(0u8).take(zeros));
    blob.push((zeros + 1) as u8);

    for i in 16..blob.len() {
        blob[i] ^= blob[i - 16];
    }

    let secret = Sha1::digest(device_id.as_bytes());

    let key = {
        let mut key = [0u8; 24];
        pbkdf2::pbkdf2_hmac::<Sha1>(&secret, username.as_bytes(), 0x100, &mut key[0..20]);
        let hash = Sha1::digest(&key[..20]);
        key[..20].copy_from_slice(&hash);
        key[20..].copy_from_slice(&20u32.to_be_bytes());
        key
    };

    let cipher = aes::Aes192::new(GenericArray::from_slice(&key));
    for chunk in blob.chunks_exact_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
    }

    Ok(BASE64.encode(blob))
}

/// Sign a speaker in to the account, so it joins Spotify Connect.
///
/// There are two families, and they take opposite things. A device whose
/// `getInfo` says `accesstoken` — every Amazon one does — wants the token in
/// plain text and its own client id handed back in `clientKey`; there is no
/// Diffie-Hellman in that flow at all, despite the key such a device
/// publishes. Everything else wants a credentials blob built for that
/// particular device, inside an envelope sealed with the key exchange.
/// One family is known not to work, and it is worth writing down so the next
/// person does not spend an afternoon on it: Amazon's speakers answer every
/// well-formed sign-in with a bare HTTP 500, in about fifteen milliseconds —
/// far too quickly to have asked Spotify about the token. An empty blob gets
/// a polite `ERROR-BAD-REQUEST` and a mis-spelled token type a different one
/// again, so the request itself is understood; it is the sign-in they refuse.
/// Tried, and all identical: tokens from login5, from keymaster under the
/// device's own client id, and OAuth tokens for Spotify's desktop client;
/// plain, bearer-prefixed and JSON payloads; sealed and unsealed blobs; with
/// and without every optional parameter; and the headers the official client
/// sends. Those speakers still list and still play once something else has
/// woken them, which is what the message below tells whoever clicked.
pub async fn wake(endpoint: &str, username: &str, access_token: &str) -> Result<()> {
    let info = get_info(endpoint).await?;

    let version = if info.version.is_empty() {
        "2.9.0".to_string()
    } else {
        info.version.clone()
    };

    let mut form: Vec<(&str, String)> = vec![
        ("action", "addUser".to_string()),
        ("version", version),
        ("loginId", username.to_string()),
        ("userName", username.to_string()),
    ];

    if info.token_type == "accesstoken" {
        form.push(("tokenType", "accesstoken".to_string()));
        form.push(("clientKey", info.client_id.clone()));
        form.push(("blob", access_token.to_string()));
    } else {
        let public_key = BASE64
            .decode(info.public_key.as_bytes())
            .context("the device's public key was not valid base64")?;

        let credentials = credentials_blob(
            username,
            AUTH_SPOTIFY_TOKEN,
            access_token.as_bytes(),
            &info.device_id,
        )?;
        let (blob, client_key) = seal(credentials.as_bytes(), &public_key)?;

        // librespot reads none of this, but the speakers built on it are
        // told who is signing them in, and left an empty token type.
        let librespot = matches!(
            info.model_display_name.to_lowercase().as_str(),
            "librespot" | "go-librespot"
        );

        form.push((
            "tokenType",
            if librespot {
                String::new()
            } else {
                "default".to_string()
            },
        ));
        form.push(("clientKey", client_key));
        form.push(("blob", blob));

        if librespot {
            form.push(("deviceName", "Rustify".to_string()));
            form.push(("deviceId", hex::encode(Sha1::digest(b"Rustify"))));
        }
    }

    let response = client()?
        .post(endpoint)
        .header("Connection", "close")
        .form(&form)
        .send()
        .await
        .context("sending the sign-in to the device")?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    // A device answers 200 with its own status inside the body, so the HTTP
    // code alone does not say whether it worked.
    let reported: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let code = reported.get("status").and_then(|v| v.as_i64()).unwrap_or(-1);
    let message = reported
        .get("statusString")
        .and_then(|v| v.as_str())
        .unwrap_or("no reason given");

    if !status.is_success() {
        return Err(anyhow!(
            "{} refused the sign-in ({status}). Some speakers only accept one from Spotify's own app — start it playing there once and it will stay on this list",
            info.display_name()
        ));
    }
    // 101 is the documented success code for addUser; some devices send 0.
    if code != 101 && code != 0 {
        return Err(anyhow!("{}: {message}", info.display_name()));
    }

    debug!("signed {} in to Spotify Connect", info.display_name());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(remote: &str, brand: &str, model: &str) -> Info {
        Info {
            device_id: "x".into(),
            remote_name: remote.into(),
            device_type: "SPEAKER".into(),
            public_key: String::new(),
            active_user: String::new(),
            token_type: "accesstoken".into(),
            brand_display_name: brand.into(),
            model_display_name: model.into(),
            client_id: String::new(),
            version: String::new(),
        }
    }

    #[test]
    fn an_idle_speaker_is_named_after_its_model() {
        // What an Echo actually answers: no remote name until it is signed in.
        assert_eq!(info("", "Amazon", "Echo_Show").display_name(), "Amazon Echo Show");
        assert_eq!(info("", "", "Echo_Spot").display_name(), "Echo Spot");
        assert_eq!(info("", "Sonos", "").display_name(), "Sonos");
        assert_eq!(info("", "", "").display_name(), "Speaker");
    }

    #[test]
    fn a_name_the_device_gives_wins() {
        assert_eq!(info("Dining Room", "Amazon", "Echo_Show").display_name(), "Dining Room");
        assert_eq!(info("  Kitchen  ", "", "").display_name(), "Kitchen");
    }

    #[test]
    fn a_brand_already_in_the_model_is_not_said_twice() {
        assert_eq!(info("", "Sonos", "Sonos One").display_name(), "Sonos One");
    }

    /// The credentials blob is only right if the receiving half can take it
    /// apart, so it is checked against the code that does: librespot's own
    /// `Credentials::with_blob`, which is what a librespot-based speaker
    /// runs when the envelope is opened.
    #[test]
    fn librespot_can_read_the_credentials_blob_we_build() {
        use librespot_core::authentication::Credentials;

        let device_id = "1234567890abcdef1234567890abcdef12345678";
        let token = "BQAyXsNF0-an-access-token-of-some-length";

        let blob = credentials_blob("vkarmahdv", AUTH_SPOTIFY_TOKEN, token.as_bytes(), device_id)
            .expect("built");

        let read = Credentials::with_blob("vkarmahdv", &blob, device_id).expect("read back");

        assert_eq!(read.username.as_deref(), Some("vkarmahdv"));
        assert_eq!(read.auth_data, token.as_bytes());
    }

    /// A blob is built for one device and is meaningless to any other, which
    /// is the whole point of stretching the key from the device id.
    #[test]
    fn a_blob_built_for_one_device_will_not_open_on_another() {
        use librespot_core::authentication::Credentials;

        let blob = credentials_blob("vkarmahdv", AUTH_SPOTIFY_TOKEN, b"a-token", "device-one")
            .expect("built");

        assert!(Credentials::with_blob("vkarmahdv", &blob, "device-two").is_err());
    }

    /// The sealed blob has to be laid out exactly as the receiving half
    /// expects to take it apart: iv, then ciphertext, then a 20-byte mac.
    #[test]
    fn a_sealed_blob_is_shaped_the_way_the_other_end_unpacks_it() {
        let theirs = DhLocalKeys::random(&mut rand::rng());
        let (blob, client_key) = seal(b"a-token", &theirs.public_key()).expect("sealed");

        let raw = BASE64.decode(blob).expect("base64");
        assert_eq!(raw.len(), 16 + b"a-token".len() + 20);
        assert!(!BASE64.decode(client_key).expect("base64").is_empty());
    }

    /// And the far end, doing what librespot's server does, gets the payload
    /// back out.
    #[test]
    fn the_far_end_can_open_it() {
        let theirs = DhLocalKeys::random(&mut rand::rng());
        let payload = b"BQCVR6-an-access-token";

        let (blob, client_key) = seal(payload, &theirs.public_key()).expect("sealed");
        let raw = BASE64.decode(blob).expect("base64");
        let client_key = BASE64.decode(client_key).expect("base64");

        // Exactly the steps in librespot's handle_add_user.
        let shared = theirs.shared_secret(&client_key);
        let base_key = Sha1::digest(shared);
        let base_key = &base_key[..16];

        let mut mac = Hmac::<Sha1>::new_from_slice(base_key).unwrap();
        mac.update(b"checksum");
        let checksum_key = mac.finalize().into_bytes();

        let mut mac = Hmac::<Sha1>::new_from_slice(base_key).unwrap();
        mac.update(b"encryption");
        let encryption_key = mac.finalize().into_bytes();

        let iv = &raw[0..16];
        let encrypted = &raw[16..raw.len() - 20];
        let cksum = &raw[raw.len() - 20..];

        let mut mac = Hmac::<Sha1>::new_from_slice(&checksum_key).unwrap();
        mac.update(encrypted);
        mac.verify_slice(cksum).expect("the mac must check out");

        let mut decrypted = encrypted.to_vec();
        let mut cipher = Aes128Ctr::new_from_slices(&encryption_key[0..16], iv).unwrap();
        cipher.apply_keystream(&mut decrypted);

        assert_eq!(decrypted, payload);
    }
}
