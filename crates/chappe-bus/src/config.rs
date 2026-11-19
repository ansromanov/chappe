//! Settings, all from environment variables.

use std::net::SocketAddr;
use std::path::PathBuf;

pub struct Config {
    pub listen: SocketAddr,
    pub db: PathBuf,
    /// Directory with one token file per machine and tool. Unset: no agent
    /// can authenticate (useful for a local run of the page only).
    pub tokens_dir: Option<PathBuf>,
    /// Projects to create at start; `all` is always there. Others appear when
    /// the first session registers in them.
    pub projects: Vec<String>,
    /// The secret of the forge webhooks (`CHAPPE_WEBHOOK_SECRET`, or the
    /// file in `CHAPPE_WEBHOOK_SECRET_FILE`), the same for Gitea, GitHub and
    /// GitLab. Unset: `/hooks/*` is off.
    pub webhook_secret: Option<String>,
    /// The address of the forge's web pages (`CHAPPE_FORGE_URL`), for links to
    /// issues. Unset: issues are shown as plain text.
    pub forge_url: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        Config::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
        let listen = get("CHAPPE_LISTEN").unwrap_or_else(|| "0.0.0.0:8080".into());
        let listen = listen
            .parse()
            .map_err(|e| format!("CHAPPE_LISTEN {listen:?}: {e}"))?;
        let projects = get("CHAPPE_PROJECTS").unwrap_or_else(|| "all".into());
        Ok(Config {
            listen,
            db: get("CHAPPE_DB")
                .unwrap_or_else(|| "chappe-bus.db".into())
                .into(),
            tokens_dir: get("CHAPPE_TOKENS_DIR").map(PathBuf::from),
            projects: parse_projects(&projects)?,
            webhook_secret: webhook_secret(&get)?,
            forge_url: get("CHAPPE_FORGE_URL").filter(|u| !u.trim().is_empty()),
        })
    }
}

fn webhook_secret(get: &impl Fn(&str) -> Option<String>) -> Result<Option<String>, String> {
    let secret = match (
        get("CHAPPE_WEBHOOK_SECRET"),
        get("CHAPPE_WEBHOOK_SECRET_FILE"),
    ) {
        (Some(s), _) => s,
        (None, Some(file)) => std::fs::read_to_string(&file)
            .map_err(|e| format!("CHAPPE_WEBHOOK_SECRET_FILE {file}: {e}"))?,
        (None, None) => return Ok(None),
    };
    let secret = secret.trim().to_string();
    if secret.len() < 16 {
        return Err("the webhook secret is shorter than 16 characters".into());
    }
    Ok(Some(secret))
}

fn valid_project(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `all,demo`; `all` is always present.
fn parse_projects(list: &str) -> Result<Vec<String>, String> {
    let mut rooms = vec!["all".to_string()];
    for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if !valid_project(name) {
            return Err(format!("CHAPPE_PROJECTS: bad project name {name:?}"));
        }
        if !rooms.iter().any(|r| r == name) {
            rooms.push(name.to_string());
        }
    }
    Ok(rooms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &'static [(&str, &str)]) -> Result<Config, String> {
        Config::from_lookup(|k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        })
    }

    #[test]
    fn defaults() {
        let c = config(&[]).unwrap();
        assert_eq!(c.listen.to_string(), "0.0.0.0:8080");
        assert_eq!(c.projects, ["all"]);
        assert!(c.tokens_dir.is_none());
        assert!(c.webhook_secret.is_none(), "the webhook is off by default");
    }

    #[test]
    fn the_webhook_secret_comes_from_the_environment_or_a_file_and_is_checked() {
        let c = config(&[("CHAPPE_WEBHOOK_SECRET", " 0123456789abcdef0123 \n")]).unwrap();
        assert_eq!(c.webhook_secret.as_deref(), Some("0123456789abcdef0123"));
        let dir = std::env::temp_dir().join(format!("bus-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("secret");
        std::fs::write(&file, "from-the-file-0123456789\n").unwrap();
        let file = file.display().to_string();
        let c = Config::from_lookup(|k| (k == "CHAPPE_WEBHOOK_SECRET_FILE").then(|| file.clone()))
            .unwrap();
        assert_eq!(
            c.webhook_secret.as_deref(),
            Some("from-the-file-0123456789")
        );
        assert!(config(&[("CHAPPE_WEBHOOK_SECRET", "short")]).is_err());
        assert!(config(&[("CHAPPE_WEBHOOK_SECRET_FILE", "/nonexistent/secret")]).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn all_is_always_first_and_not_duplicated() {
        let c = config(&[("CHAPPE_PROJECTS", "demo, all ,extra,extra")]).unwrap();
        assert_eq!(c.projects, ["all", "demo", "extra"]);
    }

    #[test]
    fn rejects_bad_values() {
        assert!(config(&[("CHAPPE_PROJECTS", "Bad Project")]).is_err());
        assert!(config(&[("CHAPPE_PROJECTS", "a/b")]).is_err());
        assert!(config(&[("CHAPPE_LISTEN", "nope")]).is_err());
    }
}
