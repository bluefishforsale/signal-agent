# Code signing setup

What the repo owner has to do by hand so CI can ship installers that open with no warnings. Apple and Microsoft only issue signing identities to a verified person, so none of this can be automated. Everything after the hand-off at the end is CI's job.

Both identities are individual, in the name Terrac Skiens (PRD goal 1). Switching to an organization later only means replacing the secrets below; no code changes.

## Apple: Developer ID and notarization

You end up with a Developer ID Application certificate (which signs the app) and an App Store Connect API key (which lets CI submit the app to Apple for notarization).

1. **Enroll in the Apple Developer Program as an individual.** Go to [developer.apple.com/programs/enroll](https://developer.apple.com/programs/enroll/) and sign in with the Apple ID you want to own this. It needs two-factor authentication turned on, and your legal name on the Apple ID has to match your ID, because it appears on every signature. The fee is $99 a year. Approval is usually quick for individuals, but Apple doesn't promise a time.
2. **Create a certificate signing request on your Mac.** Open Keychain Access, then choose Keychain Access > Certificate Assistant > Request a Certificate From a Certificate Authority. Enter your Apple ID email, choose "Saved to disk", and save the `.certSigningRequest` file. ([Apple's steps](https://developer.apple.com/help/account/certificates/create-a-certificate-signing-request))
3. **Create the Developer ID Application certificate.** In your developer account, go to Certificates, IDs & Profiles > Certificates > + and choose **Developer ID Application**. Upload the request from step 2, download the `.cer`, and double-click it to install it into your login keychain. Only the account holder can create this certificate type, which is you.
4. **Export it for CI.** In Keychain Access, open the login keychain's My Certificates tab and expand the "Developer ID Application: Terrac Skiens (TEAMID)" entry. Right-click the private key under it, choose Export, save as `.p12`, and set a strong password.
5. **Note the identity string and team ID.** Running `security find-identity -v -p codesigning` prints the exact identity string, `Developer ID Application: Terrac Skiens (XXXXXXXXXX)`. The ten characters in parentheses are your Team ID, which is also on the Membership page of your account.
6. **Create the notarization API key.** In [App Store Connect](https://appstoreconnect.apple.com/), go to Users and Access > Integrations > App Store Connect API, click +, name it `signal-agent-notarize`, and give it **Developer** access. Copy the **Issuer ID** shown above the keys table and the key's **Key ID**. Then download the `.p8` private key file. Apple lets you download it exactly once, and the download link only appears after a page reload, so save it somewhere safe immediately.

What you hand over from Apple:

| Secret | What it is |
|---|---|
| `APPLE_CERTIFICATE` | The `.p12` from step 4, base64-encoded: `openssl base64 -A -in cert.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | The password you set on the `.p12` |
| `APPLE_SIGNING_IDENTITY` | The full identity string from step 5 |
| `APPLE_TEAM_ID` | The Team ID from step 5 |
| `APPLE_API_ISSUER` | The Issuer ID from step 6 |
| `APPLE_API_KEY` | The Key ID from step 6 |
| `APPLE_API_PRIVATE_KEY` | The `.p8` from step 6, base64-encoded; CI writes it to a file and points `APPLE_API_KEY_PATH` at it |

## Microsoft: Azure Artifact Signing

You end up with a verified identity and a certificate profile that CI can sign with, through GitHub's OIDC login, so no long-lived Azure password is stored in GitHub.

Eligibility, from [Microsoft's quickstart](https://learn.microsoft.com/en-us/azure/artifact-signing/quickstart): "Individual developers must be located in the United States or Canada." Individual identity details come from your Azure billing account, which must have an Account Type of **Individual**, with a legal name and sold-to address that match your government ID. Fix those in the billing account before starting, because the name ends up on the certificate.

1. **Get an Azure subscription** on a billing account of type Individual (pay-as-you-go is fine). The Basic tier includes 5,000 signatures a month and one of each certificate profile type, which is plenty for this project. The [pricing page](https://azure.microsoft.com/en-us/pricing/details/artifact-signing/) shows the current monthly price.
2. **Register the resource provider.** In Subscriptions > your subscription > Resource providers, find `Microsoft.CodeSigning` and choose Register.
3. **Create the Artifact Signing account.** Search the portal for "Artifact Signing Accounts", then Create. Make a new resource group (e.g. `signal-agent-signing`), pick a name that's 3 to 24 letters and numbers and globally unique (e.g. `signalagentsign`), choose a supported region such as **East US**, and choose the **Basic** tier. Write down the region's endpoint; for East US it's `https://eus.codesigning.azure.net`.
4. **Give yourself the verifier role.** On the new account, go to Access control (IAM) > Add role assignment and assign yourself **Artifact Signing Identity Verifier**. Without it, the New identity button stays greyed out.
5. **Validate your identity.** Open the account's Identity validations page, switch the dropdown to **Individual**, then choose New identity > **Public** and select your billing account. The form fills itself from the billing account. Check the certificate subject preview, then Create. When the status changes to Action Required, click your name and follow "Please complete your verification here". You'll verify with AU10TIX using your phone's camera and a government photo ID, and add the resulting Verified ID to the Microsoft Authenticator app. The portal shows Completed a few minutes after you share the credential.
6. **Create the certificate profile.** On the account, go to Certificate profiles > Create > **Public Trust**. Name it (5 to 100 letters and numbers, e.g. `signalagentpublic`) and select your completed identity validation under Verified CN and O.
7. **Create the identity CI signs as.** In Microsoft Entra ID, go to App registrations > New registration and name it `signal-agent-ci`. On that app, open Certificates & secrets > Federated credentials > Add credential > GitHub Actions deploying Azure resources. Set the organization to `bluefishforsale`, the repository to `signal-agent`, the entity type to **Environment**, and the environment to `release`. This means only the release job can sign, not every push.
8. **Let that identity sign.** On the Artifact Signing account, open Access control (IAM) > Add role assignment > **Artifact Signing Certificate Profile Signer**, and assign it to the `signal-agent-ci` app registration. You'll have to search for it, because only users are listed by default.

What you hand over from Microsoft:

| Name | Kind | What it is |
|---|---|---|
| `AZURE_TENANT_ID` | secret | Entra ID > Overview > Tenant ID |
| `AZURE_CLIENT_ID` | secret | The `signal-agent-ci` app registration's Application (client) ID |
| `AZURE_SUBSCRIPTION_ID` | secret | The subscription from step 1 |
| `AZURE_SIGNING_ENDPOINT` | variable | Region endpoint from step 3 |
| `AZURE_SIGNING_ACCOUNT` | variable | Account name from step 3 |
| `AZURE_CERT_PROFILE` | variable | Profile name from step 6 |

Two constraints the pipeline is built around. The signing action runs only on Windows x64 runners and doesn't support Windows ARM runners, so the ARM64 build is compiled on an ARM runner and signed on an x64 runner. And a brand-new publisher can still see a SmartScreen warning until Microsoft has seen enough clean installs, which is acceptable under PRD acceptance criterion 3.

## Tauri updater key

The in-app updater installs a release only if its payload carries a signature from this key, on top of the Apple and Microsoft signatures (PRD goal 10). The private half signs releases in CI; the public half ships inside the app. GitHub secrets can't be read back, so keep a copy of the private key in the usual secret store too: if it's lost, every installed copy can only be updated by downloading a new installer by hand.

1. **Generate the key pair** on your Mac, with a strong password when asked:
   `npx --yes @tauri-apps/cli@2.12.4 signer generate -w ~/.tauri/signal-agent-updater.key`
   (or `cargo tauri signer generate -w ...` if you have the Tauri CLI). It writes the private key to that path and the public key beside it as `signal-agent-updater.key.pub`.
2. **Commit the public key.** Replace the `UNSET: ...` value of `plugins.updater.pubkey` in `app/src-tauri/tauri.conf.json` with the single line in `signal-agent-updater.key.pub`. Until that lands, the release preflight refuses to run, because a release signed with a key the app doesn't trust can never be installed as an update.
3. **Hand over the private half** as the two secrets below, then move the key file into the secret store and delete it from `~/.tauri`.

| Secret | What it is |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | The contents of `signal-agent-updater.key` |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | The password from step 1 |

Every release then signs the macOS `.app.tar.gz` and both Windows installers with it, checks each signature against the committed public key with `minisign`, and writes `latest.json`, the feed the app reads. A mismatch fails the release before anything is attached.

## Done for you

You don't need to do these; the agent handles them once the values above exist:

- `KEYCHAIN_PASSWORD`, a random value for the temporary keychain CI creates on the macOS runner.
- The `release` GitHub environment, limited to `v*` tags, holding all of the above as environment secrets, so ordinary pushes and pull requests never see them.

## Hand-off

Put the values in the usual secret store, not in chat, and say which entries hold them. The agent sets them with `gh secret set --env release` and `gh variable set`, then confirms by pushing a `v0.0.0-signing-test` tag, which builds, signs and notarizes a hello-world app into a draft release, so nothing public changes. Until all of them exist, the release job fails on purpose (PRD acceptance 11).
