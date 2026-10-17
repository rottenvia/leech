# Leech interface direction

Leech follows the familiar three-column messaging layout and density of a modern desktop chat
client, but it is not a visual clone: it uses its own plum-cast neutrals, a rose accent,
hairline edges, softer corner radii and its own server-rail selection language. The palette, typography
and spacing live in `crates/ui/src/design.rs`; every view resolves colours through
`design::palette(ui)`.

## Theme tokens and presets

The `Palette` carries surface roles: `base` (title strip and server rail), `sidebar`
(channel and member lists), `chat`, `raised` (composer, cards, search field, popovers), `hover`,
`selected`, `border`, `text_strong`/`text`/`muted`, `link`, `accent` (rose: pastel `#f0719b`
under dark text on dark appearances, `#d6336c` under white text on light ones), presence
colours, mention colours and an optional two-stop `backdrop` gradient. `canvas` and `surface`
remain as aliases of `chat` and `sidebar` for older call sites.

A process-wide `Variant` recolours the whole application on top of egui's light/dark preference:

| Preset | Surfaces |
|---|---|
| Leech | House plum-cast neutrals: dark (`#0f090c` / `#150d11` / `#191015` / `#22161c`) or light (`#ece4e8` / `#f6f1f3` / white), following System/Light/Dark |
| Eclipse | Deep black surfaces for OLED displays |
| Slate | Lighter blue-grey surfaces (`#1b1f2a` / `#262b38` / `#2c3140`) |
| Nightfall, Ember, Verdant, Afterglow | Gradient backdrop painted under translucent dark surfaces |

Gradient presets paint a full-window mesh in the background layer each frame and use
translucent panel fills; they always use dark text. Presets are chosen from the account card's
settings menu (swatch row) and persist in the application-wide SQLite `theme_variant` row next
to the light/dark appearance; unknown keys fall back to Leech. The keys written by earlier
builds (`onyx`, `ash`, `midnight-blurple`, `crimson-moon`, `forest`, `sunset`) still resolve to
their renamed presets, so a stored preference survives the rename. `--demo --demo-theme=<key>` and
`--demo-light` open fixtures in a preset for screenshots.

Message cards use a thin tint only on the conversation surface when chat is translucent.
Search previews, profile code blocks, component modals and opaque component containers keep
their normal card/code fills. Pending upload cards and their preview wells retain their
palette surfaces in both message and forum composers, independently of chat transparency.

## Brand mark and server rail

The application mark is a leech bent into a lowercase L (`assets/brand/`), not a third-party logo. Its
silhouette is rasterized into the shared icon atlas as `leech-mark` and painted wherever the
client identifies itself: the loading screen, the sign-in card and its button, the login header
and the Direct Messages tile at the top of the server rail.

Server tiles keep one constant rounded-square silhouette instead of morphing between a circle and
a squircle. Selection and unread state are shown by `notifications::rail_indicator`: a 3px accent
underline below the tile (wide when selected, narrower on hover), a short neutral tick for unread,
and a 2px accent ring around the selected tile. Controls use a 10px widget radius, 12px menus and
cards and 14px windows.

## Buttons and edges

Controls are filled, not outlined. Buttons (`design::button`) are 36px tall with an 8px
radius: Primary is the accent, Danger is red, Outline is a neutral veil of the text colour,
and Neutral is bare until hovered. Text fields, drop-downs and other inset controls share
`design::field_fill`, a step darker than the panels, with a faint edge and an accent ring while
focused. Every drop-down comes from `design::combo`, whose chevron turns over while the list is
open. Check boxes (`design::checkbox`) are rounded 20px boxes filled with the accent and a
white check when on; a chosen radio is a filled accent disc. Switch tracks are filled without
an outline.

Settings groups (`design::group`) are a sentence-case 16px heading over a borderless tile
(`design::card`, a veil of the text colour), with faint rules between rows. Form labels are
sentence case too; uppercase eyebrows are kept for list sections such as server categories and
member groups. A settings row measures its control and stacks the control under the text when
both do not fit, so a wide control never draws over its description.

Every menu row is `design::menu_item`: an 18px glyph, the label, a quiet hover plate and red
for an action that destroys something; submenu rows take a matching `design::menu_glyph`. The
message menu follows Discord's grouping: respond (reply, forward, thread, edit), copy and keep
(copy, pin, reactions, unread, link), delete on its own, the ID last.

Icon buttons come from `crates/ui/src/icons.rs`: bare (plate on hover) and accent (filled
while enabled, the composer's send). Their
hover and pressed plates are veils of the text colour with an 8px radius. Toggles keep a faint
plate while on. A toggle whose state must read at a glance (noise suppression in the voice
panel) shows its state in the glyph: a plain glyph while on, the same glyph slashed while off. It opens a popover with the switch, the filter (RNNoise or WebRTC and its strength), the
live microphone level and a link into the voice settings; disconnect beside it is quiet at
rest and turns red under the pointer. Account labels (APP, BOT) are tinted accent chips, not solid blocks. The title
bar's status chips (OFFLINE PREVIEW, the DEV and NIGHTLY build chips) share that treatment at
one 20px height: a faint fill of their tint inside a hairline of it, with no gradient or glow.
Row actions in lists (friends, roles) are the same rounded squares as every other button, not
circles. A disabled filled button keeps a dimmed fill and takes the strong text colour, so its
label stays legible.

Motion is short and shared. `design::hover_motion` fades hover plates and ink in and out,
and `design::press_motion` with `design::pressed_rect` dips a control by a few percent while
it is held. Buttons, icon buttons, menu rows, segmented tabs, text actions, title bar
captions, call controls and list rows all use them. Both run on egui's `animation_time`
(1/12 s, like the server rail), so a zero turns all motion off. Switching conversations fades
the new one in over 0.18 s from part-way, so the switch never flashes the bare background.

Loading reads as alive without spinning, and each surface waits in its own shape and motion
(`crates/ui/src/skeleton.rs`): a server channel's message groups rise from the composer under a
soft light tinted with the accent; a direct conversation's short replies slide in turn by turn
while the light passes from reply to reply; an app's replies unfold embed cards from their
accent stripe and pop in their buttons; forum posts drop in as cards; search results and pins
slide in from the pane's edge; people fill the member list with their presence dots popping in
after them. No placeholder shows for a wait under 0.15 s, and none is cut off: when the content
comes, the placeholder's last frame fades out under it while the content fades in over 0.22 s.
Opening a conversation that is already loaded fades it in briefly, so the click visibly lands.
Channel glyphs in the sidebar grow a little under the pointer. Restoring a saved login shows a compact screen:
the app mark with a passing sheen, a determinate bar with a glowing head, and the stage with
the time left under it. The estimate follows the last few restores on this device, reaches
about 90% when that time has passed, stays under the current stage, and never fills before
the account is ready.

Calls follow their sounds. A seat waits at 92% size and faded while its own call first
connects (a comet circles just inside the avatar edge) or while it is only being rung. It
arrives in time with Discord's two-note join chime: the own comet closes into a ring on the
first note, the avatar springs to full size with a two-percent overshoot on the second, and a
soft halo spreads around it. Someone picking up springs and glows the same way, with no ring;
once the call is up nothing circles again, not while media re-secures for a newcomer nor during
a reconnect. Leaving plays no sound of Leech's own. The outgoing ring
starts once the call connects, after the chime has rung out, and keeps a fixed three-second
cadence; the ringing seat pulses on each burst of the ring sound, and on a plate its "Ringing"
status sits just above the avatar, its dots ebbing and refilling once per cycle. A seat whose person leaves
fades out where it sat while the others glide to their places. The direct-message stage unfolds
from the header when a call starts and folds back into it when the call ends, replaying its last
frame as it fades. A call known to have nobody else in it shows no join stage, so hanging up on
an unanswered call never shows the person rung as if they had joined. Rings and halos use the
call colour (Appearance, Accent): the theme accent lifted until it stands out on the dark stage,
or the owner's pick. Halos spread up to 14 points but never past the stage, so they are never
cut. Go Live uses the same motion on its button: while the share starts, the plate waits small
and faint while a loading line runs round its rounded edge; once the share's tile is on the
stage or Discord carries the share, whichever is first, the line closes round the plate, the
plate fills and springs to full size with Discord's stream sound, and only then does LIVE pop in
under the avatar. Stopping clears the button and LIVE on the click. The call options menu matches
Discord's: Grid View, Show My Own Camera, Show Non-Video Participants, Voice & Video Settings
and, for private calls, Region. Call animations repaint every frame while they run, like egui's own animations; a timer
would round to every other vsync and look laggy. The voice-only stage is never shorter than its
avatars and bar. While connecting, the voice panel's status tile breathes and eases from amber
to green. Message rows keep instant hover so reading stays fast.

Scroll bars stay out of the picture: nothing at rest, a faint 3px thread while the pointer is
over the area, a soft handle only when the bar itself is pointed at. Dialog sheets and the
share-source list hide the bar altogether; a cut-off last row says the sheet scrolls.

## Typography

Inter (Regular, Medium, SemiBold; SIL OFL 1.1) leads proportional text; egui's default faces and
the bundled Noto CJK/Arabic fallbacks follow in every family. egui has no synthetic bold, so
`design::semibold`/`design::medium` select the heavier families for author names, headings,
channel names and the uppercase 12px eyebrows of list sections. Body is 15px, small 12px. Until `fonts::install`
marks a context, the weight families resolve to the default face so headless tests never
reference an unknown family.

Appearance → Typography can import one TTF or OTF file up to 8 MiB, apply it immediately,
or reset to Inter. The selected face leads all proportional families; variable fonts use
400/500/600 weights while static fonts retain their supplied weight. Code stays monospace,
and bundled/system missing-glyph fallbacks remain available. A bounded local copy survives
restart and logout; import and persistence failures leave the current font in use.

Text is rasterized by egui on the CPU as grayscale coverage, not by DirectWrite or Core Text.
That cannot reproduce ClearType. `design::apply` turns TrueType hinting off and sub-pixel
binning on. Dark mode remaps coverage with `FontColorTransferFunction::Gamma(0.5)`. Light
mode leaves the transfer function off. Inter faces set `FontTweak.hinting` to `Some(false)`.
The bundled faces remain upstream's hinted TrueType builds. See `assets/README.md`.

## Layout

- 36px title strip (`base`): hidden native title bar on macOS with traffic lights inline, centred
  context title, session status text and an OFFLINE PREVIEW / EXPERIMENTAL pill. Eframe centres the
  native macOS buttons in the active title strip every frame, including after resizing or zooming;
  sign-in and login use their taller headers. Windows renders native-fidelity caption buttons
  (minimize, maximize/restore, close) flush in the top-right corner, and resize hit testing excludes
  the caption area so corner clicks hit Close. Windows and macOS General settings can hide the app strip
  and use native window decorations instead. Linux always omits the app strip and defaults to system
  decorations, with the Adwaita Wayland fallback on GNOME. General → Window can hide Linux decorations
  immediately for tiling window managers; the device preference also applies at startup.
- 72px server rail (`base`): 48px home button and server icons (circle, rounded square when
  hovered/selected), white edge pill (8px unread, 20px hover, 40px selected), red mention badges.
- The shell is three floating panels on the `base` backdrop: the lists, the conversation and
  the member pane are rounded cards (12px radius, one-pixel hairline) with an 8px gap between
  them and to the window edge; the account card under the lists shares their right edge. An
  open search pane lives inside the conversation card behind a divider. Content is painted
  square and `design::finish_panel` rounds each card afterwards, so headers, the call stage and
  media need no corner handling. Window images and see-through chrome keep flat columns, since
  a corner cannot be cut out of a picture with a flat colour. Channel sidebar
  (240px default, resizable): 48px header with the server name, category eyebrows with chevrons,
  32px rows with `#`/speaker/forum/thread glyphs, `selected`/`hover` fills, unread edge pill,
  mention badge; DM rows are 44px with 32px avatars. Forum rows open their post archive.
  As in Discord, the DM column puts "Find conversation" in its header strip and a Friends row
  above the conversations.
  Account card at the bottom (`raised`, avatar with presence dot, name, status, settings gear).
- Conversation header (48px): channel glyph or DM avatar, semibold name, then icon buttons
  (threads/archive, pins, member list toggle), a 144px search field and, in a DM outside its
  call, Discord's voice and video call buttons. Reload appears only when history failed or went
  stale or the connection is lost. A one-to-one DM has no member list, as in Discord. A thin
  notice strip appears only for loading/stale/archive/history states.
- Timeline: 16px gutters, 40px avatars, content at 72px, medium-weight author names followed
  by the author's server tag, 12px muted timestamps, `hover` row highlight, date dividers with a
  centred label, red "New messages" divider, floating hover toolbar (react, reply, edit, more)
  overlapping the row above. System rows (calls, joins, pins) are one line each and stack one
  line apart. GIFs wear a GIF badge; their favourite star shows under the pointer.
- Composer: rounded `raised` bar with a bare attach (+), placeholder `Message #channel`, then
  GIF, sticker and emoji pickers and send; a character counter appears within 200 characters
  of the limit.
- Member list (240px): ONLINE/OFFLINE eyebrows with counts; group DMs show a localized
  Members — N heading including the current account, updated with participant changes. 42px rows with
  presence dots, custom status and hover fill; opens a Members window on narrow layouts.
- Settings (a modal like Discord's): the sidebar opens with your avatar, name and an "Edit
  profile" link, then a search field and the pages with their icons under sentence-case
  section titles; the sections of an open page hang off a guide line. The page title and a
  plain close button sit in a slim bar above a centred column.
- Loading: the timeline fills upwards from the composer with placeholders shaped like the
  conversation (channel groups, direct replies or app embeds), and the member list shows a
  heading and avatar rows in the real rows' geometry. They repaint only while shown.

Confirmed empty guild text, announcement and thread histories show a welcome above the
composer: a circular channel-kind icon, a wrapped semibold channel heading and a short
description, using the active palette. Loading, unavailable, incomplete and historical
pages keep their existing status treatment; pending messages suppress the welcome.
`--features demo -- --demo --demo-empty-channel` previews this state offline;
`--demo-empty-channel-long` exercises a long Unicode name and `--demo-light` selects light mode.

Channel references and bare Discord message links share one inline pill. The leading icon
distinguishes text, announcement, voice, thread, forum, forum post, direct and group
conversations; message links append a chevron and message glyph, or the post name for forum
posts, and links to another joined server lead with its icon and name. Pills wrap like text,
keep one hit region per row and one keyboard stop, and expose a localized accessible label.
Destination names come only from metadata the session may view and read; hidden or unknown
targets show `unknown-channel` or `unknown-conversation`. Untrusted names are sanitized and
bounded before layout. Masked links retain their text, inline/fenced code stays literal, and
hidden spoilers remain concealed. Drag selection copies a link pill's original URL, and two
adjacent copies of the same URL stay separate. Click, Enter and Space activate native
navigation; the timeline centers and briefly highlights the exact target.
`--features demo -- --demo --demo-message-links` exercises these states with an offline
fixture; `--demo-light` selects the light appearance.

Icons are [Phosphor Icons](https://phosphoricons.com) 2.1.1 (MIT): interface glyphs in the bold
weight, brand logos, badges and file types in the fill weight, plus a few glyphs drawn in this
repository. They are rasterized once into `assets/icons/atlas.png` (white glyphs in 64px
cells) and tinted at draw time by `crates/ui/src/icons.rs`; there is no icon font. Provenance and the regeneration
command are in `assets/icons/README.md`. The profile popout keeps its 300px Discord-style card.

Shortcut capture also accepts egui's browser, media and system keys, including Caps Lock,
Pause and the macOS Fn key, when the OS delivers them to the focused window. These bindings
persist through the existing device settings. Global availability remains limited by the
native shortcut adapter; unsupported global bindings continue to work while Leech is focused.

Voice follows Discord's call screens: a stage that stays dark in every appearance and takes
its tones from the active theme's dark palette (plum in the house theme), with 80px participant avatars (DM calls, above
the conversation; everyone else first and you last, names on hover, a red badge for a mute; a
voice-only DM call opens only as tall as its avatars and the bar) or hairlined 16:9 tiles with
name badges (guild channels), a bottom control bar of dark pills (mute with settings chevron,
camera, screen share, more) and a red hang-up button, mute/deafen toggles in the account card,
whose status line reads "In a call" with a green handset, and a "Voice Connected" panel above
it while connected. As in Discord the panel's channel line sits a few points under its title,
underlines from the left under the pointer, and opens the call's conversation when clicked. Screen-share status is not shown on the stage; the share dialog and the button tooltip carry it. A DM call
that rings or runs without this device shows the same stage with whoever is in it and green
buttons to join with video or voice (and a red decline while it rings here); a call ringing in
another conversation keeps a slim banner. When another device of the account takes the call
over, the stage stays with that device's seat, and "Call continued on another device" unfolds
as a pill at the top of the stage, rests five seconds and folds away; a toast says it only when
the call's conversation is not open. The header and the bar
stay visible beside the avatars or tiles and fade out only over an enlarged video, which keeps
a slim 8px margin from the stage edges instead of touching the sidebar. In the channel list a
voice channel's participants stand in the channel-icon column (24px avatars under the speaker
glyph), and the stream preview card floats 8px clear of the sidebar edge. Stage tiles grow to
fill the stage up to 960px wide each, so a large window shows a full grid rather than one short row. The panel follows
Discord's: a signal glyph, noise suppression (a grey five-bar waveform, slashed while off) and
disconnect beside the title, then two equal cells for
camera (split with its settings chevron) and screen share, filled with the accent while on.
Each mute/deafen toggle shares one plate with its settings chevron and turns red while latched;
the call bar tints its microphone half the same way. Activities and soundboard are absent:
Leech has no such features.

## Verification notes

Native macOS captures at 1120×760 in Leech dark, Eclipse, Slate, Nightfall and light were
inspected on September 10, 2026 with the offline fixtures (`--demo`, `--demo-chat`,
`--demo-notifications`, `--demo-voice`). Keyboard reachability of channel rows, the forum row,
toolbar actions and members is covered by headless egui tests. Screen-reader/IME behaviour and
Windows/Linux rendering (including the inline title bar, which is macOS-only) remain unverified.
Palette contrast is asserted by a unit test for every opaque preset: body text ≥ 7:1 on `chat`,
muted text ≥ 4.5:1 on `sidebar`, accent text ≥ 4.5:1 on `accent`. Gradient presets are not
contrast-certified because their surfaces are translucent.

The empty-channel welcome was inspected natively on Ubuntu 26.04.1 at 1120×760
and at 760×520 with a long Unicode name in light/dark mode. The committed pair
uses an isolated Xvfb display; the offline composer was also exercised by typing
and pressing Enter, which replaced the welcome with the synthetic message.

Server Notification Settings are available from the server dropdown and server
icon context menu, independently of administrative Server Settings. The shared
modal uses four radio choices, mute and mention-suppression switches, and an
explicit Save changes action. The content scrolls in short/narrow viewports while
the footer remains accessible. `--demo --demo-chat --demo-server-notifications`
with `--features demo` opens the synthetic editor without contacting Discord.

Poll cards follow the desktop layout shown in Discord's
[Polls FAQ](https://support.discord.com/hc/en-us/articles/22163184112407-Polls-FAQ):
a question above stacked, left-aligned answer rows, right-edge selection circles
and checkmarks, separate vote counts and percentages, and a compact vote/expiry
footer. The card spans up to 472px including padding. Long answers wrap while
their selection markers stay aligned; narrow result rows put tallies beneath the
answer. Colors continue to resolve from the active Leech palette.

Open poll creation from Create a poll in the composer's `+` menu.
The creation dialog uses uppercase field labels, inset question/answer fields,
answer removal icons, a duration row and a separate footer with multiple-choice
and Post controls. Its content scrolls while the footer remains available. Escape,
the close icon and a backdrop click dismiss an idle editor; a pending submission
keeps it open. Unknown results and expired but unfinalized polls retain explicit
status text. `--features demo -- --demo --demo-polls` previews the fixture offline.
