---
redirect_from: /using-fastpotify/
title: Everyday Use
description: Library ordering and local play history.
nav_order: 3
---

## Middle-click autoscroll

On Windows, after 0.7.1, middle-click a scrolling list or its empty background,
then move the pointer
away from the starting point. That list follows the pointer, faster as the
distance grows. Moving across another pane keeps the original list in control.
A small dead zone prevents an ordinary middle-click from moving the view.
Click again, press Esc, turn the wheel, or switch to another window to stop.
Buttons and text fields keep their normal middle-click behavior.

This works automatically on Windows, with no setting to enable. Linux and
macOS retain their existing middle-click behavior.

## Scrolling shelves

Point at a horizontal shelf, such as Made for you or Recently played on
Home, and hold `Shift` while turning the mouse wheel. The shelf moves while
the surrounding page stays put. Release `Shift` to scroll the page normally.

## Dragging beyond the visible list

On `main`, for the release after 0.7.1, hold a dragged song near the top or
bottom of an editable playlist's visible area to scroll. Scrolling gets faster
closer to the edge and stops when you move away or release the mouse. This
lets you move a song from the end to the beginning without dropping it along
the way. Clear filters and sorting before reordering playlist songs.

On `main`, after 0.7.1, drag a song from the player bar, the queue, or another
list into an open editable playlist. The line between rows marks its insertion
position. Dropping below the last row appends; the blank area of an empty
playlist accepts its first song. The source song stays in its list or queue,
and playback continues unchanged. Dragging a row within the same playlist
still moves that row.

On `main`, after 0.7.1, select several songs with `Ctrl`-click (`Cmd`-click
on macOS) or `Shift`-click, then drag any selected row. The whole selection
travels together in its displayed order, even if you selected the rows in
a different order. The preview names the first song and counts the rest.
Drop it on a sidebar playlist to append, between rows of another editable
playlist to insert, or on Liked Songs to save every selected song.
Selected rows have a translucent neutral highlight. Keyboard focus uses
the row highlight without an extra outline.

Dragging an unselected row copies just that song. Reordering within a
playlist still moves one song at a time.

Clear any playlist filter or sort before placing songs between rows, so the
visible positions match Spotify's order. A duplicate confirmation keeps the
chosen position when you select **Add anyway**. Dragging near the top or bottom
of the playlist scrolls to positions beyond the visible rows.

The Library sidebar also scrolls near its edges when you drag a song toward a
playlist or reorder its entries. Only the list under the pointer scrolls.

## Starting a playlist and resuming

On `main`, after 0.7.1, with Shuffle off and the playlist in its original
order, its **Play** button explicitly starts at the first available song.
The selected song appears in the player immediately, including while local
playback reconnects. The full playlist remains the playback source, even
when only its first page is loaded.

Double-click a song to start at that row. Sorting the table plays its chosen
order when Shuffle is off. Shuffle chooses a random starting song unless you
choose a specific row. To resume the current song at its paused position,
use **Play** in the bottom player bar or press `Space`.

On `main`, after 0.7.1, starting playback from a sorted playlist or Liked Songs
view immediately shows the requested song from the loaded rows. The preview
stays while local playback connects, even when that song was not previously
in the track cache. The selected order and the playlist stored on Spotify stay
the same.

Sorted and filtered views omit unavailable songs and local files from playback.
The rows stay visible, and selecting a repeated song starts that occurrence. Filtering a playlist or Liked Songs plays only the matching songs,
including repeated entries. Play is disabled when the view has no playable
songs; it never falls back to the unfiltered playlist in that case. Clearing
the filter restores the original view. Existing Shuffle behavior is unchanged.

## Refreshing a playlist

On `main`, after 0.7.1, choose **Refresh** in a playlist's **…** menu to reload
its details and songs, including changes made in another Spotify client. The
menu item reads **Refreshing…** and is disabled while loading. Current songs,
filtering and sorting stay visible, and pending edits finish before replacement
rows are requested. A failed refresh keeps the songs and offers **Retry**.

## Playing from the sidebar

Double-click a playlist, Liked Songs, album, artist, or podcast row in the
Library sidebar to start playing it. A single click still opens the row's page.
Pointing at a row's cover art also shows a play button, but only when the
sidebar is not in compact mode.

## Finding a setting

The Settings page has its own search field under the title. Type to narrow
the page to matching rows; sections without matches disappear. Clear the
field to see everything again.

## Keyboard and screen readers

Right-click a search, filter, settings or playlist-editing text field for
**Cut**, **Copy**, **Paste** and **Select all**. Cut and Copy require a text
selection. The usual keyboard shortcuts, including Undo, still work.
On `main`, after 0.7.1, Ctrl, Cmd and Alt arrow keys move the caret while
a text field has focus. Playback and navigation shortcuts on those keys
remain available from song rows and other controls.

The main window provides screen-reader names for playback controls, library
and song rows, menus, sliders, and settings switches. `Tab` and `Shift+Tab`
move keyboard focus, shown by an outline. `Enter` or `Space` activates the
focused control; on a song row, it plays that song. The row's **More** button
opens its menu from the keyboard too.

In a playlist, album or Liked Songs, focus a song row and use the up and down
arrows to move between whole rows in the displayed order. Rows scroll into
view as you move; Enter plays the focused song. Tab still reaches artist
links and each row's Like and More controls.

Left and right arrows adjust a focused volume slider by five percentage
points, or the seek slider by one percent of the song. Screen readers can
also read and set these sliders' values. `Ctrl+F` (`Cmd+F` on macOS) focuses
search. The playback shortcuts remain available; unmodified letter and
Space shortcuts yield to the focused control.

This is the first part of screen-reader support. Windows testing with NVDA
remains tracked in [#262](https://github.com/crmne/spotifast/issues/262).
Winamp skins do not yet have equivalent accessibility coverage.

## Library order

On `main`, after 0.7.1, the menu below the Library filters selects an order
for each section. **Name** and **Recently played** are available throughout.
Albums and podcasts also offer **Recently added**, using their actual save
dates. Spotify does not supply equivalent dates for followed playlists or
artists, so those sections do not offer that choice. Entries with missing save
dates come last.

**Spotify custom order** follows the playlist sequence and folders supplied by
the existing local playback session. Until that order arrives, available
playlists stay visible. The last good tree is kept for the same signed-in
account. Spotifast's local pins remain at the top, including pins from a closed
folder. Changing an order or dragging a row here does not change Spotify's
order or folders.

Drag playlists to choose **Local custom order**. New playlists appear below the
pinned group. Selecting **Name**, **Recently played** or **Spotify custom order**
keeps the saved arrangement, so selecting **Local custom order** restores it.
The playlist context menu's **Sort by recently played** also preserves it.

Upgrading keeps the previous default: a saved local playlist arrangement wins;
otherwise available Spotify folders keep their order, and a flat playlist list
uses recent plays. Other sections keep their supplied Library order until you
select a sort. Explicit sorts load the remaining pages of the selected section
in the background. A failed page stops that loading; choosing the order again
retries it.

Liked Songs starts pinned at the top. Drag it between pins to choose its
position, or below the pin block to unpin it and put it in **Local custom
order**. Other pins can sit above it. Its right-click menu also offers **Unpin**
and **Pin to top**; pinning adds it after your existing pins. The arrangement
survives restarting Spotifast and switching sort choices.

When unpinned, Liked Songs follows **Name** or **Recently played** like the other
rows. In **Spotify custom order**, it appears after the playlists because it
has no place in Spotify's playlist tree. Returning to **Local custom order**
restores its saved position. Dragging a song onto Liked Songs still saves that
song, wherever the row sits.

In **Settings > Appearance**, **Compact track list** puts each song on one
line. In narrow lists, the added date follows the artist credits with a spaced
bullet; each artist name remains a separate link.

## Windows taskbar controls

On `main`, after 0.7.1, hovering Spotifast's taskbar button offers **Previous**,
**Play/Pause**, and **Next** beneath its window preview. They control the same
playing device as the player bar, update immediately, and are disabled when
there is no song or the device refuses controls. The icons follow the system
appearance and display scaling.

On `main`, after 0.7.1, clicking or double-clicking the Windows tray icon shows
and raises Spotifast. Use **Show or hide Spotifast** in the tray menu to hide
it again.

Closing to the tray removes the window and its preview. Reopening the main
window or switching to the Winamp window creates its controls again. Media
keys and the system's now-playing controls continue working while the window
is closed. These buttons add no Spotify requests beyond their playback actions.

For the Winamp mini player, turn off **Show in taskbar** under
**Settings > Winamp skins**, or **Show in taskbar** in its options menu.
The choice survives restarts. The mini player stays visible; the tray icon,
**Ctrl+M**, the skin logo, and launching Spotifast again remain ways to reach
the app. Returning to the main window always restores its taskbar button.
Changing the option while the mini player is open replaces that window while
playback continues. This setting is available on Windows; it does not change
Linux panels or the macOS Dock.

On Windows, after 0.7.1, the mini player starts on the current desktop if its
saved title bar is outside every connected monitor’s work area. Positions on
connected secondary monitors still restore. Reinstalling preserves settings;
it is not needed to recover a position left on an unplugged display.

## Keeping the mini player above other windows

**Always on top** works on Windows, macOS and X11. On Wayland the app's
controls are unavailable, because the window backend cannot apply them.
Use your desktop's window rule or shortcut instead. In KDE Plasma, configure
**Keep Window Above Others** under **Settings > Keyboard > Shortcuts >
Window Management**. Your saved preference remains available when you use
Spotifast on a supported backend again.

## Background material (Windows 11)

**Settings > Appearance > Background material** picks what Windows draws behind
the window. **Follow Windows** (the default) uses Acrylic, so the windows behind
this one show through it. Windows documents Acrylic for menus and flyouts rather
than a whole window, and it costs more to compose, so expect it to be turned off
under Battery Saver. **Opaque** paints the app's own background and asks Windows
for nothing.

Windows 11's other materials, Mica and Mica Alt, are not offered: both are opaque
and carry the wallpaper colour once, so they never show what is behind the
window.

**Transparency** sets how much of the app's own colour covers the material --
panels, sidebar, player bar and the page itself. Lower leaves more of a thin
wash over the glass; higher is flatter and hides more of what is behind the
window. One number covers every layer, so text stays as readable at either end.
The slider only means anything while a material is live: with the background set
to **Opaque** there is nothing underneath to see.

Acrylic's own density is Windows', not the app's: `DWMSBT_TRANSIENTWINDOW` is a
fixed material with no tint parameter, and the slider moves the app's layers
over it rather than changing the material.

Windows keeps control of the details. The material falls back to a solid colour
when transparency effects are off, when the window is inactive, under Battery
Saver, and on Windows 10 or a build before 22H2. Nothing here changes the
Winamp mini player, which draws its own shape.

On `main`, after 0.7.1, the top bar reserves room for the device badge beside
Search. In narrow windows that badge shows only its icon. The bar stays above
the page. Library, Queue and Lyrics keep their full height. Hover to read the
device name; click to open the device picker.

## Recent

The queue panel's second tab combines Spotify's history with tracks played
through Spotifast, which Spotify does not record.

On `main`, after 0.7.1, choosing any Recent row starts that song on its own,
and the player bar shows the selection immediately while playback starts.

A song is added after about 30 seconds, or halfway through a shorter song.
Paused time and seeking do not count.

On `main`, after 0.7.1, each repeat remains a separate play, including songs
shorter than a minute. A newly loaded local repeat earns its own listening
time; pausing or seeking the current play does not create another entry.
The same play reported both locally and by Spotify appears once.

The local list is stored in `history.json` and is never uploaded. Settings →
Storage shows its location and has a **Clear history** button.

On Windows, the main window's minimize, maximize, and close buttons share the
top bar with Spotifast's controls. Drag an empty part of that bar to move or
snap the window, and drag a window edge or corner to resize it.
