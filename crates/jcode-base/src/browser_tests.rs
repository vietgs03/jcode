use super::*;

#[test]
fn test_is_browser_command() {
    assert!(is_browser_command("browser ping"));
    assert!(is_browser_command(
        "browser navigate '{\"url\": \"https://example.com\"}'"
    ));
    assert!(is_browser_command("browser"));
    assert!(is_browser_command("  browser ping"));
    assert!(is_browser_command("browser\tping"));

    assert!(!is_browser_command("echo browser"));
    assert!(!is_browser_command("browsers"));
    assert!(!is_browser_command("my-browser ping"));
    assert!(!is_browser_command(""));
    assert!(!is_browser_command("browserify install"));
}

#[test]
fn test_rewrite_command_with_full_path() {
    let _guard = crate::storage::lock_test_env();

    let cmd = "browser ping";
    let result = rewrite_command_with_full_path(cmd);
    // If binary exists, it rewrites; if not, returns unchanged
    if browser_binary_path().exists() {
        assert!(result.contains("ping"));
        assert!(result.contains(".jcode/browser"));
    } else {
        assert_eq!(result, cmd);
    }
}

#[test]
fn test_paths() {
    let _guard = crate::storage::lock_test_env();

    let bdir = browser_dir();
    assert!(bdir.to_string_lossy().contains(".jcode"));
    assert!(bdir.to_string_lossy().ends_with("browser"));

    let bin = browser_binary_path();
    assert!(bin.to_string_lossy().contains("browser"));

    let xpi = xpi_path();
    assert!(xpi.to_string_lossy().ends_with(".xpi"));
}

#[test]
fn test_platform_asset_name() {
    let name = get_platform_asset_name();
    assert!(name.starts_with("browser-"));
    assert!(!name.is_empty());
}

#[test]
fn test_should_prompt_extension_install_prompts_before_setup_complete() {
    let incomplete = BrowserStatus {
        backend: "firefox_agent_bridge",
        browser: "firefox",
        setup_complete: false,
        binary_installed: true,
        responding: false,
        compatible: false,
        missing_actions: vec![],
        ready: false,
    };
    assert!(should_prompt_extension_install(&incomplete));

    // Before the marker is written we always prompt, even if the bridge already
    // happens to respond: the setup flow itself decides not to reopen the
    // installer when it sees a live connection, and marks setup complete.
    let responding_first_run = BrowserStatus {
        responding: true,
        ..incomplete.clone()
    };
    assert!(should_prompt_extension_install(&responding_first_run));
}

/// Regression: a stale `.setup-complete` marker must not permanently suppress
/// the extension installer. If setup previously completed but the bridge is now
/// unresponsive with the binary still installed (extension removed/disabled, or
/// a Firefox profile switch), setup must re-prompt so it can recover instead of
/// reporting "already completed" forever.
#[test]
fn stale_setup_marker_still_prompts_when_bridge_is_not_responding() {
    let stale_marker_but_dead_bridge = BrowserStatus {
        backend: "firefox_agent_bridge",
        browser: "firefox",
        setup_complete: true,
        binary_installed: true,
        responding: false,
        compatible: false,
        missing_actions: vec![],
        ready: false,
    };
    assert!(should_prompt_extension_install(
        &stale_marker_but_dead_bridge
    ));

    // A healthy, responding bridge must stay inert: no phantom reinstall prompts.
    let healthy = BrowserStatus {
        responding: true,
        compatible: true,
        ready: true,
        ..stale_marker_but_dead_bridge.clone()
    };
    assert!(!should_prompt_extension_install(&healthy));

    // Without the binary there is nothing to (re)connect to, so the stale-marker
    // recovery path must not fire either.
    let no_binary = BrowserStatus {
        binary_installed: false,
        ..stale_marker_but_dead_bridge
    };
    assert!(!should_prompt_extension_install(&no_binary));
}

#[test]
fn setup_complete_requires_native_host_binary() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let temp = tempfile::TempDir::new().expect("create temp dir");
    crate::env::set_var("JCODE_HOME", temp.path());

    std::fs::create_dir_all(browser_dir()).expect("create browser dir");
    std::fs::write(setup_marker_path(), "test").expect("write setup marker");
    std::fs::write(browser_binary_path(), "browser").expect("write browser binary");

    assert!(browser_binary_path().exists());
    assert!(!host_binary_path().exists());
    assert!(!is_setup_complete());

    std::fs::write(host_binary_path(), "host").expect("write host binary");
    assert!(is_setup_complete());

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[tokio::test]
async fn test_inspect_browser_status_without_binary() {
    // Hold the test-env lock: this reads JCODE_HOME-derived paths, and other
    // tests mutate JCODE_HOME (and write browser fixture files) under the
    // lock. Without it, the status snapshot and the exists() check below can
    // observe different JCODE_HOME values mid-test.
    let _guard = crate::storage::lock_test_env();
    let status = inspect_browser_status().await.unwrap();
    assert_eq!(status.backend, "firefox_agent_bridge");
    assert_eq!(status.browser, "firefox");
    if !browser_binary_path().exists() {
        assert!(!status.binary_installed);
        assert!(!status.ready);
    }
}

#[tokio::test]
async fn test_ensure_browser_ready_noninteractive_without_binary() {
    // See test_inspect_browser_status_without_binary: serialize against tests
    // that mutate JCODE_HOME under the test-env lock.
    let _guard = crate::storage::lock_test_env();
    let status = ensure_browser_ready_noninteractive().await.unwrap();
    assert_eq!(status.backend, "firefox_agent_bridge");
    assert_eq!(status.browser, "firefox");
    if !browser_binary_path().exists() {
        assert!(!status.binary_installed);
        assert!(!status.ready);
        assert!(!status.setup_complete);
    }
}

#[cfg(unix)]
#[test]
fn ensure_browser_session_fails_fast_when_session_process_exits_immediately() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let temp = tempfile::TempDir::new().expect("create temp dir");
    crate::env::set_var("JCODE_HOME", temp.path());

    let browser_dir = temp.path().join("browser");
    std::fs::create_dir_all(&browser_dir).expect("create browser dir");
    let bin = browser_dir.join("browser");
    std::fs::write(&bin, "#!/bin/sh\nexit 2\n").expect("write fake browser binary");
    let mut perms = std::fs::metadata(&bin)
        .expect("stat fake browser binary")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).expect("chmod fake browser binary");

    let start = Instant::now();
    let session = ensure_browser_session("fast-fail-session");
    let elapsed = start.elapsed();

    assert!(session.is_none());
    assert!(
        elapsed < Duration::from_secs(1),
        "expected immediate failure, got {:?}",
        elapsed
    );

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[cfg(unix)]
#[test]
fn ensure_browser_session_does_not_pass_unsupported_bind_window_flag() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let temp = tempfile::TempDir::new().expect("create temp dir");
    crate::env::set_var("JCODE_HOME", temp.path());

    let browser_dir = temp.path().join("browser");
    std::fs::create_dir_all(&browser_dir).expect("create browser dir");
    let bin = browser_dir.join("browser");
    let invocations = temp.path().join("invocations");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1 $2 $3\" = \"session start --help\" ]; then\n  echo 'Usage: browser session start [NAME]'\nfi\nexit 2\n",
            invocations.display()
        ),
    )
    .expect("write fake browser binary");
    let mut perms = std::fs::metadata(&bin)
        .expect("stat fake browser binary")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).expect("chmod fake browser binary");

    assert!(ensure_browser_session("legacy-session").is_none());
    let calls = std::fs::read_to_string(invocations).expect("read invocations");
    assert!(calls.contains("session start --help"), "{calls}");
    assert!(calls.contains("session start legacy-session"), "{calls}");
    assert!(!calls.contains("--bind-window"), "{calls}");

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

/// Regression: `check_browser_ping` must not hang when the `browser` CLI blocks
/// waiting for a WebSocket reply from a Firefox extension that never answers
/// (extension not installed / disabled / Firefox closed). Without the timeout
/// this used to hang the whole status/setup path for as long as the CLI ran.
#[cfg(unix)]
#[tokio::test]
async fn check_browser_ping_times_out_instead_of_hanging_on_unresponsive_cli() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let temp = tempfile::TempDir::new().expect("create temp dir");
    crate::env::set_var("JCODE_HOME", temp.path());

    // A fake `browser` that models an unresponsive bridge: it blocks far longer
    // than the ping cap.
    let browser_dir = temp.path().join("browser");
    std::fs::create_dir_all(&browser_dir).expect("create browser dir");
    let bin = browser_dir.join("browser");
    std::fs::write(&bin, "#!/bin/sh\nsleep 120\n").expect("write fake browser binary");
    let mut perms = std::fs::metadata(&bin)
        .expect("stat fake browser binary")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).expect("chmod fake browser binary");

    // The cap is BRIDGE_PING_TIMEOUT (10s). Assert the call returns well within
    // the fake CLI's 120s sleep, proving we cap rather than wait for the child.
    let start = Instant::now();
    let responded = check_browser_ping().await.expect("ping should not error");
    let elapsed = start.elapsed();

    assert!(!responded, "an unresponsive CLI must report not-responding");
    assert!(
        elapsed < BRIDGE_PING_TIMEOUT + Duration::from_secs(5),
        "check_browser_ping must return near the cap, took {:?}",
        elapsed
    );

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

/// A responsive `browser ping` (prints `pong` and exits 0) is reported as
/// responding, and the capped runner returns promptly.
#[cfg(unix)]
#[tokio::test]
async fn check_browser_ping_reports_pong_from_a_responsive_cli() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let temp = tempfile::TempDir::new().expect("create temp dir");
    crate::env::set_var("JCODE_HOME", temp.path());

    let browser_dir = temp.path().join("browser");
    std::fs::create_dir_all(&browser_dir).expect("create browser dir");
    let bin = browser_dir.join("browser");
    std::fs::write(&bin, "#!/bin/sh\necho pong\n").expect("write fake browser binary");
    let mut perms = std::fs::metadata(&bin)
        .expect("stat fake browser binary")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).expect("chmod fake browser binary");

    assert!(
        check_browser_ping().await.expect("ping should not error"),
        "a `pong` reply must be reported as responding"
    );

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}
