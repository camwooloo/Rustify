//! librespot metadata -> wire types.

use librespot_metadata::audio::{AudioItem, UniqueFields};
use spotify_proto::{AlbumRef, ArtistRef, Track};

/// Covers arrive sorted widest-first, so the head is the best available art.
fn best_cover(item: &AudioItem) -> Option<String> {
    item.covers.first().map(|c| c.url.clone())
}

pub fn track_from_audio_item(item: &AudioItem) -> Track {
    let cover_url = best_cover(item);

    let (artists, album) = match &item.unique_fields {
        UniqueFields::Track {
            artists,
            album,
            ..
        } => {
            let artists = artists
                .0
                .iter()
                .map(|a| ArtistRef {
                    uri: a.id.to_uri().unwrap_or_default(),
                    name: a.name.clone(),
                })
                .collect();
            let album = AlbumRef {
                // AudioItem carries the album *name* only; the URI needs a
                // separate metadata fetch, so leave it empty rather than
                // fabricate one the UI would try to navigate to.
                uri: String::new(),
                name: album.clone(),
                cover_url: cover_url.clone(),
            };
            (artists, Some(album))
        }

        // Local files have free-form artist/album strings that cannot be
        // safely split into individual artists.
        UniqueFields::Local {
            artists, album, ..
        } => {
            let artists = artists
                .clone()
                .map(|name| {
                    vec![ArtistRef {
                        uri: String::new(),
                        name,
                    }]
                })
                .unwrap_or_default();
            let album = album.clone().map(|name| AlbumRef {
                uri: String::new(),
                name,
                cover_url: cover_url.clone(),
            });
            (artists, album)
        }

        // Podcasts: surface the show as the "album" so the now-playing bar
        // renders identically without special-casing in the UI.
        UniqueFields::Episode { show_name, .. } => (
            vec![ArtistRef {
                uri: String::new(),
                name: show_name.clone(),
            }],
            Some(AlbumRef {
                uri: String::new(),
                name: show_name.clone(),
                cover_url: cover_url.clone(),
            }),
        ),
    };

    Track {
        uri: item.uri.clone(),
        id: item.track_id.to_id().unwrap_or_default(),
        name: item.name.clone(),
        artists,
        album,
        duration_ms: item.duration_ms,
        explicit: item.is_explicit,
        saved: false, // filled in by the Web API layer when known
        cover_url,
        added_at: None,
    }
}
