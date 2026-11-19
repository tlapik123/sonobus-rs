//! `sonobus://` and `go.sonobus.net/sblaunch` links (ConnectView.cpp:870-915, 1320-1417) and
//! the command line (SonoStandaloneFilterApp.cpp:293-430).

use sonobus_core::state::DEFAULT_SERVER_PORT;

/// Upstream's default connection server (P.h:30-31). Never contacted unless the user connects.
pub const DEFAULT_SERVER_HOST: &str = "aoo.sonobus.net";

/// Group connection details carried by a link.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Link {
    /// `None`: the link does not name a server (an `sblaunch` link without `s`).
    pub server: Option<(String, u16)>,
    pub group: Option<String>,
    pub password: String,
    pub public: bool,
}

/// `host[:port]`, port defaulting to 10998 when missing or not positive (as upstream).
pub fn parse_host_port(s: &str) -> (String, u16) {
    let (host, port) = s.split_once(':').unwrap_or((s, ""));
    let port = port.parse::<u16>().ok().filter(|&p| p > 0).unwrap_or(DEFAULT_SERVER_PORT as u16);
    (host.trim().to_owned(), port)
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// JUCE `URL::removeEscapeChars`: `+` is a space, `%XX` a byte.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Finds a `sonobus://` or `http(s)://go.sonobus.net/sblaunch?` link anywhere in `text` (as
/// "Paste Share Link" does) and reads it.
pub fn parse_link(text: &str) -> Option<Link> {
    let (start, sonobus) = match text.find("sonobus://") {
        Some(i) => (i, true),
        None => {
            let i = text
                .find("http://go.sonobus.net/sblaunch?")
                .or_else(|| text.find("https://go.sonobus.net/sblaunch?"))?;
            (i, false)
        }
    };
    let url = text[start..].split_whitespace().next()?;
    let (head, query) = url.split_once('?').unwrap_or((url, ""));
    let params: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (unescape(k), unescape(v))
        })
        .collect();
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let mut link = Link::default();
    if let Some(g) = get("g") {
        link.group = Some(g.to_owned());
        link.password = get("p").unwrap_or_default().to_owned();
        link.public = get("public").and_then(|v| v.trim().parse::<i64>().ok()).is_some_and(|v| v > 0);
    }
    if sonobus {
        let authority = head["sonobus://".len()..].split('/').next().unwrap_or_default();
        if authority.is_empty() {
            return None;
        }
        link.server = Some(parse_host_port(authority));
    } else if let Some(s) = get("s") {
        link.server = Some(parse_host_port(s));
    }
    Some(link)
}

/// Inverse of [`parse_host_port`]: the default port is left out.
pub fn host_port_text(host: &str, port: u16) -> String {
    if i32::from(port) == DEFAULT_SERVER_PORT { host.to_owned() } else { format!("{host}:{port}") }
}

/// The `http://go.sonobus.net/sblaunch?...` link upstream copies with "Copy Share Link".
pub fn share_link(host: &str, port: u16, group: &str, password: &str, public: bool) -> String {
    let hostport = host_port_text(host, port);
    let mut s = format!("http://go.sonobus.net/sblaunch?s={}&g={}", escape(&hostport), escape(group));
    if !password.is_empty() {
        s += &format!("&p={}", escape(password));
    }
    if public {
        s += "&public=1";
    }
    s
}

/// The `sonobus://host:port/?g=..` form.
pub fn sonobus_link(host: &str, port: u16, group: &str, password: &str, public: bool) -> String {
    let mut s = format!("sonobus://{host}:{port}/?g={}", escape(group));
    if !password.is_empty() {
        s += &format!("&p={}", escape(password));
    }
    if public {
        s += "&public=1";
    }
    s
}

/// Upstream's group name grammar (`Source/wordmaker.g`, GPLv3).
const WORDMAKER: &str = include_str!("../assets/wordmaker.g");

fn random_index(n: usize) -> usize {
    use std::hash::{BuildHasher, Hasher};
    // every RandomState has fresh keys: its empty hash is a random number
    (std::collections::hash_map::RandomState::new().build_hasher().finish() % n.max(1) as u64) as usize
}

/// "Generate a random group name" (RandomSentenceGenerator.cpp with `capEveryWord`): expands
/// `<start>` of the grammar, every substituted word capitalised, e.g. "AbleBlueOcean".
pub fn random_group_name() -> String {
    let mut rules: std::collections::HashMap<String, Vec<&str>> = std::collections::HashMap::new();
    let delims: &[char] = &[' ', '\t', '\r', ';'];
    let mut lines = WORDMAKER.lines();
    while let Some(l) = lines.next() {
        if !l.contains('{') {
            continue;
        }
        let Some(rule) = lines.next() else { break };
        let list = rules.entry(rule.trim_end_matches(delims).to_lowercase()).or_default();
        for l in lines.by_ref() {
            if l.contains('}') {
                break;
            }
            list.push(l.trim_end_matches(delims));
        }
    }
    fn expand(rules: &std::collections::HashMap<String, Vec<&str>>, rule: &str, depth: u32) -> String {
        let Some(list) = rules.get(rule).filter(|l| !l.is_empty() && depth < 8) else { return String::new() };
        let mut s = list[random_index(list.len())].to_owned();
        while let (Some(a), Some(b)) = (s.find('<'), s.find('>')) {
            if b < a {
                break;
            }
            let mut sub = expand(rules, &s[a..=b].to_lowercase(), depth + 1);
            if let Some(c) = sub.get(..1) {
                sub = c.to_uppercase() + &sub[1..];
            }
            s.replace_range(a..=b, &sub);
        }
        s
    }
    expand(&rules, "<start>", 0)
}

/// Parsed command line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cli {
    pub version: bool,
    pub help: bool,
    /// `-q|--headless`: no GUI.
    pub headless: bool,
    /// `-c|--connectionserver host[:port]`
    pub server: Option<(String, u16)>,
    /// `-g|--group`: connect on launch.
    pub group: Option<String>,
    /// `-p|--group-password`
    pub group_password: Option<String>,
    /// `-n|--username`
    pub user: Option<String>,
    /// `-l|--load-setup <file>`
    pub load_setup: Option<String>,
    /// Trailing bare argument: a connect link.
    pub url: Option<String>,
    /// `--host-server <port>` (ours): host a session on this device.
    pub host_server: Option<u16>,
    /// `--no-audio` (ours): null audio, no devices.
    pub no_audio: bool,
    /// `--settings-dir <dir>` (ours): settings location instead of the OS default.
    pub settings_dir: Option<String>,
    /// `--host-session <name>`: host a secure session.
    pub secure_host: Option<String>,
    /// `--join <link>` or a bare `sonobus2://join/...` argument: join a secure session.
    pub secure_join: Option<String>,
    /// `--broadcast`: the hosted secure session is "Share sound" (listeners only).
    pub broadcast: bool,
    /// `--pin`: accept PIN joins (the PIN is shown with `--print-invite`).
    pub pin: bool,
    /// `--lan`: advertise the hosted secure session on the local network.
    pub lan: bool,
    /// `--password <password>`: accept password joins to the hosted secure session.
    pub password: Option<String>,
    /// `--relay none|n0|URL`: relays for secure sessions (remembered).
    pub relay: Option<String>,
    /// `--print-invite`: print the invite link (and PIN) of the hosted secure session. It is a
    /// secret: whoever has it can join.
    pub print_invite: bool,
}

pub const USAGE: &str = "Usage: sonobus [options...]  [connect_URL]
  -h|--help                              Prints the list of commands
  -v|--version                           Prints the current version number only
  -g|--group <groupname>                 Specify the group name to immediately connect to upon launch
  -n|--username <username>               Specify the displayed username for yourself when connecting to a group
  -p|--group-password <password>         Specify the password to use for the group name to connect to (optional)
  -c|--connectionserver <address[:port]> Specify connection server to use when connecting to a group (optional)
  -l|--load-setup <setup-filename>       Specify the filename of a setup file to load.
  -q|--headless                          If specified, no GUI will be used and the application will be run headless.
  --host-server <port>                   Host a session on this device (connection server on <port>) and join it
  --no-audio                             Do not open audio devices (the engine runs on a timer)
  --settings-dir <dir>                   Keep settings in <dir>
Secure sessions (this app only, end-to-end encrypted):
  --host-session <name>                  Host a secure session
    --broadcast                          ...as \"Share sound\": others only listen
    --password <password>                ...accepting this password (LAN list, rendezvous)
    --pin                                ...accepting a 6-digit PIN
    --lan                                ...advertised on the local network
    --print-invite                       Print its invite link and PIN (secrets: anyone with them can join)
  --join <invite link>                   Join a secure session (a bare sonobus2://join/... argument too)
  --relay none|n0|<url>                  Relays for secure sessions (default n0: iroh's public relays)";

/// A `sonobus2://join/...` link (the OS hands these to the app).
pub fn is_invite_link(s: &str) -> bool {
    let p = sonobus_net::auth::INVITE_URL_PREFIX;
    s.trim().get(..p.len()).is_some_and(|x| x.eq_ignore_ascii_case(p))
}

/// `--relay`'s value: `none`, `n0` or a relay URL.
pub fn relay_choice(s: &str) -> Result<sonobus_engine::modern::RelayChoice, String> {
    use sonobus_engine::modern::RelayChoice;
    Ok(match s.trim() {
        "none" => RelayChoice::None,
        "n0" => RelayChoice::N0Default,
        u if u.starts_with("https://") || u.starts_with("http://") => RelayChoice::Custom(vec![u.to_owned()]),
        u => return Err(format!("--relay: none, n0 or an http(s) URL, not {u:?}")),
    })
}

/// Parses the arguments after the program name. Options take `--opt value` or `--opt=value`.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with('-') => (n.to_owned(), Some(v.to_owned())),
            _ => (a.clone(), None),
        };
        let mut value = || inline.clone().or_else(|| it.next()).ok_or(format!("{name} needs a value"));
        match name.as_str() {
            "-v" | "--version" => cli.version = true,
            "-h" | "--help" => cli.help = true,
            "-q" | "--headless" => cli.headless = true,
            "--no-audio" => cli.no_audio = true,
            "-c" | "--connectionserver" => cli.server = Some(parse_host_port(&value()?)),
            "-g" | "--group" => cli.group = Some(value()?.trim().to_owned()).filter(|g| !g.is_empty()),
            "-p" | "--group-password" => cli.group_password = Some(value()?),
            "-n" | "--username" => cli.user = Some(value()?.trim().to_owned()).filter(|u| !u.is_empty()),
            "-l" | "--load-setup" => cli.load_setup = Some(value()?),
            "--settings-dir" => cli.settings_dir = Some(value()?),
            "--host-session" => cli.secure_host = Some(value()?.trim().to_owned()).filter(|g| !g.is_empty()),
            "--join" => cli.secure_join = Some(value()?),
            "--broadcast" => cli.broadcast = true,
            "--pin" => cli.pin = true,
            "--lan" => cli.lan = true,
            "--password" => cli.password = Some(value()?),
            "--relay" => cli.relay = Some(value()?),
            "--print-invite" => cli.print_invite = true,
            "--host-server" => {
                let v = value()?;
                cli.host_server = Some(v.parse().map_err(|_| format!("bad port {v}"))?);
            }
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option {s}")),
            _ if is_invite_link(&a) => cli.secure_join = Some(a),
            _ => cli.url = Some(a),
        }
    }
    if let Some(r) = &cli.relay {
        relay_choice(r)?;
    }
    if cli.headless && cli.group.is_none() && cli.host_server.is_none() && cli.url.is_none() && cli.secure_host.is_none() && cli.secure_join.is_none() {
        return Err("Error: you need to specify a group to connect to for headless operation".into());
    }
    Ok(cli)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn sonobus_scheme() {
        let l = parse_link("join me: sonobus://example.org:11000/?g=my%20band&p=s3cret&public=1 thanks").unwrap();
        assert_eq!(l.server, Some(("example.org".into(), 11000)));
        assert_eq!(l.group.as_deref(), Some("my band"));
        assert_eq!(l.password, "s3cret");
        assert!(l.public);
        // no port: the default
        let l = parse_link("sonobus://h/?g=a+b").unwrap();
        assert_eq!(l.server, Some(("h".into(), 10998)));
        for (h, p) in [("h", 10998), ("h", 10999)] {
            assert_eq!(parse_host_port(&host_port_text(h, p)), (h.into(), p));
        }
        assert_eq!(host_port_text("h", 10998), "h");
        assert_eq!(l.group.as_deref(), Some("a b"));
        assert!(!l.public);
    }

    #[test]
    fn sblaunch_and_round_trip() {
        let l = parse_link("https://go.sonobus.net/sblaunch?s=10.0.0.2:10999&g=x&public=0").unwrap();
        assert_eq!(l.server, Some(("10.0.0.2".into(), 10999)));
        assert_eq!(l.group.as_deref(), Some("x"));
        assert!(!l.public);
        let link = share_link("aoo.sonobus.net", 10998, "Jazz & Co/2", "p w", true);
        assert_eq!(link, "http://go.sonobus.net/sblaunch?s=aoo.sonobus.net&g=Jazz%20%26%20Co%2F2&p=p%20w&public=1");
        let l = parse_link(&link).unwrap();
        assert_eq!(
            l,
            Link {
                server: Some(("aoo.sonobus.net".into(), 10998)),
                group: Some("Jazz & Co/2".into()),
                password: "p w".into(),
                public: true
            }
        );
        let l = parse_link(&sonobus_link("192.168.1.5", 10999, "g", "", false)).unwrap();
        assert_eq!(l.server, Some(("192.168.1.5".into(), 10999)));
        assert!(parse_link("nothing here").is_none());
        assert_eq!(unescape("%4"), "%4");
    }

    #[test]
    fn random_group_names() {
        let a = random_group_name();
        assert!(a.len() >= 6 && !a.contains(['<', '>', ' ', ';']), "{a}");
        assert!(a.starts_with(|c: char| c.is_ascii_uppercase()), "{a}");
        assert!((0..5).map(|_| random_group_name()).any(|b| b != a));
    }

    #[test]
    fn command_line() {
        let c = parse_args(args("-c myserver:11000 --group=band -p pw -n Alice -q --no-audio")).unwrap();
        assert_eq!(c.server, Some(("myserver".into(), 11000)));
        assert_eq!(c.group.as_deref(), Some("band"));
        assert_eq!(c.group_password.as_deref(), Some("pw"));
        assert_eq!(c.user.as_deref(), Some("Alice"));
        assert!(c.headless && c.no_audio);
        let c = parse_args(args("-l setup.sonobus sonobus://h:1/?g=x")).unwrap();
        assert_eq!(c.load_setup.as_deref(), Some("setup.sonobus"));
        assert_eq!(c.url.as_deref(), Some("sonobus://h:1/?g=x"));
        assert!(parse_args(args("-q")).is_err());
        assert!(parse_args(args("--bogus")).is_err());
        assert!(parse_args(args("-g")).is_err());
        assert_eq!(parse_args(args("--host-server 10999 -q")).unwrap().host_server, Some(10999));
        assert_eq!(parse_args(args("-c host")).unwrap().server, Some(("host".into(), 10998)));
        // secure sessions
        let c = parse_args(args("-q --host-session jam --broadcast --pin --lan --password pw --relay none --print-invite")).unwrap();
        assert_eq!((c.secure_host.as_deref(), c.password.as_deref(), c.relay.as_deref()), (Some("jam"), Some("pw"), Some("none")));
        assert!(c.broadcast && c.pin && c.lan && c.print_invite);
        let c = parse_args(args("-q SONOBUS2://join/abc")).unwrap();
        assert_eq!((c.secure_join.as_deref(), c.url), (Some("SONOBUS2://join/abc"), None));
        assert_eq!(parse_args(args("--join x")).unwrap().secure_join.as_deref(), Some("x"));
        assert!(parse_args(args("--relay ftp://x")).is_err());
        assert_eq!(relay_choice("https://r.example/"), Ok(sonobus_engine::modern::RelayChoice::Custom(vec!["https://r.example/".into()])));
    }
}
