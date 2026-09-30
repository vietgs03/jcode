//! `/login kiro`: AWS Builder ID / IAM Identity Center device flow.
//!
//! Uses Builder ID unless `JCODE_KIRO_START_URL` (and optionally
//! `JCODE_KIRO_REGION`) select an IAM Identity Center. Importing a Kiro IDE
//! login is available from `jcode login --provider kiro`.

use super::*;

fn publish_kiro_login_result(success: bool, message: String) {
    Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
        provider: "kiro".to_string(),
        success,
        message,
    }));
}

impl App {
    pub(super) fn start_kiro_login(&mut self) {
        let target = match crate::auth::kiro::KiroLoginTarget::from_env() {
            Ok(target) => target,
            Err(err) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Kiro login is unavailable: {err}"
                )));
                self.set_status_notice("Login: kiro failed");
                return;
            }
        };
        self.set_status_notice("Login: kiro device flow...");
        self.begin_pending_login(PendingLogin::Kiro);

        tokio::spawn(async move {
            let client = crate::provider::shared_http_client();
            let authorization =
                match crate::auth::kiro::start_device_authorization(&client, &target).await {
                    Ok(authorization) => authorization,
                    Err(err) => {
                        publish_kiro_login_result(false, format!("Kiro device flow failed: {err}"));
                        return;
                    }
                };

            let url = authorization.browser_url().to_string();
            let user_code = authorization.user_code.clone();
            let clipboard_note = if copy_to_clipboard(&user_code) {
                " (copied to clipboard)"
            } else {
                ""
            };
            let qr_section = crate::login_qr::markdown_section_for_tui(
                &url,
                "Scan this on another device to open the AWS sign-in page:",
            )
            .map(|section| format!("\n\n{section}"))
            .unwrap_or_else(String::new);
            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                provider: "kiro_code".to_string(),
                success: true,
                message: format!(
                    "Kiro Login ({})\n\n\
                     Your code: {}{}\n\n\
                     Opening browser to {} ...\n\
                     Confirm the code there and approve access.{}\n\n\
                     Waiting for authorization... (type /cancel to abort)",
                    target.auth_method.label(),
                    user_code,
                    clipboard_note,
                    url,
                    qr_section
                ),
            }));

            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if !Self::open_auth_browser(&url) {
                crate::logging::info(
                    "Kiro login: browser not opened; the sign-in URL is shown in the transcript",
                );
            }

            let mut tokens =
                match crate::auth::kiro::poll_device_token(&client, &authorization).await {
                    Ok(tokens) => tokens,
                    Err(err) => {
                        publish_kiro_login_result(false, format!("Kiro login failed: {err}"));
                        return;
                    }
                };
            if tokens.auth_method == crate::auth::kiro::KiroAuthMethod::IdentityCenter
                && tokens.effective_profile_arn().is_none()
            {
                tokens.profile_arn =
                    crate::auth::kiro::discover_profile_arn(&client, &tokens).await;
            }

            match crate::auth::kiro::save_tokens(&tokens) {
                Ok(()) => publish_kiro_login_result(
                    true,
                    format!(
                        "Signed in to Kiro via {}.\n\nKiro models are now available in /model.",
                        tokens.describe()
                    ),
                ),
                Err(err) => publish_kiro_login_result(
                    false,
                    format!("Failed to save the Kiro login: {err}"),
                ),
            }
        });

        self.push_display_message(DisplayMessage::system(
            "Kiro Login\n\nStarting AWS device flow... please wait. Type /cancel to abort."
                .to_string(),
        ));
    }
}
