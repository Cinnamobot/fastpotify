---
title: Download
description: Download the app for Windows.
nav_order: 1
---

This is a Windows fork of **Spotifast** (previously **Fastpotify**) that adds
**automix**: transitions between tracks decided from the music rather than a
timer. Everything else is upstream's work; see the
[upstream README](https://github.com/crmne/spotifast#readme) for the client
itself and the fork README for what automix adds.

{% assign v = site.fastpotify_version %}
{% assign base = "https://github.com/Cinnamobot/fastpotify/releases/download/v" | append: v %}

The current version is **v{{ v }}**. SHA-256 checksums are in
[checksums.txt]({{ base }}/checksums.txt). Older versions are on the
[releases page](https://github.com/Cinnamobot/fastpotify/releases).

## Windows

The installer adds the app to the Start menu and needs no administrator
rights. It also registers it for `spotify:` links; if the official client is
installed too, Settings → Apps → Default apps decides which of the two opens
them. Choose x86_64 for most PCs or aarch64 for Windows on ARM:

- [fastpotify-v{{ v }}-x86_64-pc-windows-msvc-setup.exe]({{ base }}/fastpotify-v{{ v }}-x86_64-pc-windows-msvc-setup.exe)
- [fastpotify-v{{ v }}-aarch64-pc-windows-msvc-setup.exe]({{ base }}/fastpotify-v{{ v }}-aarch64-pc-windows-msvc-setup.exe)

For a portable copy, download a zip, unpack it, and run `fastpotify.exe`:

- [fastpotify-v{{ v }}-x86_64-pc-windows-msvc.zip]({{ base }}/fastpotify-v{{ v }}-x86_64-pc-windows-msvc.zip)
- [fastpotify-v{{ v }}-aarch64-pc-windows-msvc.zip]({{ base }}/fastpotify-v{{ v }}-aarch64-pc-windows-msvc.zip)

Either way, SmartScreen may warn about an unknown publisher on first run;
choose More info, then Run anyway.

Or build from source: see [Getting Started](/getting-started/).
