# YouTube: your own Google client

replaycut can upload a share as a video of its own to your YouTube channel
(unlisted by default) and, with the "Vertical 9:16" option in the share
row, as a Short. YouTube's API gives every Google project 10 000 quota
units a day and charges 1 600 per upload: about six uploads a day. A client
built into replaycut would share that budget between everyone who uses it,
so replaycut uses a client from **your own** Google project. Creating one
takes about five minutes and costs nothing.

## 1. Create the project and enable the API

1. Open <https://console.cloud.google.com/> with the Google account that
   owns the channel and create a project (any name, for example
   `replaycut`).
2. **APIs & Services › Library**: search for *YouTube Data API v3* and click
   **Enable**.

Newer consoles then offer a guided **Create credentials** wizard on the
API's page. It asks the same things as sections 2 and 3 in one go: choose
**User data** (that creates an OAuth client), fill in the consent screen,
leave the scopes empty or add `.../auth/youtube`, and pick the client type
in the last step. Publishing the app (section 2, step 3) is still a separate
click afterwards.

## 2. Set up the consent screen

1. **APIs & Services › OAuth consent screen** (Google calls it "Google Auth
   Platform" in newer consoles): user type **External**, app name
   `replaycut`, your e-mail as support and developer contact. Nothing else is
   required.
2. Scopes: none need to be added here; replaycut asks for
   `https://www.googleapis.com/auth/youtube` when you connect.
3. **Publishing status**: click **Publish app** so the app is *In
   production*. Google then shows an "unverified app" warning once when you
   connect (click *Advanced › Go to replaycut*); that is expected and fine
   for your own use. Do not leave the app in *Testing*: in that state Google
   expires the connection after seven days and replaycut would ask you to
   connect again every week.

## 3. Create the client

1. **APIs & Services › Credentials › Create credentials › OAuth client ID**.
2. Application type, one of two:
   - **TVs and Limited Input devices** (recommended): replaycut shows a
     short code, you type it at <https://www.google.com/device> from any
     device, the phone included.
   - **Desktop app**: replaycut opens Google's login in the browser on the
     PC that runs it, and Google sends the browser back to
     `http://127.0.0.1:<port>/oauth/youtube/callback` (a Desktop client
     accepts any loopback port). Only works from that PC's browser.
3. Name it `replaycut` and create it. Google shows a **Client ID** (ends in
   `.apps.googleusercontent.com`) and a **Client secret**. Copy both.

Both types ask for the same permission, `.../auth/youtube` ("manage your
YouTube account"): uploading needs less, but reading the channel name and
deleting a video from replaycut's delete dialog need this one.

## 4. Connect replaycut

1. Settings › Integrations › **YouTube**: choose the client type you
   created, paste client ID and client secret, click **Save**. They go to
   the Windows Credential Manager (`replaycut/youtube-client`); nothing is
   written to a file.
2. Switch the card on and click **Connect YouTube**. With a TV client: open
   the link shown, enter the code, pick the channel, allow the access. With
   a Desktop client: a tab with Google's login opens; sign in, pick the
   channel, allow, and the tab says it is done. Either way the card says
   *Connected as <channel>* when the tokens arrived (the refresh token is
   the credential `replaycut/youtube`).
3. Choose the privacy (unlisted, private, public) and edit the description
   template if you like: `{title}`, `{clip}` and `{date}` are filled in.
4. Optionally make YouTube the quick-share target; otherwise it sits in the
   Share button's menu and in "Publish to YouTube" on finished shares.

## Shorts

Choose **Vertical 9:16 (Short)** in the share row. The player shows the
9:16 window; the slider below moves it across the picture (the action is
rarely in the middle). The share is encoded at 1080x1920 and uploaded with
`#Shorts` in the title; YouTube decides by format and length that it is a
Short. The option works with every target and with "file only" too: the
file is then ready for TikTok, Instagram Reels or WhatsApp.

## Quota and limits

- 1 600 units per upload, 10 000 units a day per project, reset at midnight
  Pacific time. Deleting a video from the delete dialog costs 50.
- The connection check in the diagnostics costs 1 unit.
- Unverified apps may have at most 100 users; you are the only one.
- If uploads fail with `quotaExceeded`, wait for the reset or request a
  higher quota in the Google console (APIs & Services › YouTube Data API v3
  › Quotas).
