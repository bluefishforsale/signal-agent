# Tool audit: upstream signal-mcp 1.43.3 against presage

PRD goal 6 makes parity with upstream signal-mcp's 81 tools the v2 end state, and anti-goal 8 says any tool presage can't back joins the v2.x list. This file applies that rule to every tool, so the v2.0 tool batches (L3 to L3e) know what to build and what to leave out. Tool names stay identical to upstream.

## What was checked

- Upstream: the `signal-mcp==1.43.3` wheel from PyPI, `signal_mcp/server.py`, every `Tool(...)` entry (81, counted by parsing the file), plus `signal_mcp/tool_annotations.py` for the read-only set. Primary-only tools are the ones whose description says "Only works when signal-mcp is the account's primary device" or "Primary device only".
- presage at `6f63079` (2026-10-19), `presage/src/manager/registered.rs`: the public `Manager<S, Registered>` methods, and `presage/src/store.rs` for what the store holds (contacts, groups, sticker packs, profiles, avatars). presage has no storage-service client (no reference to `storage_service` anywhere under `presage/src`) and no group-change API.
- libsignal-service-rs at `fae0679`, the revision presage pins: `protobuf/SignalService.proto` for which message types exist (`DataMessage` carries `pollCreate`, `pollVote`, `pollTerminate`, `pinMessage`, `unpinMessage`, `adminDelete`, `sticker`, `expireTimer`; `ContentBody` has `EditMessage`, `StoryMessage`, `TypingMessage`, `ReceiptMessage`, `SynchronizeMessage`), `src/groups_v2/manager.rs` (fetch and decrypt only, no create or modify), and `src/websocket/stickers.rs` (download only, no pack upload).

## Classifications

- **presage-backed**: a presage `Manager` method or presage store query does it.
- **buildable on send_message**: we build the protobuf content ourselves and push it through presage's `send_message` or `send_message_to_group`; presage doesn't need to change.
- **local store only**: our message store or local config, no Signal protocol involved.
- **deferred to v2.x**: needs protocol work presage lacks. Anti-goal 8 covers most; the extra ones are marked "extra".

"Write" means the tool sends, changes the account or server state, or deletes local history; those are hidden in read-only mode. "Primary" means Signal only allows it from a primary device; those are hidden on linked profiles.

## Tools

| # | Tool | What it does | R/W | Primary | Classification | Backing / reason |
|---|---|---|---|---|---|---|
| 1 | send_message | Text to one contact | W | | presage-backed | `send_message` |
| 2 | send_group_message | Text to a group | W | | presage-backed | `send_message_to_group` |
| 3 | send_note_to_self | Text to Note to Self | W | | buildable on send_message | `send_message` to own ACI |
| 4 | edit_message | Replace text of a sent message | W | | buildable on send_message | `ContentBody::EditMessage` |
| 5 | receive_messages | Poll for new messages into the store | R | | presage-backed | `receive_messages`; in v2 the agent runs it continuously, so the tool reports what the loop stored |
| 6 | receive_direct | signal-cli daemon bypass for receiving | R | | presage-backed | Same loop as receive_messages; there is no daemon to bypass, kept for name parity |
| 7 | list_contacts | List contacts in the local contact store | R | | presage-backed | store `contacts()` |
| 8 | list_groups | List known groups | R | | presage-backed | store `groups()` |
| 9 | get_conversation | One conversation's history | R | | local store only | `MessageStore::history` |
| 10 | search_messages | Full-text search across conversations | R | | local store only | `MessageStore::search` (FTS5) |
| 11 | send_attachment | Files to one contact | W | | presage-backed | `upload_attachments` + `send_message` |
| 12 | send_group_attachment | Files to a group | W | | presage-backed | `upload_attachments` + `send_message_to_group` |
| 13 | react_to_message | Add or remove an emoji reaction | W | | buildable on send_message | `DataMessage.reaction` |
| 14 | set_typing | Typing indicator on or off | W | | buildable on send_message | `TypingMessage` |
| 15 | get_profile | Look up a number on Signal's servers | R | | presage-backed | `discover_contacts_by_phone_number` (presage `cdsi` feature) + `retrieve_profile_by_uuid` |
| 16 | block_contact | Block a contact | W | yes | deferred to v2.x | Anti-goal 8: storage-service contact edit; presage has no storage-service client |
| 17 | unblock_contact | Unblock a contact | W | yes | deferred to v2.x | Anti-goal 8, as block_contact |
| 18 | remove_contact | Delete a contact entry | W | | deferred to v2.x | Anti-goal 8, as block_contact |
| 19 | update_profile | Change own name, about, emoji, avatar | W | | presage-backed | `update_profile(name, about, emoji)`; presage only uploads without an avatar, so `avatar_path`, `remove_avatar` and `mobilecoin_address` are refused until presage exposes `upload_versioned_profile` |
| 20 | create_group | Create a group | W | | deferred to v2.x | Anti-goal 8: group administration; `groups_v2::manager` has no create |
| 21 | join_group | Join from an invite link | W | | deferred to v2.x | Anti-goal 8: group administration |
| 22 | list_devices | List the account's devices | R | | presage-backed | `devices()` |
| 23 | add_device | Link a new device by its URI | W | yes | presage-backed | `link_secondary` (returns `NotPrimaryDevice` on a linked device) |
| 24 | remove_device | Unlink a device | W | yes | presage-backed | `unlink_secondary` (returns `NotPrimaryDevice` on a linked device) |
| 25 | get_own_number | This account's number | R | | presage-backed | registration data, via `SignalBackend::account` |
| 26 | store_stats | Counts and date bounds of the store | R | | local store only | `MessageStore::stats` |
| 27 | get_unread | Unread incoming messages | R | | local store only | `MessageStore::unread`; the agent receives continuously, so no poll is needed |
| 28 | import_desktop | Import Signal Desktop history | W | | local store only | Reads Desktop's database, no protocol; PRD goal 4 opt-in extra (lane L7) |
| 29 | sync_desktop | Incremental Desktop import | W | | local store only | As import_desktop |
| 30 | list_conversations | Inbox overview | R | | local store only | `MessageStore::conversations` |
| 31 | get_user_status | Whether numbers or usernames are on Signal | R | | presage-backed | `discover_contacts_by_phone_number` + `lookup_username`; username lookup, not username management |
| 32 | send_sync_request | Ask the primary for contacts, groups, settings | W | | presage-backed | `request_contacts`; the groups and configuration request types are the same `SyncMessage.Request` built on send_message |
| 33 | delete_message | Delete a sent DM for everyone | W | | buildable on send_message | `DataMessage.delete` |
| 34 | delete_group_message | Delete a sent group message for everyone | W | | buildable on send_message | `DataMessage.delete` to the group |
| 35 | send_read_receipt | Read receipts to a sender | W | | buildable on send_message | `ReceiptMessage` |
| 36 | update_contact | Set a contact's local name, nickname, note | W | | deferred to v2.x | Anti-goal 8: storage-service contact edit |
| 37 | update_group | Change a group's details, members, admins | W | | deferred to v2.x | Anti-goal 8: group administration; no group-change API |
| 38 | leave_group | Leave a group | W | | deferred to v2.x | Anti-goal 8: group administration |
| 39 | terminate_group | End a group for everyone | W | | deferred to v2.x | Anti-goal 8: group administration |
| 40 | pin_message | Pin a message for everyone | W | | buildable on send_message | `DataMessage.pinMessage` |
| 41 | unpin_message | Remove a pin | W | | buildable on send_message | `DataMessage.unpinMessage` |
| 42 | admin_delete_message | Admin deletes a member's group message | W | | buildable on send_message | `DataMessage.adminDelete`; a message, not a group change, so outside anti-goal 8 |
| 43 | send_contacts_sync | Push the contact list to own devices | W | | buildable on send_message | `SyncMessage.contacts` with an uploaded contacts blob; little use until contact edits ship |
| 44 | update_device | Rename a device | W | partly | deferred to v2.x | Extra: libsignal-service has `update_device_name`, presage doesn't expose it; renaming other devices is primary-only |
| 45 | mark_as_unread | Mark stored messages unread again | R | | local store only | Needs a `mark_unread` on `MessageStore`, which it lacks today (lane L2) |
| 46 | get_avatar | A contact's or group's avatar | R | | presage-backed | `retrieve_profile_avatar_by_uuid`, `retrieve_group_avatar` |
| 47 | send_message_request_response | Accept or decline a message request | W | | buildable on send_message | Profile key to the sender plus `SyncMessage.messageRequestResponse` to own devices |
| 48 | create_poll | Send a poll | W | | buildable on send_message | `DataMessage.pollCreate` |
| 49 | vote_poll | Vote on a poll | W | | buildable on send_message | `DataMessage.pollVote` |
| 50 | terminate_poll | Close own poll | W | | buildable on send_message | `DataMessage.pollTerminate` |
| 51 | set_expiration_timer | Disappearing-messages timer | W | | buildable on send_message | DMs: `DataMessage.expireTimer` with the timer-update flag. With `group_id` it's a group attribute change, deferred under group administration, so the group form is refused in v2.0 |
| 52 | list_identities | Stored identity keys and trust | R | | presage-backed | protocol store `get_identity` per contact |
| 53 | trust_identity | Trust a changed identity key | W | | presage-backed | protocol store `save_identity` |
| 54 | clear_local_store | Delete all stored messages | W | | local store only | Message store |
| 55 | delete_local_messages | Delete one conversation locally | W | | local store only | Message store |
| 56 | export_messages | Export the store as JSON or CSV | R | | local store only | Message store export |
| 57 | update_configuration | Read receipts, typing, link previews settings | W | yes | deferred to v2.x | Anti-goal 8: account settings (live in the storage-service account record) |
| 58 | list_sticker_packs | Installed sticker packs | R | | presage-backed | store `sticker_packs()` |
| 59 | add_sticker_pack | Install a pack from its link | W | | presage-backed | `install_sticker_pack` |
| 60 | send_sticker | Sticker to a contact | W | | buildable on send_message | `DataMessage.sticker` |
| 61 | send_group_sticker | Sticker to a group | W | | buildable on send_message | `DataMessage.sticker` to the group |
| 62 | send_story | Post an image or video story | W | | deferred to v2.x | Extra: without `group_id` it posts to My Story, whose distribution list lives in the storage service presage can't read; only the group-story half is buildable |
| 63 | list_attachments | Locally saved attachment files | R | | local store only | Attachment folder |
| 64 | get_attachment | Fetch one attachment | R | | presage-backed | `get_attachment` |
| 65 | get_sticker | One sticker image | R | | presage-backed | store `sticker_pack`, which keeps sticker bytes |
| 66 | upload_sticker_pack | Publish a new sticker pack | W | | deferred to v2.x | Extra: libsignal-service at `fae0679` has no sticker upload endpoint (`websocket/stickers.rs` is download only) |
| 67 | list_accounts | Accounts on this machine | R | | local store only | Our profile list |
| 68 | update_account | Device name, discoverability, username | W | | deferred to v2.x | Anti-goal 8: account settings and usernames |
| 69 | set_pin | Set the registration-lock PIN | W | yes | deferred to v2.x | Anti-goal 8: registration-lock PIN |
| 70 | remove_pin | Remove the registration-lock PIN | W | yes | deferred to v2.x | Anti-goal 8: registration-lock PIN |
| 71 | start_change_number | Begin moving the account to a new number | W | yes | deferred to v2.x | Extra: presage has no change-number flow (PNI key distribution exists in libsignal-service, unused by presage) |
| 72 | finish_change_number | Complete the number change | W | yes | deferred to v2.x | Extra, as start_change_number |
| 73 | submit_rate_limit_challenge | Lift a send rate limit with a captcha | W | | presage-backed | `submit_recaptcha_challenge` |
| 74 | prune_store | Delete stored messages older than N days | W | | local store only | Message store prune |
| 75 | set_webhook | Save or clear the webhook URL | W | | local store only | Local config |
| 76 | get_webhook | Current webhook URL | R | | local store only | Local config |
| 77 | find_contact | Substring search over contacts | R | | presage-backed | store `contacts()`, filtered locally |
| 78 | schedule_message | Save a message to send later | W | | local store only | Stored job; delivery is run_scheduled_messages |
| 79 | list_scheduled_messages | Scheduled jobs | R | | local store only | Stored jobs |
| 80 | cancel_scheduled_message | Cancel a scheduled job | W | | local store only | Stored jobs |
| 81 | run_scheduled_messages | Send every due scheduled message | W | | buildable on send_message | `send_message` / `send_message_to_group` per due job |

## Counts

| Classification | Tools |
|---|---|
| presage-backed | 25 |
| buildable on send_message | 19 |
| local store only | 19 |
| deferred to v2.x | 18 |
| Total | 81 |

Of the 18 deferred, 13 are already named in anti-goal 8: block_contact, unblock_contact, remove_contact and update_contact (storage-service contact edits), create_group, join_group, update_group, leave_group and terminate_group (group administration), update_configuration and update_account (account settings, usernames), set_pin and remove_pin. The rule adds five more, which belong on the PRD's v2.x list:

- update_device: presage doesn't expose `update_device_name`.
- send_story: My Story needs storage-service distribution lists.
- upload_sticker_pack: no upload endpoint in the pinned libsignal-service.
- start_change_number, finish_change_number: presage has no change-number flow.

Two v2.0 tools ship with a narrower input than upstream, refusing the rest as invalid input rather than deferring the whole tool: update_profile (no avatar or MobileCoin fields) and set_expiration_timer (DMs only).

## Primary-only

Upstream marks block_contact, unblock_contact, add_device, remove_device, update_configuration, set_pin, remove_pin, start_change_number and finish_change_number as primary-only, and update_device when renaming another device. Of those, v2.0 ships add_device and remove_device, both with `needs_primary` set so the registry hides them on linked profiles.
