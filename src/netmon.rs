use serde_json::Value;

/// Protocol filter for the Network Monitor page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProtoFilter {
    #[default]
    All,
    Tcp,
    Udp,
}

/// Scope / state filter for the Network Monitor page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScopeFilter {
    #[default]
    All,
    Active,
    Listening,
    External,
    Lan,
    Loopback,
}

/// Sub-view inside the Network Monitor page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MonitorView {
    #[default]
    Connections,
    ByApp,
    Interfaces,
}

/// Sort column for connections on the Network Monitor page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MonitorSort {
    #[default]
    Traffic,
    Process,
    State,
    Remote,
    Port,
}

impl MonitorSort {
    pub fn next(self) -> Self {
        match self {
            Self::Traffic => Self::Process,
            Self::Process => Self::State,
            Self::State => Self::Remote,
            Self::Remote => Self::Port,
            Self::Port => Self::Traffic,
        }
    }
}

/// All active filter parameters for the Network Monitor connections table.
#[derive(Debug, Clone, Default)]
pub struct NetMonFilters {
    pub search: String,
    pub proto: ProtoFilter,
    pub scope: ScopeFilter,
    pub sort: MonitorSort,
}

impl NetMonFilters {
    pub fn is_filtered(&self) -> bool {
        !self.search.trim().is_empty()
            || self.proto != ProtoFilter::All
            || self.scope != ScopeFilter::All
    }

    pub fn clear(&mut self) {
        self.search.clear();
        self.proto = ProtoFilter::All;
        self.scope = ScopeFilter::All;
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn num_field(v: &Value, key: &str) -> f64 {
    v.get(key)
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite())
        .unwrap_or(0.0)
}

fn u64_field(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

pub fn conn_proto<'a>(v: &'a Value) -> &'a str {
    let p = str_field(v, "proto");
    if !p.is_empty() {
        p
    } else {
        str_field(v, "protocol")
    }
}

pub fn conn_id(v: &Value) -> String {
    let id = str_field(v, "id");
    if !id.is_empty() {
        id.to_string()
    } else {
        format!(
            "{}:{}:{}->{}:{}",
            conn_proto(v),
            str_field(v, "local_ip"),
            u64_field(v, "local_port"),
            str_field(v, "remote_ip"),
            u64_field(v, "remote_port")
        )
    }
}

/// Filter and sort connection indices according to `filters`.
pub fn filter_connections(connections: &[Value], filters: &NetMonFilters) -> Vec<usize> {
    let q = filters.search.trim().to_lowercase();
    let mut indices: Vec<usize> = (0..connections.len())
        .filter(|&i| {
            let c = &connections[i];
            let proto = conn_proto(c);
            match filters.proto {
                ProtoFilter::All => {}
                ProtoFilter::Tcp => {
                    if !proto.starts_with("tcp") {
                        return false;
                    }
                }
                ProtoFilter::Udp => {
                    if !proto.starts_with("udp") {
                        return false;
                    }
                }
            }
            let state = str_field(c, "state");
            let scope = str_field(c, "scope");
            match filters.scope {
                ScopeFilter::All => {}
                ScopeFilter::Active => {
                    if !matches!(state, "ESTABLISHED" | "SYN_SENT" | "SYN_RECV" | "UNCONN") {
                        return false;
                    }
                }
                ScopeFilter::Listening => {
                    if state != "LISTEN" {
                        return false;
                    }
                }
                ScopeFilter::External => {
                    if scope != "external" {
                        return false;
                    }
                }
                ScopeFilter::Lan => {
                    if scope != "lan" {
                        return false;
                    }
                }
                ScopeFilter::Loopback => {
                    if scope != "loopback" {
                        return false;
                    }
                }
            }
            if !q.is_empty() {
                let hay = format!(
                    "{} {} {} {} {} {} {} {} {} {}",
                    str_field(c, "process"),
                    str_field(c, "cmdline"),
                    str_field(c, "remote_ip"),
                    str_field(c, "remote_host"),
                    u64_field(c, "remote_port"),
                    str_field(c, "local_ip"),
                    u64_field(c, "local_port"),
                    str_field(c, "service"),
                    u64_field(c, "pid"),
                    state,
                )
                .to_lowercase();
                if !hay.contains(&q) {
                    return false;
                }
            }
            true
        })
        .collect();

    indices.sort_by(|&a, &b| {
        let ca = &connections[a];
        let cb = &connections[b];
        match filters.sort {
            MonitorSort::Traffic => {
                let ra = num_field(ca, "rx_rate") + num_field(ca, "tx_rate");
                let rb = num_field(cb, "rx_rate") + num_field(cb, "tx_rate");
                let ba = num_field(ca, "rx_bytes") + num_field(ca, "tx_bytes");
                let bb = num_field(cb, "rx_bytes") + num_field(cb, "tx_bytes");
                rb.total_cmp(&ra)
                    .then_with(|| bb.total_cmp(&ba))
                    .then_with(|| str_field(ca, "process").cmp(str_field(cb, "process")))
            }
            MonitorSort::Process => str_field(ca, "process")
                .to_lowercase()
                .cmp(&str_field(cb, "process").to_lowercase())
                .then_with(|| u64_field(ca, "remote_port").cmp(&u64_field(cb, "remote_port"))),
            MonitorSort::State => state_rank(str_field(ca, "state"))
                .cmp(&state_rank(str_field(cb, "state")))
                .then_with(|| str_field(ca, "process").cmp(str_field(cb, "process"))),
            MonitorSort::Remote => {
                let ha = preferred_remote(ca);
                let hb = preferred_remote(cb);
                ha.to_lowercase()
                    .cmp(&hb.to_lowercase())
                    .then_with(|| u64_field(ca, "remote_port").cmp(&u64_field(cb, "remote_port")))
            }
            MonitorSort::Port => {
                let pa = effective_port(ca);
                let pb = effective_port(cb);
                pa.cmp(&pb)
                    .then_with(|| str_field(ca, "process").cmp(str_field(cb, "process")))
            }
        }
    });

    indices
}

fn state_rank(state: &str) -> u8 {
    match state {
        "ESTABLISHED" => 0,
        "LISTEN" => 1,
        "SYN_SENT" | "SYN_RECV" => 2,
        "CLOSE_WAIT" => 3,
        "TIME_WAIT" => 4,
        "UNCONN" => 5,
        _ => 6,
    }
}

pub fn preferred_remote(conn: &Value) -> &str {
    let host = str_field(conn, "remote_host");
    if !host.is_empty() {
        host
    } else {
        str_field(conn, "remote_ip")
    }
}

pub fn effective_port(conn: &Value) -> u64 {
    let rp = u64_field(conn, "remote_port");
    if rp > 0 {
        rp
    } else {
        u64_field(conn, "local_port")
    }
}

/// Format a filtered connection list as a clean Markdown table for clipboard export.
pub fn connections_markdown(connections: &[Value], indices: &[usize]) -> String {
    let cell = |v: &str| v.replace(['|', '\r', '\n'], " ").trim().to_string();
    let mut lines = vec![
        "| Process | Proto | State | Local | Remote | Service | Scope |".to_string(),
        "| --- | --- | --- | --- | --- | --- | --- |".to_string(),
    ];
    for &i in indices.iter().take(256) {
        let c = &connections[i];
        let proc_name = str_field(c, "process");
        let proc_label = if proc_name.is_empty() { "—" } else { proc_name };
        let local = format!("{}:{}", str_field(c, "local_ip"), u64_field(c, "local_port"));
        let rem_host = preferred_remote(c);
        let rem_port = u64_field(c, "remote_port");
        let remote = if rem_port > 0 {
            format!("{rem_host}:{rem_port}")
        } else {
            rem_host.to_string()
        };
        let row = [
            cell(proc_label),
            cell(conn_proto(c)),
            cell(str_field(c, "state")),
            cell(&local),
            cell(&remote),
            cell(str_field(c, "service")),
            cell(str_field(c, "scope")),
        ];
        lines.push(format!("| {} |", row.join(" | ")));
    }
    lines.join("\n") + "\n"
}

/// Validate a WPA/WPA2/WPA3 PSK passphrase (8..63 UTF-8 bytes or 64 hex chars, no newlines,
/// no leading/trailing spaces).
pub fn wifi_password_valid(password: &str) -> bool {
    if password.contains('\n') || password.contains('\r') || password.trim() != password {
        return false;
    }
    let len = password.len();
    (8..=63).contains(&len) || (len == 64 && password.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filter_and_sort_connections() {
        let conns = vec![
            json!({
                "proto": "tcp",
                "state": "ESTABLISHED",
                "local_ip": "192.168.178.22",
                "local_port": 50000,
                "remote_ip": "1.1.1.1",
                "remote_port": 443,
                "remote_host": "one.one.one.one",
                "service": "HTTPS",
                "scope": "external",
                "pid": 100,
                "process": "firefox",
                "rx_rate": 5000.0,
                "tx_rate": 1000.0,
                "rx_bytes": 100000,
                "tx_bytes": 20000
            }),
            json!({
                "proto": "udp",
                "state": "UNCONN",
                "local_ip": "127.0.0.53",
                "local_port": 53,
                "remote_ip": "0.0.0.0",
                "remote_port": 0,
                "remote_host": "",
                "service": "DNS",
                "scope": "loopback",
                "pid": 200,
                "process": "systemd-resolved",
                "rx_rate": 0.0,
                "tx_rate": 0.0,
                "rx_bytes": 0,
                "tx_bytes": 0
            }),
            json!({
                "proto": "tcp",
                "state": "LISTEN",
                "local_ip": "0.0.0.0",
                "local_port": 22,
                "remote_ip": "0.0.0.0",
                "remote_port": 0,
                "remote_host": "",
                "service": "SSH",
                "scope": "loopback",
                "pid": 300,
                "process": "sshd",
                "rx_rate": 0.0,
                "tx_rate": 0.0,
                "rx_bytes": 0,
                "tx_bytes": 0
            }),
        ];

        assert_eq!(filter_connections(&conns, &NetMonFilters::default()), vec![0, 2, 1]);

        let tcp_only = NetMonFilters {
            proto: ProtoFilter::Tcp,
            ..Default::default()
        };
        assert_eq!(filter_connections(&conns, &tcp_only), vec![0, 2]);

        let listening = NetMonFilters {
            scope: ScopeFilter::Listening,
            ..Default::default()
        };
        assert_eq!(filter_connections(&conns, &listening), vec![2]);

        let external = NetMonFilters {
            scope: ScopeFilter::External,
            ..Default::default()
        };
        assert_eq!(filter_connections(&conns, &external), vec![0]);

        let search = NetMonFilters {
            search: "one.one".into(),
            ..Default::default()
        };
        assert_eq!(filter_connections(&conns, &search), vec![0]);
    }

    #[test]
    fn password_validation_matches_nmcli_rules() {
        assert!(!wifi_password_valid("short"));
        assert!(wifi_password_valid("12345678"));
        assert!(!wifi_password_valid(" 12345678"));
        assert!(!wifi_password_valid("12345678 "));
        assert!(!wifi_password_valid("1234\n5678"));
        assert!(!wifi_password_valid(&"g".repeat(64)));
        assert!(wifi_password_valid(&"a".repeat(64)));
        assert!(wifi_password_valid(&"a".repeat(63)));
    }
}
