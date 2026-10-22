# Link-and-sync: how a new linked device gets its history

When you link a new device, the phone can offer to "transfer message history". This file records exactly how that works on the wire, so `relay-signal` (lane L1) and `relay-history` (lane L7) can implement it without re-deriving it, and so the next person can check every claim against source.

Sources read, all at the commits below (cloned 2026-10-21):

- Signal-Desktop `17502b9d1a`, the reference for a *receiving* linked device.
- Signal-Android `b549495785`, the reference for the *sending* primary.
- libsignal tag `v0.99.0` (`439f035d9c`), the version libsignal-service-rs pins, so the one we must use.
- libsignal-service-rs `fae0679db1` and presage `6f630791b2` (the revisions presage's `Cargo.toml` pins).

Each claim below is marked **verified** (read in source at those commits, with the file and line) or **inferred** (reasoned from source or product copy, not traced end to end). Nothing here was tested against Signal's servers.

## Verdict

Feasible, with three additions to the presage stack, none of them crypto. Decryption and parsing are fully covered by libsignal's `libsignal-message-backup` crate, and `relay-history` now does that part (see "What relay-history does" below). What's missing is all network plumbing on the receiving side:

1. **Advertise the capability in the QR code.** libsignal-service-rs builds the `sgnl://linkdevice` URL with only `uuid` and `pub_key` (`src/provisioning/pipe.rs:100-111`, **verified**). Android only offers sync when the URL's `capabilities` query parameter contains `backup5` (`LinkDeviceViewModel.kt:176-180`, `:434-436`, **verified**), and Desktop sends `capabilities=nopni,nopni2,backup5` (`ts/textsecure/Provisioner.preload.ts:429-433`, **verified**). Without `backup5` the phone links "without sync" and never sends a key. L1 can append the parameter to the `ProvisioningStep::Url` it gets back, before rendering the QR code, and offer the change upstream.
2. **Keep the ephemeral backup key from provisioning.** Already done upstream: presage exposes `RegistrationData::ephemeral_backup_key() -> Option<[u8; 32]>`, deliberately not persisted (`presage/src/manager/registered.rs:157-177`, **verified**). L1 has to read it in the same process, right after `link_secondary_device` returns.
3. **Fetch the archive.** An authenticated long-poll on `GET /v1/devices/transfer_archive` and an unauthenticated CDN download. Neither libsignal-service-rs nor presage has the long-poll (searched both trees for `transfer_archive`, no hits, **verified**). `PushService::get_from_cdn` exists but is `pub(crate)` (`src/push_service/cdn.rs:97`, **verified**).

## The flow, step by step

1. **QR code.** The new device opens a provisioning socket and shows `sgnl://linkdevice?uuid=…&pub_key=…&capabilities=backup5[,…]`. **Verified** (Desktop, above).
2. **Phone decides.** If `backup5` is present the phone shows a "transfer message history" choice. If the user accepts, the phone makes a fresh random 32-byte key, `MessageBackupKey(Util.getSecretBytes(32))` (`LinkDeviceViewModel.kt:265`, **verified**), and puts it in the provisioning message.
3. **Provisioning message.** `ProvisionMessage.ephemeralBackupKey`, field 14, 32 bytes (`protos/DeviceMessages.proto:42`, **verified**). libsignal-service-rs decodes it into `NewDeviceRegistration::ephemeral_backup_key` (`src/provisioning/mod.rs:262-268`, **verified**). This is the only channel the key travels on; it's end-to-end encrypted to the new device's provisioning key, and the server never sees it.
4. **New device finishes linking** (`PUT /v1/devices/link`), as presage already does. Android waits at most 60 seconds for that (`waitForDeviceToBeLinked(result.token, maxWaitTime = 60.seconds)`, `LinkDeviceViewModel.kt:289`, **verified**), so the device must complete linking promptly after the scan.
5. **Phone exports and uploads.** Android builds the archive with `exportForLinkAndSync` (`BackupRepository.kt:818-844`, **verified**), uploads it as an ordinary attachment through a v4 upload form, and then tells the server where it is with `PUT /v1/devices/transfer_archive`, body `{destinationDeviceId, destinationDeviceRegistrationId, transferArchive: {cdn, key}}` or `{…, transferArchive: {error}}` (`LinkDeviceApi.kt:178-192`, `SetLinkedDeviceTransferArchiveRequest.kt`, **verified**).
6. **New device long-polls** `GET /v1/devices/transfer_archive?timeout=<secs>` on the chat service, authenticated as the newly linked device (Desktop `_ajax({host: 'chatService', call: 'transferArchive'})`, `ts/textsecure/WebAPI.preload.ts:2393-2433`, endpoint string at `:820`, **verified**). Desktop caps each request at 5 minutes, keeps retrying for up to an hour, and treats `204` as "not yet, ask again". A `200` carries one of:
   - `{"cdn": <number>, "key": "<string>"}`: the archive is ready.
   - `{"error": "RELINK_REQUESTED"}`: the phone failed and wants the user to relink.
   - `{"error": "CONTINUE_WITHOUT_UPLOAD"}`: the phone gave up; carry on with no history.

   (Response schema at `WebAPI.preload.ts:1495-1506`, handling at `ts/services/backups/index.preload.ts:1046-1066`, **verified**.)
7. **Download.** `GET https://<cdn N host>/attachments/<url-encoded key>`, unauthenticated, resumable with a `Range: bytes=<offset>-` header (`getEphemeralBackupStream`, `WebAPI.preload.ts:3235-3251`, and `_getAttachment`, `:4217-4250`, **verified**). Desktop writes it to a file first, because decryption needs two passes over it.
8. **Decrypt and import** (below). Desktop then deletes the file and forgets the key (`index.preload.ts:301-304`, **verified**).

## Keys

The provisioning key is used as a `BackupKey` directly; it is *not* derived from the account entropy pool. **Verified**: Desktop passes `new BackupKey(options.ephemeralKey)` into `getKeyMaterial` (`index.preload.ts:840-842`), which builds `new MessageBackupKey({backupKey, backupId: backupKey.deriveBackupId(aci)})` with no forward-secrecy token (`ts/services/backups/crypto.preload.ts:64-74`). Android's writer for this path is `createForLocalOrLinking`, which also passes `forwardSecrecyToken = null` and derives the backup id from our ACI (`lib/archive/.../EncryptedBackupWriter.kt:94-123`).

In libsignal terms (`rust/message-backup/src/key.rs:32-58`, `rust/account-keys/src/backup.rs:123`, **verified**):

```
backup_id   = BackupKey(ephemeral).derive_backup_id(our_aci)       // HKDF, info "20241024_SIGNAL_BACKUP_ID:" || aci
hmac||aes   = HKDF-SHA256(salt = none, ikm = ephemeral,
                          info = "20241007_SIGNAL_BACKUP_ENCRYPT_MESSAGE_BACKUP:" || backup_id)   // 64 bytes
```

So the import needs exactly two inputs besides the file: the 32-byte ephemeral key and our own ACI.

## File format

**Verified** in libsignal `rust/message-backup/src/frame.rs:93-118` and Desktop `index.preload.ts:843-901`:

```
IV (16 bytes) || AES-256-CBC(PKCS7, gzip(frames)) || HMAC-SHA256(hmac_key, IV || ciphertext) (32 bytes)
```

This is the *legacy* layout: no `SBACKUP\x01` magic number and no forward-secrecy metadata, which only paid remote backups carry (`frame/forward_secrecy.rs:19`; Android only writes the magic number when it has forward-secrecy metadata, `EncryptedBackupWriter.kt:132-133`).

Inside the gzip stream is a sequence of varint-length-delimited protobufs from `backup.proto`: first a `BackupInfo`, then `Frame`s, each holding one of `AccountData`, `Recipient`, `Chat`, `ChatItem`, `StickerPack`, `AdHocCall`, `NotificationProfile` or `ChatFolder`. Recipients and chats get file-local numeric ids; a `ChatItem` refers to its chat and its author by those ids. The export order puts each recipient and chat before the items that use them (**inferred** from the format's reference rules, which libsignal's validator enforces by failing on unknown references).

libsignal validates link-and-sync files with `Purpose::DeviceTransfer` (Android `app/.../backup/v2/ArchiveValidator.kt:45`, **verified**).

## The history window

The "about 45 days" in the PRD is the **media** limit, not the message limit. **Verified**:

- Android exports link-and-sync with `messageInclusionCutoffTime = 0` (`BackupRepository.kt:842`), and the cutoff only applies when it's positive (`backup/v2/database/MessageTableArchiveExtensions.kt:111-112`). Every text message the phone has goes into the archive.
- The phone's own copy says so: "Transfer your text messages and your last 45 days of media" (`app/src/main/res/values/strings.xml:1095`). Desktop's relink dialog: "If you have paid Signal Secure Backups, all of your media will be transferred. Otherwise, your last 45 days of media will be included." (`_locales/en/messages.json`, `icu:MaybeTransferModal__media`).
- One exception: disappearing messages that will expire before the export finishes are left out (`ChatItemArchiveExporter.kt:730-732`).

**Inferred**: the 45-day media limit comes from attachment pointers in the archive referencing the transit CDN tier, which Signal's servers keep for about 45 days. We don't download attachment bytes at import time anyway; attachment pointers are kept as metadata only.

So for us: all text history the phone holds, which comfortably satisfies acceptance 5 ("a message sent 30 days before linking"). The UI copy should say "your messages, and the last 45 days of photos and files" rather than "45 days of history".

**Unverified**: iOS as the sending phone. Signal-iOS wasn't read; it presumably follows the same server contract, since the receiving clients don't distinguish.

## What relay-history does

`relay_history::import_link_and_sync(archive, key, our_aci, store, progress)` takes the downloaded file, the key, and our ACI, verifies the HMAC, decrypts with libsignal's own `FramesReader`, parses frames with libsignal's generated `backup.proto` types, and writes each message into the `MessageStore`. The function's doc comment is the contract for L1.

Mapping into relay-core:

- A contact chat becomes `Recipient::Contact(their ACI)`; the self chat ("Note to Self") becomes `Recipient::Contact(our ACI)`.
- A group chat becomes `Recipient::Group(id)`, where `id` is the group identifier derived from the master key (`GroupSecretParams::derive_from_master_key(..).get_group_identifier()`), never the master key itself, which is a secret. Live messages from L1 must use the same derivation or imported and live messages for one group will land in two conversations.
- `MessageId` is the author's ACI plus `dateSent`, which is what reactions, edits and deletes reference.
- Messages whose author has no ACI (PNI-only or e164-only contacts, release notes) are skipped and counted.
- Incoming messages the phone had read are marked read, so a fresh link doesn't show years of history as unread.
- An edited message imports once, with its latest text, under the original's `dateSent`, since the archive's top-level `dateSent` is the edit's own (Signal-Desktop `ts/services/backups/import.preload.ts:1882-1908`, **verified**).
- Chat events (`ChatUpdateMessage`: joins, timer changes, and so on) are skipped, not stored as empty messages.
- Attachments are not imported yet, not even as metadata. Fetching one later needs its CDN key, decryption key and digest kept somewhere the agent can resolve, and `AttachmentRef::id` has no agreed meaning for that yet; that's a follow-up with L1.
- The archive is decrypted into memory before frames are written, because libsignal's `FramesReader` isn't `Send`. Text-only archives keep that to tens of MB.

## What L1 needs to provide

Implemented in `relay-signal` as `Provisioner::transfer_history` (`crates/relay-signal/src/transfer.rs`), except step 5, which setup drives:

1. `capabilities=backup5` on the provisioning URL (plus `nopni2`, which Android also checks and Desktop sends).
2. Read `ephemeral_backup_key()` right after linking; if it's `Some`, hold it in memory only, and don't start the receive loop's store writes for history until the import has run (Desktop holds message processing until the import finishes, `AccountManager.preload.ts:1337-1350`, **verified**).
3. `GET /v1/devices/transfer_archive` long-poll as above, mapping the two error strings to "relink" and "continue without history".
4. CDN download of `attachments/<key>` on the given CDN number to a file under the profile directory, resumable with `Range`.
5. Then call `import_link_and_sync`, delete the file, and drop the key.

`crates/relay-history/examples/link_and_sync_smoke.rs` is the manual harness for step 5 against a real phone.
