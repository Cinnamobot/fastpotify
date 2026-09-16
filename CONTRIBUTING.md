# Contributing to Spotifast

Spotifast is a native Spotify client. Changes should improve the
desktop app without adding a browser, fallback services, or another backend.

## Before opening an issue

Search open and closed issues first. For a bug, use the bug form and include
the requested log and exact steps to reproduce it. Reports without enough
information to investigate may be closed.

For a feature, explain the user problem. Discuss large changes in an issue
before writing code. Existing code does not guarantee that a feature fits the
project.

Some boundaries come from Spotify or from upstream libraries:

- Local playback requires Spotify Premium because librespot requires it.
- Spotify Lossless is not available through librespot. Spotifast will
  reconsider it if librespot gains lawful upstream support; proposals that
  depend on bypassing Spotify's DRM are out of scope.
- Spotify tracks must come from Spotify. Substituting audio from YouTube,
  Piped, `yt-dlp`, or another catalogue is out of scope.
- Spotifast will not embed a browser engine, add telemetry, or introduce a
  Spotifast-operated service.

[What Spotify Lets a Client Do](docs/_reference/what-spotify-allows.md)
lists what each of the three surfaces offers and the requests none of them
can serve, with the reason for each; a request in its last section is
answered from there and closed.

Duplicate, out-of-scope, or incomplete issues may be closed with a short
explanation.

A bug can be closed once its fix is on `main` and the relevant checks pass,
with the commit and release status stated. Reporter confirmation is welcome
but is not required for closure. Reopen the issue if it persists after updating.

## Design principles

1. **Native and fast.** Startup time, idle work, memory use, and binary size
   are product features. Keep the UI thread free of network and disk waits.
2. **Focused.** Prefer a complete, coherent workflow over a collection of
   settings, modes, and speculative features.
3. **Honest integrations.** Use Spotify's Web API and librespot for what they
   support. Do not scrape, impersonate capabilities, bypass technical
   protections, or silently replace one service with another.
4. **Windows is the supported target.** Windows is the product this fork
   ships and tests. Platform-specific code for other targets stays isolated
   so the tree keeps compiling, but it is not a promised product here.
5. **Small dependency surface.** Reuse the standard library and existing
   crates where practical. A new dependency needs a concrete benefit worth
   its build time, binary size, maintenance, and security cost.
6. **Visible failure, private data.** Errors should be actionable, rate limits
   should be respected, and credentials must never appear in logs. Network
   behaviour belongs in the documentation.

## Pull requests

Keep each pull request to one change. Explain why it belongs in Spotifast,
what changed, and how you tested it. Avoid unrelated formatting, refactors,
generated prose, and large mechanical rewrites.

`main` has a linear history. Outside pull requests are squash-merged into one
focused commit with contributor credit; merge commits are not accepted.
Maintainer work is committed directly on `main`, one topic per commit. Use
fast-forward-only pulls and rebase unpublished local commits when needed.
Do not rewrite published history without explicit maintainer approval.

The same rules apply to hand-written and AI-assisted changes. The author must
understand every line and answer review comments with specific reasoning.

Code changes should include tests for behaviour that can regress. UI changes
should include before/after screenshots or a short recording and should use
demo mode where possible. User-visible behaviour, settings, files, or network
access must be documented in the same pull request.

### Visual reviews

Provide an HTML comparison with actual before-and-after captures, using demo
mode where possible. Match the data, page, interaction state, zoom and window
size so the difference shows the proposed change. Identify the baseline and
candidate, and report which platforms actually produced the captures.

Include light/dark and narrow/normal window selectors, plus Before and After
buttons or a comparison slider. Show relevant open menus, loading, error and
empty states. Explain the visible change briefly and list any remaining checks.

When reviewing several changes, provide one index with the PR number and name,
a selector and Previous/Next controls, and a direct link to each comparison.
Load only the selected review. The maintainer can approve by PR number and
give exceptions or requested adjustments in a normal message.

Visual approval covers the described appearance and interaction; integration
still requires the relevant checks. Retain approval across rebases that preserve
that scope. Implement explicitly requested adjustments and update the evidence;
ask again only if the resulting scope goes beyond what was approved or requested.

### Checks

Run the same checks CI runs before submitting:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo test --locked --all-targets --all-features
cargo test --locked --all-features --doc
RUSTDOCFLAGS='-D warnings' cargo doc --locked --all-features --no-deps
```

This fork targets Windows. MilkDrop builds libprojectM from source, so a
Windows checkout needs CMake, a C++ compiler, libclang, and vcpkg with
`glew:x64-windows-static` installed and `VCPKG_INSTALLATION_ROOT` pointing at
it; `--no-default-features` leaves MilkDrop out and needs none of that.
CI runs the suite on Windows. Passing CI is required, but does not replace
review for correctness, product fit, maintainability, or security.

Credential-storage changes also need a native store round trip. With the
desktop credential store unlocked, run
`cargo test --locked --lib credentials::tests::native_store_round_trip -- --ignored --exact`.
It uses temporary dummy grants and deletes them afterward. CI runs this check
on Windows. The ordinary test suite uses an isolated fake store and never
reads a real Spotify grant. Demo mode also skips credential restoration.

Translation changes also need `.github/scripts/update-translations.sh --check`,
using GNU gettext tools with Rust support. Run the script without `--check` when
translatable source strings change, and review any fuzzy or missing entries in
the updated PO files. Normal Cargo builds compile the catalogs without gettext
tools. See [Translating Spotifast](docs/_reference/translating.md) for the pilot
scope and contributor workflow.

Documentation deployments take their canonical URL from the domain configured
in GitHub Pages. When changing domains, configure DNS and GitHub Pages before
redeploying; the previous hostname keeps working until that switch. Renamed
guides use `jekyll-redirect-from` to preserve their old URLs.

Releases must wait for all required CI jobs on the version commit before the
tag is pushed.

By contributing, you agree that your contribution is licensed under the
project's MIT License.
