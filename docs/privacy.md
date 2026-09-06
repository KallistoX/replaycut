# Privacy

replaycut is a self-hosted program. It runs on your own PC, and there is
no replaycut server, account or telemetry. This page exists because the
integrations with YouTube, X and OneDrive ask for a privacy policy of the
application that requests access.

## What replaycut stores, and where

- Your recordings, the previews and the shared clips stay in the folders
  on your PC that you configured.
- Settings are a JSON file in your user profile.
- Credentials and OAuth refresh tokens for the integrations you connect
  (Nextcloud, OneDrive, S3, WebDAV, YouTube, X, Telegram, Discord, webhook)
  are stored in the Windows Credential Manager on your PC. They are never
  written to a file and never sent anywhere but to the service they belong
  to.

## What leaves your PC

Only what you trigger: a share uploads the clip you cut to the storage you
chose and posts the link to the notify integrations you switched on. The
optional update check asks GitHub once a day for the newest release and
sends nothing about you.

## Google user data

When you connect a YouTube channel, replaycut asks for the scope
`https://www.googleapis.com/auth/youtube`. It uses that access only to
read the channel's name for the settings card, to upload the videos you
share, and to delete a video when you delete the clip in replaycut with
"also remove from storage". The refresh token stays in your Credential
Manager; "Disconnect" removes it. replaycut does not share Google user
data with anyone, does not use it for advertising, and does not let
humans read it. Its use of information received from Google APIs adheres
to the [Google API Services User Data Policy](https://developers.google.com/terms/api-services-user-data-policy),
including the Limited Use requirements.

## Contact

Questions go to the issue tracker of this repository.
