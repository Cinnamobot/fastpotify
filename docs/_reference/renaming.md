---
title: The Rename
description: How existing installations, commands, settings and links survive the new name.
---

Fastpotify is now **Spotifast**, at [spotifast.rocks](https://spotifast.rocks/).
The rename is on `main`. The current stable release, 0.7.1, still uses the
Fastpotify name; a new application release has not been published yet.

## Existing installations

Settings, saved sign-ins, local pins, history, caches and window positions stay
where they are. There is no migration to a second set of directories. Saved
Spotify Connect names also stay as you chose them; new settings use Spotifast.

The `spotifast` and `fastpotify` commands open and control the same application.
Linux packages provide `spotifast` as an alias, without installing a second
copy of the app. Existing `playerctl --player=fastpotify` commands keep working.

Keep using the existing AUR packages, Homebrew cask, DEB/RPM package name,
Flatpak ID and Cargo package name. Their display name changes to Spotifast;
their upgrade identity stays the same. Nix also exposes `spotifast` and, on
macOS, `spotifast-app`, alongside the old attribute names.

## Packaging

Release asset names retain the `fastpotify-` prefix, and the compatibility
command keeps its `fastpotify VERSION` response. The Spotifast command reports
`spotifast VERSION`.

New macOS installations use `Spotifast.app`. The bundle ID remains
`me.paolino.fastpotify`, and its internal executable remains `fastpotify`.
Replacing an existing installation preserves its current bundle location.
Homebrew upgrades remain owned by Homebrew, whichever bundle name is
installed.

Windows keeps its original installer ID, registry identities and installation
directory. Its app name and new shortcuts say Spotifast. The previous command
remains installed for existing shortcuts and scripts.

The Flatpak ID `rocks.fastpotify.Fastpotify`, protected credential-store service,
MPRIS names and single-instance protocol retain their original identities.
Changing these would create a separate app or disconnect existing integrations.

## Website links

The guides now live at `/using-spotifast/` and `/what-is-spotifast/`.
`jekyll-redirect-from` generates redirects from their old Fastpotify URLs.
The download page continues to link to the existing stable artifacts until a
new release is available.
