//! rspotify models -> wire types.
//!
//! Kept in one place so the daemon and UI never see an rspotify type. That
//! matters more than it looks: several Web API endpoints were deprecated for
//! new apps, and confining the blast radius to this file makes swapping an
//! endpoint a local change.

use rspotify::model::{
    FullAlbum, FullArtist, FullTrack, Image, PlayableItem, PlaylistItem, SimplifiedAlbum,
    SimplifiedArtist, SimplifiedPlaylist, SimplifiedTrack,
};
use rspotify::prelude::*;
use spotify_proto as wire;

/// Spotify returns images widest-first, but that is not contractual, so pick
/// the largest explicitly.
fn largest(images: &[Image]) -> Option<String> {
    images
        .iter()
        .max_by_key(|i| i.width.unwrap_or(0))
        .map(|i| i.url.clone())
}

pub fn artist_ref(a: &SimplifiedArtist) -> wire::ArtistRef {
    wire::ArtistRef {
        uri: a
            .id
            .as_ref()
            .map(|i| i.to_string())
            .unwrap_or_default(),
        name: a.name.clone(),
    }
}

pub fn album_ref(a: &SimplifiedAlbum) -> wire::AlbumRef {
    wire::AlbumRef {
        uri: a.id.as_ref().map(|i| i.to_string()).unwrap_or_default(),
        name: a.name.clone(),
        cover_url: largest(&a.images),
    }
}

pub fn full_track(t: &FullTrack) -> wire::Track {
    wire::Track {
        uri: t.id.as_ref().map(|i| i.to_string()).unwrap_or_default(),
        id: t.id.as_ref().map(|i| i.id().to_string()).unwrap_or_default(),
        name: t.name.clone(),
        artists: t.artists.iter().map(artist_ref).collect(),
        album: Some(album_ref(&t.album)),
        duration_ms: t.duration.num_milliseconds().max(0) as u32,
        explicit: t.explicit,
        saved: false,
        cover_url: largest(&t.album.images),
        added_at: None,
    }
}

/// Album-context tracks carry no album of their own, so the caller supplies it.
pub fn simplified_track(t: &SimplifiedTrack, album: Option<&wire::AlbumRef>) -> wire::Track {
    wire::Track {
        uri: t.id.as_ref().map(|i| i.to_string()).unwrap_or_default(),
        id: t.id.as_ref().map(|i| i.id().to_string()).unwrap_or_default(),
        name: t.name.clone(),
        artists: t.artists.iter().map(artist_ref).collect(),
        album: album.cloned(),
        duration_ms: t.duration.num_milliseconds().max(0) as u32,
        explicit: t.explicit,
        saved: false,
        cover_url: album.and_then(|a| a.cover_url.clone()),
        added_at: None,
    }
}

/// Playlists can contain episodes and unavailable entries; both come back as
/// `None` so the caller can filter them out rather than render placeholders.
pub fn playlist_item(item: &PlaylistItem) -> Option<wire::Track> {
    let added_at = item.added_at.map(|d| d.format("%d %b %Y").to_string());

    // Spotify renamed `track` to `item` once playlists could hold episodes.
    let mut track = match item.item.as_ref()? {
        PlayableItem::Track(t) => Some(full_track(t)),
        PlayableItem::Episode(e) => Some(wire::Track {
            uri: e.id.to_string(),
            id: e.id.id().to_string(),
            name: e.name.clone(),
            artists: vec![wire::ArtistRef {
                uri: String::new(),
                name: e.show.name.clone(),
            }],
            album: None,
            duration_ms: e.duration.num_milliseconds().max(0) as u32,
            explicit: e.explicit,
            saved: false,
            cover_url: largest(&e.images),
            added_at: None,
        }),
        // Spotify occasionally introduces new playable kinds; skip rather
        // than render a row that cannot be played.
        PlayableItem::Unknown(_) => None,
    }?;

    track.added_at = added_at;
    Some(track)
}

pub fn full_album(a: &FullAlbum) -> wire::Album {
    let cover_url = largest(&a.images);
    let self_ref = wire::AlbumRef {
        uri: a.id.to_string(),
        name: a.name.clone(),
        cover_url: cover_url.clone(),
    };
    wire::Album {
        uri: a.id.to_string(),
        id: a.id.id().to_string(),
        name: a.name.clone(),
        artists: a.artists.iter().map(artist_ref).collect(),
        cover_url,
        release_date: Some(a.release_date.clone()),
        total_tracks: a.tracks.total,
        tracks: a
            .tracks
            .items
            .iter()
            .map(|t| simplified_track(t, Some(&self_ref)))
            .collect(),
    }
}

pub fn simplified_album(a: &SimplifiedAlbum) -> wire::Album {
    wire::Album {
        uri: a.id.as_ref().map(|i| i.to_string()).unwrap_or_default(),
        id: a.id.as_ref().map(|i| i.id().to_string()).unwrap_or_default(),
        name: a.name.clone(),
        artists: a.artists.iter().map(artist_ref).collect(),
        cover_url: largest(&a.images),
        release_date: a.release_date.clone(),
        total_tracks: 0,
        tracks: Vec::new(),
    }
}

/// Note the fields we deliberately do not populate: Spotify removed
/// `followers`, `genres`, and the artist top-tracks endpoint from the Web API.
/// Reading the deprecated struct fields would yield zeros, so the artist page
/// is built from albums instead. See `README.md` for the full gap list.
pub fn full_artist(a: &FullArtist) -> wire::Artist {
    wire::Artist {
        uri: a.id.to_string(),
        id: a.id.id().to_string(),
        name: a.name.clone(),
        image_url: largest(&a.images),
        followers: 0,
        genres: Vec::new(),
        top_tracks: Vec::new(),
        albums: Vec::new(),
    }
}

pub fn simplified_playlist(p: &SimplifiedPlaylist) -> wire::Playlist {
    wire::Playlist {
        uri: p.id.to_string(),
        id: p.id.id().to_string(),
        name: p.name.clone(),
        owner: p
            .owner
            .display_name
            .clone()
            .unwrap_or_else(|| p.owner.id.to_string()),
        description: None,
        cover_url: largest(&p.images),
        total_tracks: p.items.total,
    }
}
