/// A parsed `[user@]host[:path]` target.
#[derive(Debug, PartialEq)]
pub struct Target {
    /// Passed to ssh verbatim (may include `user@`). `"local"` is the
    /// loopback pseudo-host: the agent runs as a local subprocess.
    pub host: String,
    /// Remote path as given; empty means the remote home directory.
    pub path: String,
}

impl Target {
    pub fn parse(s: &str) -> Self {
        match s.split_once(':') {
            Some((host, path)) => Target {
                host: host.to_string(),
                path: path.to_string(),
            },
            None => Target {
                host: s.to_string(),
                path: String::new(),
            },
        }
    }

    pub fn is_local(&self) -> bool {
        self.host == "local"
    }

    /// Directory-name-safe form of the host for the workspace prefix.
    pub fn host_slug(&self) -> String {
        self.host.replace([':', '/'], "_")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets() {
        assert_eq!(
            Target::parse("dev-box:~/proj"),
            Target {
                host: "dev-box".into(),
                path: "~/proj".into()
            }
        );
        assert_eq!(
            Target::parse("user@10.0.0.1"),
            Target {
                host: "user@10.0.0.1".into(),
                path: "".into()
            }
        );
        assert_eq!(
            Target::parse("local:/tmp/x"),
            Target {
                host: "local".into(),
                path: "/tmp/x".into()
            }
        );
        assert!(Target::parse("local").is_local());
    }
}
