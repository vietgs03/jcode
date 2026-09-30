//! `jcode login --provider kiro`: AWS Builder ID / IAM Identity Center device
//! flow, or import of an existing Kiro IDE login.

use super::*;
use crate::auth::kiro::{KiroAuthMethod, KiroLoginTarget};

enum KiroLoginMethod {
    Device(KiroLoginTarget),
    ImportKiroIde,
}

pub(super) async fn login_kiro_flow(no_browser: bool) -> Result<()> {
    eprintln!("Starting Kiro login...");
    match choose_login_method()? {
        KiroLoginMethod::Device(target) => login_with_device_flow(target, no_browser).await,
        KiroLoginMethod::ImportKiroIde => import_kiro_ide_login(),
    }
}

fn choose_login_method() -> Result<KiroLoginMethod> {
    // An explicit IAM Identity Center start URL (scripted setups) skips the menu.
    if std::env::var_os(auth::kiro::START_URL_ENV).is_some() {
        return Ok(KiroLoginMethod::Device(KiroLoginTarget::from_env()?));
    }
    if !io::stdin().is_terminal() {
        return Ok(KiroLoginMethod::Device(KiroLoginTarget::builder_id()));
    }

    let ide_login = match auth::kiro::kiro_ide_token_path() {
        Ok(path) if path.exists() => Some(path),
        _ => None,
    };
    eprintln!("Kiro login. Choose an authentication method:");
    eprintln!("  [1] AWS Builder ID (device code, default)");
    eprintln!("  [2] AWS IAM Identity Center (organization start URL)");
    if let Some(path) = &ide_login {
        eprintln!(
            "  [3] Import the existing Kiro IDE login ({})",
            path.display()
        );
    }

    loop {
        let choice = read_line_trimmed("Choice [1]: ")?;
        match choice.as_str() {
            "" | "1" => return Ok(KiroLoginMethod::Device(KiroLoginTarget::builder_id())),
            "2" => return Ok(KiroLoginMethod::Device(prompt_identity_center()?)),
            "3" if ide_login.is_some() => return Ok(KiroLoginMethod::ImportKiroIde),
            other => eprintln!(
                "Unrecognized choice '{other}'. Enter 1, 2{}.",
                if ide_login.is_some() { " or 3" } else { "" }
            ),
        }
    }
}

fn prompt_identity_center() -> Result<KiroLoginTarget> {
    let start_url = read_line_trimmed(
        "IAM Identity Center start URL (e.g. https://my-org.awsapps.com/start): ",
    )?;
    if start_url.is_empty() {
        anyhow::bail!("No IAM Identity Center start URL provided.");
    }
    let region = read_line_trimmed(&format!(
        "Identity Center region [{}]: ",
        auth::kiro::DEFAULT_REGION
    ))?;
    KiroLoginTarget::identity_center(&start_url, Some(&region))
}

async fn login_with_device_flow(target: KiroLoginTarget, no_browser: bool) -> Result<()> {
    let client = crate::provider::shared_http_client();
    let authorization = auth::kiro::start_device_authorization(&client, &target).await?;
    let url = authorization.browser_url().to_string();

    eprintln!();
    eprintln!("  Sign in with {} at:", target.auth_method.label());
    eprintln!("    {url}");
    eprintln!();
    if let Some(qr) = crate::login_qr::indented_section(
        &url,
        "  Or scan this QR on another device to open the sign-in page:",
        "    ",
    ) {
        eprintln!("{qr}");
        eprintln!();
    }
    eprintln!(
        "  Confirm this code in the browser: {}",
        authorization.user_code
    );
    eprintln!();
    eprintln!("  Waiting for authorization...");
    maybe_open_browser(&url, no_browser);

    let mut tokens = auth::kiro::poll_device_token(&client, &authorization).await?;
    if tokens.auth_method == KiroAuthMethod::IdentityCenter
        && tokens.effective_profile_arn().is_none()
    {
        match auth::kiro::discover_profile_arn(&client, &tokens).await {
            Some(profile_arn) => {
                eprintln!("  Using Kiro profile {profile_arn}");
                tokens.profile_arn = Some(profile_arn);
            }
            None => eprintln!(
                "  Could not look up a Kiro profile ARN automatically. If requests are rejected, set {} to your profile ARN.",
                auth::kiro::PROFILE_ARN_ENV
            ),
        }
    }
    auth::kiro::save_tokens(&tokens)?;

    eprintln!("  ✓ Signed in to Kiro via {}", tokens.describe());
    eprintln!("  Tokens saved to {}", auth::kiro::tokens_path()?.display());
    crate::telemetry::record_auth_success("kiro", "device_code");
    Ok(())
}

fn import_kiro_ide_login() -> Result<()> {
    eprintln!(
        "Importing the Kiro IDE login. jcode copies the tokens into its own store and never modifies the IDE file."
    );
    eprintln!(
        "Note: jcode and the Kiro IDE will share this refresh token. If the IDE later asks you to sign in again, sign in there and re-run this import, or use AWS Builder ID instead."
    );
    let tokens = auth::kiro::import_kiro_ide_tokens()?;
    eprintln!("  ✓ Imported Kiro login ({})", tokens.describe());
    eprintln!("  Tokens saved to {}", auth::kiro::tokens_path()?.display());
    crate::telemetry::record_auth_success("kiro", "kiro_ide_import");
    Ok(())
}
