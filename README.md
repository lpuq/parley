# Parley

Private voice, video and screen-share calls for Linux, straight between friends,
including your desktop or individual apps' audio when you share your screen, which
few calling apps on Linux can do. No accounts, no port forwarding, and no server you
have to run or pay for. A [Windows preview](#windows-preview) is out too.

![A Parley call: Bob shares a slide while he and Alice talk](docs/screenshot.png)

- **1:1 and group calls** (up to 3 people). Everyone can talk, show their webcam and
  share their screen with sound at the same time.
- **End-to-end encrypted.** Every connection is QUIC + TLS 1.3, authenticated by each
  person's own key. Nobody watching the traffic can see or hear anything.
- **Share a code once.** Your code works like a PGP key: send it to a friend once
  (Signal, email, anything), or let them scan it as a QR code with their webcam, and
  you're contacts. Groups get their own code, which is only needed for people who
  aren't already your contacts.
- **Discord-style call screen**: video tiles with green speaking rings, click-to-focus,
  double-click (or **F**) to fill the window with a stream, mute/deafen, mic and
  camera pickers, and previews of what you're sending. Right-click a person or a
  stream for its volume, mute, fill-window and Stop watching. Chimes when people join
  and leave.
- **Clean voice.** *Voice Isolation* (default) uses neural noise suppression
  (RNNoise) to remove keyboards, desk knocks, chewing and mic rubbing, transmits only
  while you speak, and evens out your loudness so you stay audible when you lean back.
  *Studio* sends your mic untouched; *Custom* lets you pick the suppression and set the
  input sensitivity (−100 to 0 dB) against a live level meter. *Hear yourself* plays
  your processed mic back a second later.
- **Text chat.** Message friends and groups in a Discord-like view: messages grouped
  under each person's picture and name, day separators, clickable links, unread
  counts and notifications (and a red dot on the tray icon). Calls show up in the
  conversation, Discord-style: "Bob started a call that lasted 5 minutes", or a
  missed call. Chat during a call in a side panel. Messages travel
  directly between you, encrypted; with no server to hold them, a message to someone
  offline waits on your computer (greyed out) and is delivered as soon as you're both
  online. History stays on each person's computer.
- **Tell people apart.** Everyone has a picture (identicon) and a name tag like
  `alice#f3a7`, both derived from their identity key, so two people (or two identities
  on one computer) with the same name never look alike. A contact's ⋮ → *User info*
  shows their full key, when you added them, and their Parley version. Settings →
  *Appearance* can hide the tags (strangers calling always show theirs), or show
  GNOME-style initials instead of the patterns.
- **Pictures, your way.** Give yourself or anyone a picture: an image file, or letters
  or a symbol on a colour or gradient, GNOME style (a contact's ⋮ → *Change picture*,
  or the menu → *Change my picture*). Pictures stay on your computer; nobody else sees
  them.
- **Online status.** See which friends are online or on Do Not Disturb (calls arrive
  without a sound and nothing pops up), and go Invisible to look offline. A group
  call rings but doesn't pull Parley to the front; joining is up to you. Group calls list who's in them under
  the group, with a red LIVE badge for anyone sharing their screen and their
  mute/deafen state, so you can see who's there before joining.
- **Screen share with app audio.** Share all desktop audio or only chosen apps.
  Parley's own audio (your friends' voices) is never captured, so there's no echo.
  Pick resolution (480p–1440p) and frame rate (24, 30, 60 or custom) separately; when the
  connection can't keep up, the bitrate drops on the fly so motion stays smooth. Change
  the quality, sound or what you're sharing mid-stream, and friends watching stay on it.
- **Resilient groups.** Someone dropping out doesn't end the call for everyone else.
  Parley reconnects automatically after network blips, and anyone can rejoin with one
  click.
- **Group owners.** Whoever creates a group owns it (a crown in its member list) and
  can right-click a member → *Remove from group*, for old accounts or people who
  shouldn't be there. They're out for everyone, and the group code no longer gets them
  back in.
- **Looks the same everywhere.** Light, dark, or follow the system (Settings →
  *Appearance*), with GNOME's Adwaita icons built in, so KDE and other desktops show
  Parley as designed.

## Install

```bash
git clone https://github.com/lpuq/parley.git
cd parley
./install.sh
```

That's the whole setup. The installer needs only Flatpak, which Fedora Workstation
ships with; on other distros it offers to install it with your package manager (the only
step that can ask for your password). It then fetches everything else into your user
account: Flathub, the GNOME 51 SDK, Rust and the H.264 codecs (about 2 GB, once). It
compiles Parley in the Flatpak sandbox (about 5–10 minutes the first time) and installs
it into your app menu. Running it again updates Parley in a minute or two.

- `./install.sh --bundle` also writes `parley-<arch>.flatpak`. Put that file next to
  `install.sh` on another machine of the same architecture and `./install.sh` installs it
  in seconds, with no compiling.
- `flatpak/cross-bundle.sh` (on an ARM machine) cross-compiles `parley-x86_64.flatpak`
  for Intel/AMD machines, using the host's clang/lld and the x86_64 GNOME SDK.
- `./install.sh --native` builds a regular binary instead, offering to install the
  needed `-devel` packages.
- `./install.sh --uninstall` removes Parley; its notes explain how to free the build tools.

Runtime needs: PipeWire (with pipewire-pulse) and GStreamer 1.22+ with the *good*,
*bad*, *ugly* and *libav* plugin sets plus the GTK 4 video sink (`gstreamer1-plugin-gtk4`
on Fedora, `gstreamer1.0-gtk4` on Debian/Ubuntu). Screen sharing uses
xdg-desktop-portal, which works on GNOME, KDE, niri, Hyprland, Sway and others.

## Windows (preview)

Parley runs on Windows 11 (x64; Windows 10 should work but hasn't been tried). It's a
preview: so far it has been tested in a Windows 11 virtual machine, against Linux
friends, not on real PCs. Calls work between Windows and Linux like any others.

1. Download `Parley-<version>-windows-x64.zip` from the
   [Releases](https://github.com/lpuq/parley/releases) page and unzip it anywhere.
2. Start `Parley\bin\parley.exe`. The build isn't code-signed, so Windows SmartScreen
   may say it doesn't recognise it: **More info → Run anyway**.

Calls, chat, groups, screen sharing, the tray icon (with Mute and Hang up during a
call), notifications and starting at login all work as on Linux. What's different
or missing so far:

- Screen sharing shares a whole screen (you pick which one if there are several),
  not single windows.
- The sound you share is everything playing on the computer except Parley itself, so
  friends never hear themselves. Choosing single apps is Linux-only for now.
- Video is encoded in software (x264); hardware encoding isn't used yet.
- Webcams haven't been tested.
- No installer. When a new version is out, an **Update** button appears in the top bar
  (see [Updating](#updating)). Your identity, friends and chats stay in
  `%LOCALAPPDATA%\parley` and `%APPDATA%\parley`. If you move the folder, switch
  Settings → *Start when I log in* off and on again.
- Windows 11 tucks new tray icons away: click **^** next to the clock, and drag
  Parley's icon onto the taskbar to keep it in sight.

To build it yourself, install [MSYS2](https://www.msys2.org), then in its *CLANG64*
shell:

```bash
pacman -S --needed git zip mingw-w64-clang-x86_64-{toolchain,pkgconf,rust,gtk4,libadwaita,ntldd} \
    mingw-w64-clang-x86_64-{gstreamer,gst-plugins-base,gst-plugins-good,gst-plugins-bad,gst-plugins-ugly,gst-libav,gst-plugins-rs}
git clone https://github.com/lpuq/parley.git && cd parley
windows/package.sh    # builds dist/Parley and dist/Parley-<version>-windows-x64.zip
```

## Building it yourself (no prebuilt binaries)

You don't have to trust the release bundles. `./install.sh` compiles Parley from this
source tree inside the Flatpak SDK; it never downloads a Parley binary. Everything it
fetches comes from Flathub (signed runtimes, SDK, Rust toolchain and codecs) and
crates.io (Rust dependencies, pinned with checksums in `Cargo.lock`).

With the standard Flatpak tooling instead of the installer:

```bash
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak install --user flathub org.freedesktop.Platform.codecs-extra//26.08-extra   # H.264
flatpak-builder --user --install --install-deps-from=flathub --force-clean \
    build-dir flatpak/io.github.parley.Parley.yml
```

The manifest builds offline: `flatpak/cargo-sources.json` lists every crate with its
checksum (regenerate it with `flatpak/gen-cargo-sources.py` after changing
`Cargo.lock`).

Without Flatpak, `./install.sh --native` installs the build dependencies with your
package manager (dnf, apt or pacman) and builds a regular binary. By hand on Fedora:

```bash
sudo dnf install cargo clang-devel gtk4-devel libadwaita-devel gstreamer1-devel \
    gstreamer1-plugins-base-devel pipewire-devel gstreamer1-plugins-good \
    gstreamer1-plugins-bad-free gstreamer1-plugins-ugly gstreamer1-plugin-libav gstreamer1-plugin-gtk4
cargo build --release --locked
./target/release/parley
```

### Updating

Release bundles and the Windows zip update themselves, when you say so: a small
**Update** button appears in the top bar when a new version is out, and **Restart to
update** once it's installed. Nothing installs without that click, and Settings →
*Check for updates* turns it off. Updates are signed, and Parley refuses any that aren't:
on Linux they come through Flatpak's own update portal, so Parley needs no extra sandbox
permission. Moving to this from a version before it takes one last manual install.

If you built Parley from source:

```bash
git pull
./install.sh
```

Only what changed is recompiled (about a minute), and your identity, contacts and
settings are kept. The installer offers to restart a running Parley. To hear about new
versions, use **Watch → Custom → Releases** on the GitHub page.

### Verifying a release bundle

If you do use a `.flatpak` from the Releases page, check it against `SHA256SUMS`
from the same release:

```bash
sha256sum -c SHA256SUMS --ignore-missing
flatpak install --user parley-x86_64.flatpak
```

## Using it

1. **First launch:** pick the name friends will see.
2. **Add a friend:** press **Copy my code** and send it to them, or **Show my QR code**
   and let them scan it (**+** → *Scan QR code with camera*). Once they've added you,
   they can call you. The first time they do, you can save
   them back with one click, so only one of you has to share a code.
3. **Call:** press the green phone button next to a friend.
4. **Group call:** menu → **New group**, then tick your friends. Press **Join** to ring
   everyone. Friends who are your contacts don't need any code. For anyone else,
   use **Copy invite code** or **Show invite QR code** on the group. Later, anyone in the group presses **Join**
   to start or rejoin the call.
5. **In a call:** **M** mutes, **D** deafens, **F** fills the window with a stream and
   **F11** toggles full screen. Use the arrows next to the mic and
   camera buttons to switch devices. The screen button opens the share options
   (quality, sound), then your desktop's own window/screen picker.

Closing the window keeps Parley running so friends can still reach you (you can turn
this off in Settings). Settings → General → *Start when I log in* starts it
automatically.

## How it works

```
 ┌────────── GTK4 / libadwaita UI ──────────┐
 │ sidebar · call grid · dialogs · settings │
 └──────┬───────────────────────────▲───────┘
        │ commands                  │ events
 ┌──────▼───────────────────────────┴───────┐      ┌───────────────────────┐
 │ net.rs  call engine (tokio actor)        │      │ media/ (GStreamer)    │
 │  ringing, group auth, full-mesh,         │◄────►│ mic → webrtcdsp →     │
 │  duplicate resolution, auto-reconnect    │ hub  │   Opus → RTP          │
 │ iroh QUIC: dial by public key, hole      │      │ cam/screen → H.264    │
 │ punching, encrypted relay fallback       │      │ mixer + echo probe    │
 └──────────────────────────────────────────┘      │ appaudio (PipeWire)   │
                                                   └───────────────────────┘
```

- **Connectivity:** [iroh](https://iroh.computer) dials peers by public key. It
  hole-punches a direct UDP path when it can. When it can't (strict NATs), it falls back
  to forwarding the already-encrypted packets through a public relay. Peers find each
  other through iroh's public discovery service. These relays and discovery servers are
  run for free by n0 (the iroh developers). They only ever see encrypted bytes and
  public keys. No setup is needed, and nobody in the call hosts anything. Some
  rendezvous point is unavoidable when nobody port-forwards; this design keeps it
  content-blind.
- **Identity & contacts:** each install has an ed25519 key
  (`~/.local/share/parley/identity.key`, mode 0600). A contact code is your public key
  plus your name. Calls are authenticated against that key, so nobody can impersonate a
  contact.
- **Identity moves** (Settings → *Replace my identity*): a new key is generated and the
  old key signs a notice "old → new" (ed25519, domain-separated), then the old key is
  deleted. The new key delivers the notice over `parley-move/1` to every contact and
  group member, retrying every 30 s until each is reached. A receiver follows it only if
  the signed chain starts at an identity it knows and ends at the key it's connected to,
  then rewrites that person in its contacts, groups and chat history. Anyone who
  doesn't know an earlier identity just hears "not known". The whole chain is sent, so
  someone who missed a move can follow several at once.
- **Groups:** a group code carries a random 256-bit secret. Joining proves knowledge of
  it with an HMAC bound to both participants' keys. A member who rings a contact for
  the group includes the invite inside the encrypted connection, so contacts can join
  without ever seeing the code.
- **Group owners and removals:** a group records its creator as owner, carried in its
  code and invites (groups from before 0.8.0 take their first member, which is the
  creator in every copy). A removal is signed by the owner's key ("remove X from room
  R"); members accept only removals signed by the owner they recorded, then refuse X's
  group calls, group chat and status checks even though X still knows the secret. Each
  member passes removals on to the others over `parley-group/1` (retrying every minute
  until confirmed), and they travel with codes and invites to people who join later.
- **Media transport** (one QUIC connection per pair of people):
  - Audio: Opus (20 ms frames, in-band FEC) in RTP over unreliable QUIC datagrams.
    Playback is scheduled from the sender's RTP timestamps (anchored to the fastest
    packets), not from arrival times, and the jitter buffer adapts between 80 and 400 ms
    to cover 97% of packets. On choppy Wi-Fi it grows within seconds; on a calm link it
    shrinks back slowly.
  - Video: H.264 (hardware VA-API/NVENC when available, otherwise x264 `zerolatency`).
    The bitrate adapts twice a second to how long frames wait to be sent: it falls at
    once when they back up (softer picture, same frame rate) and climbs back slowly,
    remembering where trouble started so it doesn't keep overshooting. Each group of pictures (GOP) goes on its own QUIC stream. If a
    link gets congested, the sender drops the stale GOP and restarts at a fresh
    keyframe instead of building up delay.
- **Screen audio:** while sharing, Parley creates a temporary private PipeWire sink.
  It links the chosen apps' output ports into it (they keep playing to your speakers
  too) and records its monitor. Streams belonging to Parley's own process and
  system DSP nodes are never linked. This is the same idea as Vesktop's venmic.
- **Echo cancellation:** WebRTC's echo canceller (`webrtcdsp`) listens to exactly what
  Parley plays, so speakers work without feedback.
- **Voice processing** (`media/voice.rs`), after echo cancellation, on 10 ms frames:
  RNNoise suppression (the pure-Rust `nnnoiseless` port, model built in), a voice gate
  driven by RNNoise's voice detection or a dB threshold (with hold time and click-free
  ramps), and an automatic gain that learns your speech level from voiced frames only
  and steers it to −20 dBFS (up to +24 dB), followed by a limiter.
- **Chat:** each delivery is a small connection (`parley-chat/1`) carrying up to 50
  messages, acknowledged by the recipient; undelivered messages wait in an outbox and
  are retried when the recipient shows up online, and every minute. Direct messages
  are accepted from contacts (and, if you allow calls from people not in your
  contacts, from anyone with your code, who is then added as a contact); group
  messages only from that group's members. History is one JSON-lines file per
  conversation in Parley's data folder.
- **Online status:** every 30 s Parley opens a tiny separate connection
  (`parley-presence/1`) to each contact and group member to exchange status, and
  re-announces within two seconds when your status, group call, or mute/deafen/sharing
  changes. Which group call you're in is only told to that group's members. It only
  answers contacts and group members, and doesn't answer at all while you're Invisible.

## Privacy and security

- **What's encrypted:** everything between callers (audio, video, names, group
  invites) travels inside QUIC/TLS 1.3 connections authenticated by each person's
  ed25519 key. Relays forward only ciphertext.
- **What others can learn:** your contact code contains your public key and display
  name. Anyone holding it can look up how to reach you through iroh's discovery service
  and, by calling, learn your IP address, as with any peer-to-peer app. n0's relay and
  discovery servers see your IP address and public key, but not who you talk to inside
  a connection or what is said.
- **Update checks:** to find new versions, Parley (or Flatpak, on Linux) asks GitHub, which
  sees your IP address like any website. No identity or contact data is sent. Settings →
  *Check for updates* turns this off.
- **Online status:** only your contacts can see it. Invisible makes Parley stop
  answering status checks, so you look offline; someone with your code could still
  notice your computer is reachable at the network level.
- **Who can ring you:** by default anyone who has your code, so a friend can add you
  and call without you adding them first. Strangers are shown with their ID next to the
  name they chose, so they can't pass as one of your contacts, and only a few calls can
  ring at once. Settings → *Allow calls from people not in my contacts* turns this off.
- **If your code got out:** Settings → *Replace my identity* gives you a new code and
  deletes the old key, so the old code stops reaching you at once (no calls, messages,
  status, or IP address through it). Your friends and the members of your groups are
  moved over automatically, each the next time you're both online. Nobody else learns the
  new code, so remove anyone you don't trust first. Friends need Parley 0.7.0 or newer to
  follow a move.
- **Nothing happens until you answer:** no audio or video flows on a call you haven't
  accepted. Group joins require proof of the group's secret.
- **Local data:** your private key (`identity.key`, mode 0600) and contacts live in your
  home directory (inside `~/.var/app/io.github.parley.Parley/` for the Flatpak). The log
  file (Settings → Troubleshooting) contains names and public keys but no call content.

Parley hasn't had an independent security audit. Please report problems through the
issue tracker, or privately via GitHub's security advisories.

## Development

```bash
cargo test                                  # unit tests
cargo test --lib -- --include-ignored       # + real network tests over iroh (needs internet)
cargo run --example appaudio_check          # checks screen-audio routing on your PipeWire
cargo run --example devlist                 # lists mics/speakers/cameras as Parley sees them
cargo run --example qr_image_check -- f.jpg # decodes a photo/screenshot of a QR code
PARLEY_PROFILE=alice cargo run              # separate identity/config, to run two copies
PARLEY_LOG=parley=debug cargo run           # verbose logs
```

The network tests cover a direct call, a 3-person group mesh where two members only
know the creator, leaving, rejoining without ringing, automatic reconnection after a
dropout, in-order delivery of video GOPs plus audio datagrams, chat delivery and
refusal, identity moves (followed by friends, ignored by strangers), and a removed
group member being refused by the group (after joining fine before the removal).

### Publishing a release

Signing keys live in `~/.config/parley-release`, never in this repository. Back them up:
without them, people need one manual update to move to new keys.

```bash
./install.sh --bundle                 # aarch64
flatpak/cross-bundle.sh               # x86_64
flatpak/publish-repo.sh --push        # signs both into the hosted repo, rebuilds the bundles
cargo run --release --example sign_release -- manifest VERSION Parley-VERSION-windows-x64.zip OUTDIR
```

Attach `latest.json` and `latest.json.sig` to the GitHub release with the zip and bundles.

## Known limitations / next steps

- Up to 3 people (full mesh). Larger calls would need an SFU-style forwarding peer.
- No Signal integration. Codes are plain text built for pasting into Signal or any
  messenger, or can be scanned as QR codes.
- Windows is a preview (see [above](#windows-preview) for what's missing there); there's
  no macOS version.
- The Flatpak isn't on Flathub. Publishing it would need an app ID on a domain or
  GitHub account you control (`io.github.parley.Parley` is a placeholder).

## License

Copyright © 2026 lpuq. Parley is free software under the [GNU General Public License v3.0 or later](LICENSE):
you can use, study, change and share it, and anyone who distributes a modified version
must publish its source under the same terms.

The built-in icons in `data/icons` are GNOME's Adwaita icons (adwaita-icon-theme by the
GNOME Project, LGPL-3.0-only or CC-BY-SA-3.0), renamed, with the call icon turned a
quarter turn; the crown was drawn for Parley.
