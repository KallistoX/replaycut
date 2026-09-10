# Privacy

replaycut is a self-hosted program. It runs on your own PC, and there is
no replaycut server, account or telemetry. This page exists because the
integrations with YouTube and OneDrive ask for a privacy policy of the
application that requests access.

The published version of this text lives at
<https://replaycut.de/privacy/>, and that is the URL those integrations are
registered with; it says the same thing plus what the website itself does.
Change both together.

## What replaycut stores, and where

- Your recordings, the previews and the shared clips stay in the folders
  on your PC that you configured.
- Settings are a JSON file in your user profile.
- Credentials and OAuth refresh tokens for the integrations you connect
  (Nextcloud, OneDrive, S3, WebDAV, YouTube, Telegram, Discord, webhook)
  are stored in your PC's credential store (the Windows Credential Manager,
  or the keyring behind the Secret Service on Linux). They are never
  written to a file and never sent anywhere but to the service they belong
  to.

## What leaves your PC

Only what you trigger: a share uploads the clip you cut to the storage you
chose and posts the link to the notify integrations you switched on. The
optional update check asks GitHub once a day for the newest release and
sends nothing about you.

## Google user data

This section is about the YouTube integration alone. It applies only once
you switch that integration on and connect a channel; until then replaycut
touches no Google user data at all. When you click **Connect YouTube**,
replaycut asks your Google account for one OAuth scope,
`https://www.googleapis.com/auth/youtube`, and for nothing else.

### What Google user data replaycut accesses

- The **title of your YouTube channel**, read with `channels.list`
  (`mine=true`) when you connect, so the settings page can show which
  channel a share would go to.
- The **video id and link of the videos replaycut itself uploads** for you,
  returned by `videos.insert`.
- An **OAuth refresh token and access token** for your Google account,
  issued when you grant the access.

Nothing else: replaycut does not read the other videos on your channel, and
no comments, subscribers, playlists, analytics, e-mail address or Google
profile data.

### How replaycut uses Google user data

- The channel title is shown to you, on your own screen, on the settings
  page ("Connected as <channel>").
- `videos.insert` uploads the clip you chose to share to that channel, with
  the privacy (unlisted, private or public), title and description you set.
- `videos.delete` removes such a video again - only when you delete the clip
  in replaycut and tick "also remove the uploaded copies".
- The tokens authenticate exactly those calls against Google's API.

Every one of these calls happens because you clicked: replaycut runs nothing
on a schedule and nothing in the background. replaycut does not use Google
user data for advertising or profiling, does not sell it, does not use it
for creditworthiness or lending decisions, and does not use it to develop,
improve or train generalized artificial intelligence or machine learning
models.

### Who replaycut shares Google user data with

Nobody. There is no replaycut server and no replaycut account, so there is
nowhere for the data to go: the program on your PC talks to Google directly.
Google user data is not transferred, sold or disclosed to us or to any third
party, and no human being other than you ever sees it. If you switch on a
notify integration such as Discord, what it posts is the link of the share
you just made - the link you would otherwise paste yourself - and nothing
else. replaycut's use of information received from Google APIs adheres to
the [Google API Services User Data Policy](https://developers.google.com/terms/api-services-user-data-policy),
including the Limited Use requirements.

### How replaycut protects Google user data

- The refresh token is stored in your operating system's credential store -
  the Windows Credential Manager, or the keyring behind the Secret Service
  on Linux - as the entry `replaycut/youtube`. It is never written to a
  settings file, a log file or the database.
- The access token exists only in the memory of the running program and
  expires after about an hour.
- Every request to Google goes over HTTPS with TLS.
- replaycut's own interface listens on your PC and, out of the box, accepts
  connections from that PC only. Opening it to your network is a switch you
  have to throw, and it turns on a device login with a password.

### How long replaycut keeps Google user data, and how to delete it

- The **refresh token** stays in the credential store until it is removed,
  so that you do not have to sign in again every few days. **"Disconnect" on
  the YouTube card deletes it immediately**, and so does
  `replaycut uninstall --purge`.
- The **channel title** is kept only as the user name of that same
  credential entry and goes with it.
- The **video ids and links** of your shares live in replaycut's database on
  your PC as long as the clip's history entry does; deleting the clip in
  replaycut deletes them with it.
- You can revoke replaycut's access at any time and independently of
  replaycut, at <https://myaccount.google.com/permissions>. The uploaded
  video is yours: it stays on your channel until you delete it, in replaycut
  or in YouTube Studio.

There is no copy anywhere else - no backup of ours, no analytics, no server.

## Contact

Questions go to the issue tracker of this repository.
