//! Auto-select the AWS CLI profile for the SSM tunnel by matching the control
//! plane's AWS account.
//!
//! `devbox ssh` opens an `aws ssm start-session` tunnel that needs credentials
//! for the account the pool runs in. Engineers who ran `vouch setup aws
//! --discover` have one profile per account/role and would otherwise have to
//! remember which is the devbox account. Instead we read the account the server
//! advertises (in its RFC 9728 discovery document) and pick the local
//! `~/.aws/config` profile whose role targets it.
//!
//! Parsing the config with `rust-ini` (rather than shelling out to `aws
//! configure`) keeps the matching logic pure and unit-testable and avoids a
//! per-profile subprocess fan-out. We recognise the two ways a profile encodes
//! its account: an assume-role `role_arn`, or a `credential_process` that
//! carries a `--role <arn>` (how `vouch setup aws` writes profiles).

use std::path::PathBuf;

use anyhow::{Context, Result};
use dialoguer::Select;
use ini::Ini;

/// Pick the AWS profile that targets `account_id`, or `None` to fall back to the
/// caller's default credentials.
///
/// Reads `~/.aws/config` (or `$AWS_CONFIG_FILE`). Zero matches or a missing
/// config warn and return `None`; exactly one match is returned directly;
/// several open an interactive picker when `interactive`, else warn and return
/// `None`.
///
/// # Errors
///
/// Returns an error only when the interactive picker is cancelled.
pub(crate) fn select_profile(account_id: &str, interactive: bool) -> Result<Option<String>> {
    let Some(config) = read_aws_config() else {
        eprintln!(
            "warning: no AWS config found; ssh will use your default AWS credentials \
             (pass --profile to choose one explicitly)."
        );
        return Ok(None);
    };

    let mut profiles = profiles_for_account(&config, account_id);
    match profiles.len() {
        0 => {
            eprintln!(
                "warning: no AWS profile targets account {account_id}; ssh will use your \
                 default AWS credentials. Set one up with `vouch setup aws` or pass --profile."
            );
            Ok(None)
        }
        1 => Ok(profiles.pop()),
        _ if interactive => {
            let choice = Select::new()
                .with_prompt(format!("Select an AWS profile for account {account_id}"))
                .items(&profiles)
                .default(0)
                .interact()
                .context("AWS profile selection cancelled")?;
            Ok(profiles.get(choice).cloned())
        }
        _ => {
            eprintln!(
                "warning: multiple AWS profiles target account {account_id} ({}); ssh will use \
                 your default AWS credentials. Pass --profile to choose one.",
                profiles.join(", ")
            );
            Ok(None)
        }
    }
}

/// The path to the AWS config file: `$AWS_CONFIG_FILE` if set, else
/// `~/.aws/config`. `None` when neither is resolvable.
fn aws_config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("AWS_CONFIG_FILE").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(home).join(".aws").join("config"))
}

/// Read the AWS config file, or `None` when it is absent or unreadable.
fn read_aws_config() -> Option<String> {
    std::fs::read_to_string(aws_config_path()?).ok()
}

/// Profile names from `config_text` whose role targets `account_id`, sorted and
/// de-duplicated. Recognises `[profile NAME]` and `[default]` sections; ignores
/// every other section (e.g. `[sso-session …]`, `[services …]`). An unparseable
/// config warns and yields no matches (auto-select falls back to default creds).
fn profiles_for_account(config_text: &str, account_id: &str) -> Vec<String> {
    let ini = match Ini::load_from_str(config_text) {
        Ok(ini) => ini,
        Err(e) => {
            eprintln!("warning: could not parse AWS config: {e}");
            return Vec::new();
        }
    };

    let mut matches: Vec<String> = Vec::new();
    for (section, properties) in ini.iter() {
        let Some(name) = section.and_then(profile_name) else {
            continue;
        };
        let account = account_of_profile(
            properties.get("role_arn"),
            properties.get("credential_process"),
        );
        if account.as_deref() == Some(account_id) {
            matches.push(name);
        }
    }

    matches.sort();
    matches.dedup();
    matches
}

/// The profile name a `[…]` header denotes: `default` for `[default]`, the
/// trailing name for `[profile NAME]`, and `None` for any other section.
fn profile_name(header: &str) -> Option<String> {
    let header = header.trim();
    if header == "default" {
        return Some("default".to_string());
    }
    let rest = header.strip_prefix("profile")?;
    let name = rest.trim();
    if rest.starts_with(char::is_whitespace) && !name.is_empty() {
        Some(name.to_string())
    } else {
        None
    }
}

/// The account a profile targets, via its `role_arn` or a `credential_process`
/// `--role <arn>`.
fn account_of_profile(role_arn: Option<&str>, credential_process: Option<&str>) -> Option<String> {
    if let Some(arn) = role_arn
        && let Some(account) = account_of_arn(arn)
    {
        return Some(account.to_string());
    }
    if let Some(cmd) = credential_process
        && let Some(arn) = role_arg(cmd)
        && let Some(account) = account_of_arn(arn)
    {
        return Some(account.to_string());
    }
    None
}

/// The value following `--role` (or `--role=…`) in a `credential_process`
/// command line.
fn role_arg(cmd: &str) -> Option<&str> {
    let mut tokens = cmd.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "--role" {
            return tokens.next();
        }
        if let Some(value) = token.strip_prefix("--role=") {
            return Some(value);
        }
    }
    None
}

/// The account id in an IAM-style ARN (`arn:aws:iam::<account>:role/…`) — the
/// non-empty 5th colon-field. `None` for anything else.
fn account_of_arn(arn: &str) -> Option<&str> {
    arn.split(':').nth(4).filter(|account| !account.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_of_arn_extracts_account() {
        assert_eq!(
            account_of_arn("arn:aws:iam::123456789012:role/Admin"),
            Some("123456789012")
        );
    }

    #[test]
    fn account_of_arn_rejects_garbage() {
        assert_eq!(account_of_arn("not-an-arn"), None);
        assert_eq!(account_of_arn("arn:aws:iam:::role/NoAccount"), None);
        assert_eq!(account_of_arn(""), None);
    }

    #[test]
    fn role_arg_parses_both_forms() {
        assert_eq!(
            role_arg("vouch credential aws --role arn:aws:iam::1:role/X"),
            Some("arn:aws:iam::1:role/X")
        );
        assert_eq!(
            role_arg("vouch credential aws --role=arn:aws:iam::1:role/X --json"),
            Some("arn:aws:iam::1:role/X")
        );
        assert_eq!(role_arg("vouch credential aws --json"), None);
    }

    /// A realistic `~/.aws/config` covering a `vouch` credential_process profile,
    /// a plain assume-role profile, `[default]`, an unrelated account, a second
    /// profile in the target account, and an ignored `[sso-session]` section.
    const FIXTURE: &str = "\
[default]
region = us-east-1

[profile devbox]
credential_process = /usr/local/bin/vouch credential aws --role arn:aws:iam::111111111111:role/VouchAccess
region = us-west-2

[profile devbox-alt]
role_arn = arn:aws:iam::111111111111:role/Other
source_profile = default

[profile prod]
role_arn = arn:aws:iam::222222222222:role/Prod
source_profile = default

# a comment line
[sso-session corp]
sso_region = us-east-1
sso_account_id = 111111111111
";

    #[test]
    fn profiles_for_account_matches_credential_process_and_role_arn() {
        let mut profiles = profiles_for_account(FIXTURE, "111111111111");
        profiles.sort();
        // Both the vouch credential_process profile and the plain assume-role
        // profile in account 111111111111 match; the [sso-session] is ignored.
        assert_eq!(profiles, ["devbox", "devbox-alt"]);
    }

    #[test]
    fn profiles_for_account_isolates_other_accounts() {
        assert_eq!(profiles_for_account(FIXTURE, "222222222222"), ["prod"]);
        assert!(profiles_for_account(FIXTURE, "999999999999").is_empty());
    }

    #[test]
    fn profiles_for_account_ignores_non_profile_sections() {
        // The account only appears in an [sso-session] block → no profile match.
        let text = "\
[sso-session corp]
sso_account_id = 333333333333
";
        assert!(profiles_for_account(text, "333333333333").is_empty());
    }

    #[test]
    fn profiles_for_account_tolerates_inline_comments_and_matches_default() {
        // Inline comments on values and section headers must not break parsing,
        // and `[default]` is a valid, matchable profile.
        let text = "\
[default]
role_arn = arn:aws:iam::444444444444:role/Base  # primary
region = us-east-1

[profile work] ; work account
credential_process = vouch credential aws --role arn:aws:iam::444444444444:role/Work
";
        let mut profiles = profiles_for_account(text, "444444444444");
        profiles.sort();
        assert_eq!(profiles, ["default", "work"]);
    }
}
