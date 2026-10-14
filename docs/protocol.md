# NektoMe Wire Protocol (v4.1.7)

Everything below was recovered by static analysis of
`Анонимный+чат+NektoMe_4.1.7_APKPure.xapk` (jadx decompilation of the DEX +
resource inspection). Full findings: `reports/nektome_4.1.7_endpoints.md` in the
`apk_flash` analysis workspace.

## Transport

| Aspect | Value |
|---|---|
| Protocol | Socket.IO over WebSocket |
| Socket path | `/android` |
| Server | `https://im.nektome.online/` (Remote Config default) |
| Engine.IO version | v3 (Android client uses socket.io-client 1.x ⇒ socket.io v2 wire) |
| Emit event | `"action"` — the payload **is** the action object, including its `"action"` name field |
| Listen event | `"notice"` — envelope `{"notice", "data", "badge", "page"}` |
| JSON | camelCase; `null` fields omitted from payloads |
| Timezone field | `timeZone` = `TimeZoneInfo.Local.BaseUtcOffset` formatted `\+HH:mm` (e.g. `+03:00`) |

## Authentication

The client requests a token with `auth.getToken` on first run, and re-sends the
stored token with `auth.sendToken` afterwards. The server replies with
`auth.successToken` / `auth.successUser`. Text chat uses the current browser
profile: `deviceType` = 2, `version` = 14, `isTouch` = false, and `t` is the
current Unix time in milliseconds. The older Android signed-device payload is
rejected by the production server as expired data.

## Outbound actions

| Action | Payload fields |
|---|---|
| `auth.sendToken` | `token`, `deviceType`, `version`, `isTouch`, `t`, `timeZone`, `locale` |
| `auth.getToken` | `deviceType`, `version`, `isTouch`, `t`, `timeZone`, `locale` |
| `search.run` | `isAdult?`, `isRole`, `isVoiceDisable?`, `myAge` (int[]), `mySex` (`M`/`F`/`null`), `wishAge` (int[][] — pairs), `wishSex` (`M`/`F`/`null`) |
| `search.sendOut` | none (bare `{"action":"search.sendOut"}`) |
| `dialog.setTyping` | `dialogId`, `typing` (bool), `voice` (bool) |
| `dialog.info` | `dialogId` |
| `anon.leaveDialog` | `dialogId` |
| `anon.message` | `dialogId`, `message` (text), `randomId` (long, client-generated) |
| `anon.readMessages` | `dialogId`, `lastMessageId` (highest read id) |
| `online.track` | `on` (bool) |
| `captcha.verify` | `solution` |
| `antispam.report` | `dialogId`, `messageId`, `reasonId` |

## Inbound notices

| Notice | `data` contents |
|---|---|
| `error.code` | `{code or id, description, additional?}`; captcha ID `600` includes `leftChats`, `needChats`, and `publicKey` in `additional` |
| `auth.successToken` / `auth.successUser` | `{config, id, statusInfo, tokenInfo}` — `tokenInfo.authToken`, `statusInfo.communicationBan`, `statusInfo.anonDialogId`, plus server `config` (ages, report reasons, voice settings, rules) |
| `dialog.opened` / `dialog.info` | `{id, interlocutors[], messages[], supportVoice, createTime, updateTime, close}` — each message: `{id, dialogId, senderId, message, randomId?, read, createTime}` |
| `dialog.closed` | either an object `{id}` or a **bare numeric id** |
| `dialog.typing` | `{dialogId, typing, voice?}` |
| `search.success` | — (searching started) |
| `search.out` | — (search cancelled) |
| `messages.new` | single message object (same shape as in `dialog.opened`) |
| `messages.reads` | `{dialogId, reads[]}` |
| `dialog.paid` | paid-dialog payload |
| `online.count` | online users counters |
| `captcha.verify` | `{solution}` — server sends the solution for auto-verification |
| `socket.close` | server-requested disconnect |
| `purchase.changed` | purchase status |

## Semantics implemented in the client

- **Own messages**: a `messages.new` echo is "own" when its `randomId` matches a
  `randomId` sent in this session, or `senderId` equals our authenticated `id`.
- **Read receipts**: `anon.readMessages` is sent with the highest known message id
  (history load + every newly received foreign message).
- **Timestamps**: server sends unix **seconds** (~1e9); values ≥ 1e12 are treated
  as unix milliseconds.
- **Captcha**: text search may return error ID `600`. The desktop UI opens an
  isolated Chrome-family browser profile for the official challenge, observes
  successful verification, adopts that verified text session, reconnects, and
  resumes the search. A `captcha.verify` notice confirms manual verification.
- **Sex wire values**: `"M"` / `"F"` / omitted (`null`); `wishAge` is an array of
  `[from, to]` pairs; `myAge` a flat array of allowed ages.

## Voice chat roulette — NOT part of this protocol

The voice roulette is a **separate Android app** (`com.nektome.chatruletka.voice`).
The main app's `/android` socket carries **no** voice signaling at all:

- `auth.successToken.config.audioChat` (`chance`, `count_first`, `count_max`,
  `url`, `version`) is only a cross-promo config shown as a banner after a
  finished chat and used to launch the companion app
  (`getLaunchIntentForPackage`), nothing else.
- Voice *messages* are REST-only (`api/v1/billing/file/audio`,
  `get-file/{fileId}`).

The companion app (analyzed v1.7.1) uses its own socket.io transport — a
different host from `nekto.me/audioserver.json`, event channel `"event"` with a
flat `{"type": ...}` envelope — and libwebrtc for audio; the server assigns the
offer initiator and ephemeral TURN credentials in `peer-connect`. Full
breakdown: `/Volumes/External/Code/apk_flash/reports/nektome_voice_roulette.md`.

## Voice roulette wire protocol (companion app, v1.7.1)

Implemented by this client in `src/voice.rs` and `src/voice_session.rs`.

### Transport & identity

| Aspect | Value |
|---|---|
| Signaling server | `https://audio.nekto.me/`, Socket.IO path `/androiduk` |
| Endpoint resolution | Firebase Remote Config (`audiochat_server_url` / `audiochat_server_path`); production RC overrides only the path (`/androiduk`), the URL falls back to the APK default `https://audio.nekto.me/`. The legacy bootstrap `GET https://nekto.me/audioserver.json` is **dead** (404 SPA HTML), and `audiochat.nekto.me` (web client) fails with Cloudflare 526 |
| Protocol | Socket.IO over WebSocket only (Engine.IO v3) |
| Emit + listen channel | `"event"` — **single channel, both directions** |
| Envelope | flat JSON: `{"type": "<event-name>", …fields}` (no nested `data`) |
| Headers | `NektoMe-Chat-Version: 2`, `App-Android-Version: 1.7.1`, `App-Android-Code: 96`, `Android-Language`, custom `User-Agent` |
| Identity | `userId = "<store>-<id>"` (Android: e.g. `google-…`); this client uses `google-<uuid>` persisted at `~/.nektome-client/voice-user.json` |

### Outbound events

| `type` | Fields |
|---|---|
| `register` | `android` (bool), `userId`, `connectionId?`, `peerSuccess?`, `locale`, `timeZone?` |
| `scan-for-peer` | `searchCriteria{userSex? (MALE/FEMALE/ANY), peerSex? (MALE/FEMALE/ANY), peerAges? ([{from,to}]), userAge? ({from,to}), group?}`, `peerToPeer: true`, `token?`, `tokenType: IMAGE/RECAPTCHA` |
| `stop-scan` | — |
| `offer` / `answer` | `connectionId`, plus a field holding the SDP **as a JSON string**: `"{\"type\":\"offer\",\"sdp\":\"…\"}"` |
| `ice-candidate` | `connectionId`, `candidate` = JSON string `"{\"candidate\":{\"sdpMid\"…,\"sdpMLineIndex\"…,\"candidate\"…}}"` |
| `peer-mute` | `connectionId`, `muted` (bool) — also sent with `false` at initial media setup |
| `peer-connection` | `connection` (bool), `connectionId?` — sent with `true` + `connectionId` after `stream-received`, sent with `false` (no id) on teardown |
| `peer-trouble` | `connectionId` |
| `peer-disconnect` | `connectionId` |
| `peer-soft-disconnect` | `connectionId` (skips the stranger without ending the search) |
| `stream-received` | `connectionId` (sent after the remote track starts rendering) |
| `users-count-request` | — |
| `log-ping-results` | `result` = `[{server, ping, fails}]` — answer to `ping-server-request`; `ping` = best (min) RTT in ms, `-1` if the host never answered; `fails` = unanswered probes. Reference probes with `ping -i 0.5 -c <attempts>` |
| service | `abuse-report`, `client-log` |

> **Endpoint migration (2026-09):** the voice backend moved to `audio.nekto.me`
> with the Android-only Socket.IO path `/androiduk`. The Android client resolves
> it exclusively through Firebase Remote Config keys `audiochat_server_url` /
> `audiochat_server_path` — the old JSON bootstrap document is no longer
> published. This client ships the live endpoint as `VoiceOptions.StaticEndpoint`
> and keeps the legacy bootstrap only as an opt-in fallback.
> The Rust UI can persist an explicit server-host selection; without that
> selection, `NEKTOME_VOICE_SERVER` and the opt-in bootstrap remain authoritative.

### Inbound events

| `type` | Fields |
|---|---|
| `registered` | `success`, `connectionId`, `errorCode?`, `internal_id`, `premium`, `recaptchaSiteKey?`, `config` |
| `search.success` | — (scan started) |
| `peer-connect` | `connectionId`, `initiator` (bool — server picks the offer side), `time`, `relay` (bool), `stunUrl`, `turnParams` (**JSON-encoded string** `"[{\"url\":...}]"` or raw array) |
| `peer-connection` | `connectionId` (ack of candidate/offer traffic) |
| `offer` / `answer` | `connectionId` + SDP JSON string |
| `ice-candidate` | `connectionId` + candidate JSON string |
| `peer-disconnect` / `peer-soft-disconnect` | `connectionId` |
| `peer-mute` | `muted` (peer's state) |
| `users-count` | `usersCount`, `waitingUsersCount`, `talkingUsersCount` |
| `ping-server-request` | `attempts` (probe count), `list[]` (hosts to measure); answered with `log-ping-results` |
| `registered` `errorCode` | **int** (0 on success) — not a string |
| `captcha-request` | `captchaType`, `leftChats`, `needChats`, `url`, `report` |
| `ban` | `banInfo{banEnum, permanently, text}` — APK 1.7.1's `BanEnum` only has `asn`/`ip`/`token`; the live server also sends a generic `"BAN"` the old client cannot map (Gson → `null`). Observed live payload: `permanently:true, banEnum:"BAN"`, text = IP-level block («Доступ к чатам с Вашего IP-адреса заблокирован… подозрительная активность… если подключен VPN, выключите его») |
| `error` | error payload |
| `purchase-changed` | `paidType` |

### Call flow

1. `register` → `registered{connectionId}` (re-register with the same
   `connectionId` after reconnects; a `peerSuccess` flag is cleared once used).
   The reference client also persists the last id across process restarts
   (`connectionIdLast`) and re-sends it on the first register of a fresh
   session — verified live via Frida: a cold APK start re-sent
   `connectionId:"3446150251"` from a previous run.
2. `scan-for-peer` → `search.success` → `peer-connect` with role
   (`initiator`) and ICE credentials.
3. Initiator: create offer → `offer`; the other side answers with `answer`.
   Both trickle `ice-candidate`s (candidates received before the remote
   description are buffered, then flushed).
4. First remote RTP packet ⇒ media established; send `stream-received`.
5. End: `peer-disconnect` (new search) or `peer-soft-disconnect`
   (skip, keep searching).

### Media

WebRTC audio negotiates Opus by default, with G.711 PCMU/PCMA alternatives.
The Rust client uses `webrtc-rs`'s `RTCPeerConnection`; set
`NEKTOME_AUDIO_CODEC=pcmu` or `pcma` to select a G.711 offer. Its managed
G.711 codec and bundled Opus path decode remote PCM to an `AudioSink` (native
macOS, dump-to-WAV optional).

Local capture runs the same route in reverse: an `IMicrophoneCapture` port
(CoreAudio AudioQueue input, 8 kHz mono 16-bit PCM, 4×100 ms ring buffers)
starts when remote audio first lands (RTP is up both ways) and stops on every
chat teardown. Frames are dropped client-side while not established or muted,
so a mute pressed mid-chat cannot replay pre-mute audio.
