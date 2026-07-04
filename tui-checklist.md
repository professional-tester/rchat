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
- [ ] Roster/details
- [ ] Send group text/media references
- [ ] Receipts/sync status

## Polish Track

- [ ] Context menus
- [ ] Search/filter
- [ ] Richer sidebar states
- [ ] Mouse support beyond basic click/scroll
- [ ] Command palette completion/help/errors

## Core Extraction Track

- [x] `settings::*`
- [x] `chat::media`
- [ ] Remaining sticker/media command logic
