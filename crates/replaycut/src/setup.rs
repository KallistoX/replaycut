//! `replaycut setup`: interactive configuration of the integrations on the
//! console. Secrets go to the Credential Manager, everything else to
//! settings.json. The browser wizard replaces this later; the command stays
//! for headless use.

use std::io::{self, BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result};

use crate::credentials;
use crate::integrations::{is_webhook_url, Discord, Nextcloud};
use crate::settings::Settings;

pub async fn run(settings_path: &Path, settings: &mut Settings) -> Result<()> {
    println!(
        "replaycut setup - settings file: {}",
        settings_path.display()
    );
    println!("Press Enter to keep the value shown in brackets.\n");

    let display = ask(
        "Display name (prefix of the Discord post)",
        &settings.display_name,
    )?;
    settings.display_name = display;

    // Nextcloud
    let nc = &mut settings.integrations.nextcloud;
    if ask_yes_no("Upload shared clips to Nextcloud?", nc.enabled)? {
        nc.url = ask("Nextcloud URL", &nc.url)?
            .trim_end_matches('/')
            .to_string();
        nc.folder = ask("Folder for clips", &nc.folder)?
            .trim_matches('/')
            .to_string();
        let existing = credentials::read(credentials::NEXTCLOUD)?;
        let default_user = existing
            .as_ref()
            .map(|c| c.user.clone())
            .unwrap_or_default();
        let user = ask("Nextcloud user name", &default_user)?;
        println!(
            "  Create an app password in Nextcloud: Settings -> Security -> Devices & sessions."
        );
        let hint = if existing.is_some() && user == default_user {
            " (Enter keeps the stored one)"
        } else {
            ""
        };
        let password = ask_secret(&format!("App password{hint}"))?;
        let password = match (password.is_empty(), &existing) {
            (true, Some(c)) if c.user == user => c.secret.clone(),
            (true, _) => anyhow::bail!("an app password is required"),
            (false, _) => password,
        };
        print!("  Checking login ... ");
        io::stdout().flush()?;
        let client = Nextcloud::with_values(
            &nc.url,
            &nc.folder,
            nc.expire_days,
            user.clone(),
            password.clone(),
        )?;
        match client.test().await {
            Ok(name) => println!("ok ({name})"),
            Err(e) => {
                println!("failed");
                anyhow::bail!("Nextcloud: {e}");
            }
        }
        credentials::write(credentials::NEXTCLOUD, &user, &password)?;
        nc.enabled = true;
        println!("  Credentials stored as {}.", credentials::NEXTCLOUD);
    } else {
        nc.enabled = false;
        if credentials::read(credentials::NEXTCLOUD)?.is_some()
            && ask_yes_no("Remove the stored Nextcloud credentials?", false)?
        {
            credentials::delete(credentials::NEXTCLOUD)?;
        }
    }

    // Discord
    let dc = &mut settings.integrations.discord;
    if ask_yes_no(
        "Post shared clips to a Discord channel (webhook)?",
        dc.enabled,
    )? {
        let existing = credentials::read(credentials::DISCORD_WEBHOOK)?;
        let hint = if existing.is_some() {
            " (Enter keeps the stored one)"
        } else {
            ""
        };
        let url = ask_secret(&format!("Webhook URL{hint}"))?;
        let url = match (url.is_empty(), &existing) {
            (true, Some(c)) => c.secret.clone(),
            (true, None) => anyhow::bail!("a webhook URL is required"),
            (false, _) => url,
        };
        anyhow::ensure!(
            is_webhook_url(&url),
            "this does not look like a Discord webhook URL"
        );
        credentials::write(credentials::DISCORD_WEBHOOK, "webhook", &url)?;
        dc.enabled = true;
        println!("  Webhook stored as {}.", credentials::DISCORD_WEBHOOK);
        if ask_yes_no("Send a test message now?", false)? {
            let d = Discord::new(url, settings.display_name.clone())?;
            let status = d
                .post(&format!(
                    "**{}** replaycut setup: test message",
                    settings.display_name
                ))
                .await?;
            println!("  {status}");
        }
    } else {
        dc.enabled = false;
        if credentials::read(credentials::DISCORD_WEBHOOK)?.is_some()
            && ask_yes_no("Remove the stored webhook?", false)?
        {
            credentials::delete(credentials::DISCORD_WEBHOOK)?;
        }
    }

    if let Some(dir) = settings_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        settings_path,
        serde_json::to_string_pretty(settings)? + "\n",
    )
    .with_context(|| format!("cannot write {}", settings_path.display()))?;
    println!(
        "\nSaved {}. Restart the service to apply.",
        settings_path.display()
    );
    Ok(())
}

/// Check the enabled integrations without changing anything.
pub async fn test(settings: &Settings) -> Result<()> {
    let nc = &settings.integrations.nextcloud;
    if nc.enabled {
        match credentials::read(credentials::NEXTCLOUD)? {
            Some(c) => {
                let client = Nextcloud::new(settings, c.user.clone(), c.secret)?;
                match client.test().await {
                    Ok(name) => println!("Nextcloud: ok ({} as {name})", c.user),
                    Err(e) => println!("Nextcloud: {e}"),
                }
            }
            None => {
                println!("Nextcloud: enabled but no credentials stored - run `replaycut setup`")
            }
        }
    } else {
        println!("Nextcloud: disabled");
    }
    if settings.integrations.discord.enabled {
        match credentials::read(credentials::DISCORD_WEBHOOK)? {
            Some(c) => println!(
                "Discord: webhook stored ({})",
                if is_webhook_url(&c.secret) {
                    "looks valid"
                } else {
                    "does not look like a webhook URL"
                }
            ),
            None => println!("Discord: enabled but no webhook stored - run `replaycut setup`"),
        }
    } else {
        println!("Discord: disabled");
    }
    Ok(())
}

pub(crate) fn ask(prompt: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{prompt}: ");
    } else {
        print!("{prompt} [{default}]: ");
    }
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let line = line.trim();
    Ok(if line.is_empty() {
        default.to_string()
    } else {
        line.to_string()
    })
}

pub(crate) fn ask_yes_no(prompt: &str, default: bool) -> Result<bool> {
    let answer = ask(&format!("{prompt} (y/n)"), if default { "y" } else { "n" })?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "j" | "ja"
    ))
}

fn ask_secret(prompt: &str) -> Result<String> {
    Ok(rpassword::prompt_password(format!("{prompt}: "))?
        .trim()
        .to_string())
}
