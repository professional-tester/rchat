# RChat TUI Checklist

Living tracker for bringing `rchat-tui` to GUI parity. Keep each iteration small,
testable, and shippable.

## Done / Mostly Done

- [x] Auth/unlock flow
- [x] Direct 1:1 chat list/history/composer
- [x] Inline image/sticker preview
- [x] Local screen/media smoke tests
- [x] Envelope grouping basics
- [x] New Person modal: mDNS connect, GitHub/Gist create/redeem, temporary invite create/redeem/cancel, QR display/decode

## Next: Settings Foundation

- [x] Profile editing
- [x] Trusted peers/friends
- [x] Connectivity mode
- [x] Theme presets/custom theme editor
- [x] Sticker management
- [x] Media diagnostics/about pages

## Then: Sending Attachments

- [x] Send image/document/audio/video from path
- [x] Send sticker picker
- [x] Composer action bar entry points for attachments and stickers
- [x] Add sticker from path inside the sticker picker
- [x] Save received sticker to local sticker library from message actions
- [x] Retry failed attachment fetch
- [x] Open/save/copy actions on attachment cards

## Then: Media Viewers

- [x] Full image viewer with zoom/pan/save/open
- [x] Video attachment viewer with thumbnail/metadata/open externally
- [x] Audio/document viewer actions

## Then: Live Media UI

- [x] Voice call start/accept/reject/end/mute
- [x] Video call start/accept/reject/end/mute/camera toggle
- [x] Remote video render in normal TUI flow
- [x] Screen-share hosting UI and profile picker
- [x] Polished incoming screen-share prompt

## Then: Group Chat UI

- [x] Group list items
- [x] Group invite accept/reject
- [ ] Roster/details (deferred)
- [ ] Send group text/media references (deferred)
- [ ] Receipts/sync status (deferred)

## Polish Track

- [x] Context menus for chats, envelopes, and attachment messages
- [x] Search/filter for conversations
- [x] Richer sidebar states: pinned, unread, online/offline, temporary, group, active live/share
- [x] Mouse support beyond basic click/scroll: action buttons and right-click menus
- [x] Remove command palette from normal user-facing UX/help

## Core Extraction Track

- [x] `settings::*`
- [x] `chat::media`
- [x] Remaining sticker/media command logic
