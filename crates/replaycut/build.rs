//! Embeds the icons and the application manifest into the Windows executable.
//! Icon 1 is the application icon (also used by the tray in the normal
//! state), 2 and 3 are the tray states "job running" and "last job failed".

fn main() {
    println!("cargo:rerun-if-changed=assets");
    // The built-in OAuth clients are compiled in with `option_env!`, so a
    // cached build would otherwise keep the values of the run before.
    for var in [
        "REPLAYCUT_YOUTUBE_TV_CLIENT_ID",
        "REPLAYCUT_YOUTUBE_TV_CLIENT_SECRET",
        "REPLAYCUT_YOUTUBE_DESKTOP_CLIENT_ID",
        "REPLAYCUT_YOUTUBE_DESKTOP_CLIENT_SECRET",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon_with_id("assets/replaycut.ico", "1")
            .set_icon_with_id("assets/tray-busy.ico", "2")
            .set_icon_with_id("assets/tray-error.ico", "3")
            .set_manifest_file("assets/replaycut.manifest");
        res.compile().expect("cannot embed the Windows resources");
    }
}
