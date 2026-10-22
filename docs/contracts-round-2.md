# Contract round 2

The first tool batches hit gaps in the shared APIs and filed `contract:` issues instead of editing each other's crates. This round adds every agreed API in one PR, with `FakeSignal` and `MemoryStore` implementing all of it and tests named for the invariant each one pins, so the implementation lanes below can build in parallel. The real backends compile with `bail!("... not implemented yet (lane …)")` stubs until those lanes land.

Implementation lanes: **L1b** builds the presage side of every `SignalBackend` addition, **L2b** the SQLCipher side of every `MessageStore` addition, **L3f** and **L3g** the tools, **L4b** the agent wiring (supervisor process, scheduler tick, webhook delivery, settings).

Two rules held throughout. Every new `Outgoing` variant that reaches a person is classified in `counts()` in `crates/relay-tools/src/limits.rs`, whose match stays exhaustive, and `every_visible_variant_spends_the_message_budget` pins that. Nothing here can address more than one recipient or group per send (anti-goal 12).

## #44 Outgoing attachments

Decision: two steps, matching how presage works. `SignalBackend::upload(&SendableFile) -> AttachmentRef` encrypts and uploads a file; `Outgoing::Text` gains `attachments: Vec<AttachmentRef>` (serde default empty) carrying refs `upload` returned. Because an `AttachmentRef`'s id is the encoded pointer, the backend stays stateless, and the tool stores the sent message with the same refs, so `get_attachment` can fetch them back. `send` keeps returning `MessageId`.

The path allow-list lives in the type system. `relay_signal::SendRoots::new(roots)` canonicalizes the folders; `SendRoots::admit(path) -> SendableFile` canonicalizes the path (so `..` and symlinks are judged by where they land), requires a regular file inside a root, and is the only way to make a `SendableFile`. `ToolContext` gains `send_roots: Arc<SendRoots>`. The roots come from `Settings::send_roots` (#72) and default to none: no file is sendable until the user picks a folder under Advanced, because a prompt-injected client that can send any file can mail out an SSH key.

```rust
Outgoing::Text { body: String, quote: Option<MessageId>, attachments: Vec<AttachmentRef> }
async fn upload(&self, file: &SendableFile) -> anyhow::Result<AttachmentRef>;
pub struct SendRoots; impl SendRoots { fn new(..) -> Result<Self>; fn admit(&self, &Path) -> Result<SendableFile> }
```

`counts()`: Text, unchanged (true). `upload` is not metered: it reaches no one until a counted Text carries it. Voice-note and view-once flags are deferred until a tool needs them. Builds: L1b (`upload_attachments`, pointers into `DataMessage.attachments`), L3f (send_attachment, send_group_attachment, send_note_to_self's `attachments`).

## #45 Outgoing sticker

Decision: `Outgoing::Sticker { pack: StickerPackId, sticker_id: u32 }`. `StickerPackId` is a new relay-core newtype (16 bytes, 32 hex characters, `parse` and `Display` round-trip under proptest). The backend looks the pack key and sticker pointer up in presage's store and fails when the pack isn't installed; `FakeSignal::with_sticker_pack(pack, ids)` seeds installed packs and the fake refuses anything else.

`counts()`: true, a sticker is a message someone reads. Builds: L1b, L3f (send_sticker, send_group_sticker).

## #46 Pins

Decision: `Outgoing::Pin { target, duration_secs: Option<NonZeroU32> }` and `Outgoing::Unpin { target }`. The proto at `fae0679` has `pinDuration` as a oneof of `pinDurationSeconds` and `pinDurationForever`; `None` maps to forever. No store change.

`counts()`: both true, everyone in the chat sees the pin notice. Builds: L1b, L3g (pin_message, unpin_message).

## #47 Polls

Decision: `PollCreate { question, options, multi_select }`, `PollVote { poll, option_indexes, vote_count }`, `PollTerminate { poll }`. The vote counter is durable in the store, not the backend, so the backend stays stateless: `MessageStore::next_poll_vote(poll) -> u32` returns 1 for the first vote and a strictly higher number after, per poll. The tool calls it, then sends. A failed send leaves a gap in the count, which is harmless because receivers only keep the highest. `PollTerminate`'s wire form names only a timestamp, so it can only close this account's own poll; the fake refuses another author's poll and the tool should check first. The registry sweep already has an array arm, so the array inputs need nothing extra.

`counts()`: all three true; a vote loop is still a message loop. Builds: L1b, L2b (`next_poll_vote`), L3g (create_poll, vote_poll, terminate_poll).

## #48 Admin delete

Decision: `Outgoing::AdminDelete { target }`, group-only (the fake refuses a contact recipient); `Group` gains `admins: Vec<ServiceId>`, filled from presage's member roles in this PR; `Incoming::AdminDelete { group, target, from }`; and a separate store method that leaves `mark_deleted`'s author check exactly as it was:

```rust
/// Flags `target` deleted on a group admin's authority. Fails unless `target` is stored in `group`.
async fn mark_admin_deleted(&self, group: GroupId, target: MessageId) -> anyhow::Result<()>;
```

The store can't verify the deleter, so checking that the deleter is in `Group::admins` (from `SignalBackend::groups`, never from the caller's input) is the caller's job, both in the tool before sending and in capture before applying. Capture currently logs and skips `Incoming::AdminDelete`, because applying it needs the backend. Test: `admin_deletes_stay_inside_the_named_group_and_leave_the_author_check_alone`.

`counts()`: true. Builds: L1b (send and receive), L2b, L3g (admin_delete_message), L4b (capture applies it after the admin check).

## #49 discover

Decision, as proposed: `discover(&self, numbers: &[String]) -> Vec<(String, Option<ServiceId>)>`, answering every number in the order given, `None` when unregistered or undiscoverable. Numbers are E.164 strings the tool has already validated. `FakeSignal::with_registered(number, id)`.

Lookups pass the limiter unmetered (no person receives anything); Signal rate-limits contact discovery itself and the backend surfaces that. A lookup budget of our own is deferred, see the end. The tool should cap the array with `maxItems`. Builds: L1b (`discover_contacts_by_phone_number`, `cdsi`), L3f (get_profile, get_user_status numbers).

## #50 lookup_username

Decision, as proposed: `lookup_username(&self, username: &str) -> Option<ServiceId>`. Lookup only, so outside anti-goal 8. `FakeSignal::with_username`. Builds: L1b, L3f (get_user_status usernames).

## #51 Scheduled sends

Decision: jobs live in `MessageStore` (same encrypted database, no sibling trait), and they fire through the registry's normal write path, so read-only mode, the author rules and the limiter apply at fire time exactly as to a live send.

```rust
pub struct ScheduledSend { pub recipient: Recipient, pub body: String, pub send_at_ms: u64 }
pub enum JobState { Pending, Sending, Failed { error: String } }
pub struct ScheduledJob { pub id: JobId, pub send: ScheduledSend, pub state: JobState }
async fn schedule(&self, send: &ScheduledSend) -> anyhow::Result<JobId>;
async fn scheduled(&self) -> anyhow::Result<Vec<ScheduledJob>>;          // not yet sent, soonest first
async fn cancel_scheduled(&self, id: JobId) -> anyhow::Result<bool>;
async fn take_due(&self, now_ms: u64) -> anyhow::Result<Vec<ScheduledJob>>;  // claims: Pending -> Sending
async fn finish_scheduled(&self, id: JobId, result: Result<(), String>) -> anyhow::Result<()>;
```

One recipient per job, by type. The open questions, answered: a failed send is never retried; it stays as `Failed` with the error for list_scheduled_messages to show, because a limiter refusal must not queue a burst for later (PRD goal 13) and a send that errored may still have reached Signal. A job left in `Sending` by a crash is never claimed again for the same reason; the user sees it and can cancel. Jobs due while the agent was down fire late on the next tick, as upstream does, and the limiter turns any pile-up into `Failed` rather than a burst. Attachments on scheduled sends wait for a later round.

Firing, for L3g and L4b to share: one function in relay-tools, `run_due(registry, ctx, now_ms)`, that calls `take_due`, then for each job `registry.call(ctx, "send_message" | "send_group_message", input)`, then `finish_scheduled` with the outcome. The tick (every 30 s, beside capture) and run_scheduled_messages both call it, so they can't drift. Tests: `a_due_job_is_claimed_exactly_once`, `failed_and_interrupted_jobs_are_never_sent_again`, `cancelling_a_job_removes_it_once`. Builds: L2b, L3g (the four tools and `run_due`), L4b (the tick).

## #52 avatar

Decision, as proposed: `avatar(&self, of: Recipient) -> Option<Vec<u8>>`. The tool applies get_attachment's 5 MiB cap before encoding. `FakeSignal::with_avatar`. Builds: L1b, L3f (get_avatar).

## #53 Webhook config

Decision: the webhook URL is set only by the user, in Settings (the Advanced screen), never by an MCP tool. A webhook is a standing exfiltration channel: with full access, one prompt-injected client calling `set_webhook("https://attacker…")` would have every future incoming message posted there, silently and for good. So **set_webhook is deliberately not shipped**, for security, and the store holds no webhook.

`relay_tools::Settings` gains `webhook: Option<WebhookUrl>`. `relay_tools::WebhookUrl` validates at construction and on deserialize: `https` anywhere, plain `http` only to loopback (`localhost`, 127.0.0.0/8, `::1`), the same no-TLS-no-leaving-the-machine rule as the HTTP endpoint, so a hand-edited settings file can't sneak in a plain-http remote. `ToolContext` gains `webhook: Option<WebhookUrl>`, an owned copy of the setting, so get_webhook (a read) can report it and nothing a tool does reaches the agent's capture loop. Tests: `a_webhook_leaves_the_machine_only_over_https`, `deserializing_a_webhook_validates_it`, `no_registered_tool_can_set_the_webhook`.

Delivery, for L4b: capture takes the URL from the loaded settings and posts `{"event": "message", "profile": "<name>", "message": <Message>}` only after `insert` returned true, so a webhook never sees a message the store refused. The POST is spawned, best effort, with a 5 s timeout and a log line on failure, and never stalls capture. A changed setting applies when the agent next loads settings (restart, or a settings reload if L4b adds one). Builds: L3g (get_webhook only), L4b (delivery in capture), L8 (the field on the Advanced screen).

## #54 identities

Decision: `identities(&self) -> Vec<Identity>` with `Identity { service_id, safety_number, trust: Trust, changed: bool }` and `Trust { Verified, Unverified, Untrusted }`. `changed` is true when the key replaced an earlier one and hasn't been verified since. The proposed `added_at_ms` is dropped: the protocol store doesn't keep it and no tool needs it. `FakeSignal::with_identities`. trust_identity (audit row 53) had no contract issue and is not in this round. Builds: L1b, L3f (list_identities).

## #55 Delete one conversation

Decision, as proposed: `delete_conversation(&self, conversation: Recipient) -> u64`, removing the conversation's messages, their read state and its title atomically, and returning the count. The tool takes `recipient` plus `confirm: true` and is a write. Test: `deleting_a_conversation_leaves_the_others_untouched`. Builds: L2b, L3g (delete_local_messages).

## #56 devices

Decision, as proposed: `devices(&self) -> Vec<Device>` with `Device { id: u32, name, created_at_ms, last_seen_at_ms }`, id 1 the primary. `FakeSignal::with_devices`. Builds: L1b, L3f (list_devices).

## #65 Paged attachment query

Decision, as proposed: `attachments(limit, offset) -> Vec<StoredAttachment>` (newest message first, messages deleted for everyone left out) and `attachment(id) -> Option<StoredAttachment>`, with `StoredAttachment { attachment, message, conversation }`. Test: `stored_attachments_page_newest_first_without_deleted_messages`. Builds: L2b (an attachments index), L3f (directory.rs drops its full scan).

## #68 Voice code in setup (and duplicate #71)

Decision: `SetupEvent::CodeByVoice`, accepted only in `EnterCode`, goes to `RequestingCode` with `Command::RequestCode { number, captcha, voice: true }`; number entry and captcha resubmits send `voice: false`. The driver passes `voice` to `Provisioner::request_code`.

We don't lean on Signal's own pacing (PRD goal 13). The first call request in a registration attempt finishes that attempt, like a captcha resubmit, so it isn't held behind the five-minute provisioning backoff. A second `CodeByVoice` in the same attempt is refused locally, before Signal hears of it, with a plain message ("A call for this code was already requested. Wait for it, or go back and enter your number again to start over."), and starting over from the number is a fresh attempt that meets the normal backoff. The driver keeps the per-attempt flag, reset when a fresh code request starts. Not carried through a captcha: if Signal demands a captcha after a voice request, the resubmit goes by SMS, which avoids adding a field to every state the front ends render. The window's input filter lets the page send `CodeByVoice`. Tests: `asking_for_a_call_re_requests_the_code_by_voice`, `a_voice_call_after_the_sms_is_not_held_back_by_the_backoff`, `a_second_voice_call_request_in_one_attempt_is_refused_without_contacting_signal`. Builds: L8 (a "Call me instead" button on the code screen), L9 ("Didn't get it? Type v for a phone call.").

## #70 Supervisor for all profiles

Decision: one login item on every OS runs `signal-agent agent --all-profiles`, which runs each profile's agent as its own child process (`signal-agent agent --profile <name>`), so one crash can't take the others down. The policy is `relay_agent::Supervisor`, free of processes and clocks:

```rust
pub enum SupervisorAction { Start(ProfileName), Stop(ProfileName) }
impl Supervisor {
    pub fn reconcile(&mut self, ready: &[ProfileName], now_ms: u64) -> Vec<SupervisorAction>;
    pub fn exited(&mut self, profile: &ProfileName, now_ms: u64);
}
```

Only ready profiles start: those in `relay_setup::Profiles` whose access mode setup has saved (`ProfilePaths::load_access` succeeds). A profile still in setup has its Signal store open in the setup process, and two processes on one store corrupt its sessions. Restarts back off 5 s, doubling to a 5 minute cap, and reset after 10 minutes of healthy running, because each restart reconnects to Signal. A child that exits because another agent already holds the profile's endpoint (the stdio shim spawned one) is just an exit; the bind is the single-instance check and the backoff keeps the retries quiet.

L10's two questions: the supervisor rescans `profiles.json` on a 10 s timer and after every child exit, with no signal from setup, so setup doesn't need to know the supervisor exists and a crashed setup can't leave it stale. Linux and Windows switch to the same model: one systemd user unit and one logon task running the supervisor, so all three OSes have one code path and uninstall removes one item. `login::register(profile)` keeps its signature and, under this model, ensures the supervisor item exists and is running. Tests: `each_ready_profile_runs_once_and_a_removed_one_stops`, `a_crash_loop_backs_off_up_to_five_minutes`, `a_healthy_run_resets_the_backoff`. Builds: L4b (the `--all-profiles` loop, child processes, login items on all three OSes, migrating existing per-profile units), L6 (the bundled launchd plist runs `--all-profiles`).

## #72 Settings-driven limits

Decision: `ProfilePaths` gains `settings` (`settings.json`), `limits` (`limits.json`) and `provisioning` (`provisioning.json`), so setup, the agent and uninstall agree on them. `relay_tools::Settings { limits: Limits, send_roots: Vec<PathBuf>, webhook: Option<WebhookUrl> }` is the Advanced settings file's shape, `serde(default, deny_unknown_fields)`, loaded by `Settings::load(path)`: a missing file gives the defaults, a misspelled field or zero rate is an error. Tests: `missing_settings_mean_default_limits_and_no_send_folders`, `settings_fill_gaps_with_defaults_and_refuse_typos_and_zero_rates`.

Wiring, for L4b: the agent loads `Settings::load(&paths.settings)` at start and builds `Limiter::new(settings.limits, clock).persisted(paths.limits.clone())` and `SendRoots::new(settings.send_roots)` into the `ToolContext`, with `settings.webhook` copied into it and handed to capture; a damaged settings file or a missing send folder is logged and the agent falls back to default limits and no send roots, because stopping the agent would also stop capture. The setup driver's provisioning gate switches to `paths.provisioning` and the loaded limits. Builds: L4b (agent and driver wiring), L8 (Advanced screen writes the file).

## Deferred or rejected

- A lookup budget for discover, lookup_username and avatar. They reach Signal but no person; Signal's own limits apply and are surfaced. Worth an L14 follow-up if a client is seen looping on lookups.
- Attachments on scheduled sends, voice-note and view-once flags on outgoing attachments: no tool needs them yet.
- trust_identity, add_device and remove_device signatures: no contract issue asked for them; next round.
- Rejected for security: a set_webhook tool (#53). The webhook is user-set in Settings only.
- Rejected: an `Outgoing::Attachments` variant beside `Text` (two near-identical variants), a backend-held poll vote counter (would need state in presage's store, which we don't own), and passing the admin list into `mark_admin_deleted` (the store would only be trusting whatever list it was handed).
