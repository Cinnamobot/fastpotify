---
title: Getting Started
description: Install the app, sign in through your browser, and enable playback on this computer.
nav_order: 2
---

## Install

The [Download page](/download/) has the Windows installer and portable zip.

Or build from source with [Rust](https://rustup.rs) 1.95 or newer:

```sh
git clone https://github.com/Cinnamobot/fastpotify
cd fastpotify
cargo install --path . --bin fastpotify
```

MilkDrop builds libprojectM from source, so a Windows checkout needs CMake, a
C++ compiler, libclang, and vcpkg with `glew:x64-windows-static` installed.
Use `--no-default-features` to leave MilkDrop out and skip all of that.

On `main`, after 0.7.1, letters from system fallback fonts share the Latin
baseline. This keeps mixed Japanese and Latin titles aligned when the system
selects a CJK face, including when Japanese is a preferred language.

Spotifast uses system fonts for scripts that its interface font does not
cover, including Chinese, Japanese, Korean, Arabic, Hebrew, Thai, and Indic
scripts. Windows includes fonts for the common cases.

On `main`, after 0.7.1, long right-to-left titles in song rows and the player
bar end with an ellipsis inside their text area. Joined Arabic letters are
measured together so the last visible letter stays whole at the edge.

![Japanese, Chinese, and Korean titles in a playlist](/assets/images/scripts.png)

## Sign in

Start the app and press **Sign in with Spotify**. Your browser opens Spotify's
consent page, so Spotifast never sees your password. When the browser returns
to the app, your library loads.

Spotifast stores a refresh token in your platform's state directory
(`~/.local/state/fastpotify` on Linux). You normally need the browser only
once per machine.

## Enable playback on this computer

Playing music *on this machine* needs a second browser approval because
Spotify authorizes streaming separately ([why](/how-it-connects/)). Open the
device menu in the player bar and select **Set up playback here**, or use
Settings. This needs Spotify Premium. Spotifast saves the playback credential.

The computer then appears as a Spotify Connect device named **Spotifast**.
You can rename it in Settings.

## Basics

- **Closing the window does not stop the music.** Spotifast keeps playing
  from the system tray; reopen it from the tray icon and quit from the tray
  menu or Ctrl+Q. On macOS you can also reopen it from the Dock. Settings can
  turn this off. On Linux, including Flatpak, a desktop with a working system
  tray is required for this behavior.
- **Play and Pause fade.** With the default audio backend, music played on
  this computer fades in or out to avoid a hard cut. Gapless transitions
  between songs and playback on another device are unchanged.
- **Play buttons show progress.** The button spins until Spotify responds.
- **Artist names are links.** Click a credited artist in the player bar to
  open their page. On `main`, for the release after 0.7.1, local playback
  preserves these links before Web API metadata arrives.
- **Common actions have shortcuts.** Space plays and pauses, Ctrl+F or `/`
  searches, and `Q` opens the queue. Ctrl+/ shows the full list.
- **Rows and cards have context menus.** Right-click a song, playlist, album,
  or artist to see actions such as queue, save, add to playlist, and copy link.
  Search's **Top result** card has the menu for the song, artist, album,
  playlist, or podcast it shows. Search shelves and filtered grids, and Home's
  playlist shortcuts, Made for you, Recently played, and top artist cards offer
  the same menus. Your own playlists include **Edit details** and **Delete**.
  On `main`, for the release after 0.7.1, Library's album, artist, and podcast
  grids and artist pages' discography and related-artist cards offer these
  menus too. Right-clicking opens the menu without starting playback.
  **Add to playlist** includes a local name filter for the playlists you can
  edit, for either one song or a selection. **New playlist** stays available
  when no names match.
  If the playlist already contains the song, Spotifast asks before adding
  another copy.
- **Spotify links open in Spotifast.** A `spotify:` link shared from another
  app opens its page, starting Spotifast if it is not running. Links to
  `open.spotify.com` go through the browser first, which hands them over the
  same way. `spotifast <link>` does the same from a terminal.
