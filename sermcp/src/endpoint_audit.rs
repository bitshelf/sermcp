//! Diagnostic audit of competing TCP clients in the current network namespace.
//! This snapshot cannot establish global exclusivity or prove serial log loss.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

fn parse_address(raw: &str) -> Option<SocketAddr> {
    let (ip, port) = raw.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let ip = match ip.len() {
        8 => IpAddr::V4(Ipv4Addr::from(
            u32::from_str_radix(ip, 16).ok()?.to_ne_bytes(),
        )),
        32 => {
            let mut bytes = [0; 16];
            for (i, chunk) in bytes.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                chunk.copy_from_slice(
                    &u32::from_str_radix(&ip[i * 8..i * 8 + 8], 16)
                        .ok()?
                        .to_ne_bytes(),
                );
            }
            IpAddr::V6(Ipv6Addr::from(bytes)).to_canonical()
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

fn competing_inodes(table: &str, peer: SocketAddr, own_inode: u64) -> Vec<u64> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.get(3) != Some(&"01") || parse_address(fields.get(2)?)? != peer {
                return None;
            }
            let inode = fields.get(9)?.parse().ok()?;
            (inode != own_inode).then_some(inode)
        })
        .collect()
}

/// Return other established client socket inodes, excluding this socket.
#[cfg(target_os = "linux")]
pub fn audit(stream: &tokio::net::TcpStream) -> std::io::Result<Vec<u64>> {
    use std::os::fd::AsRawFd;
    let peer = stream.peer_addr()?;
    let peer = SocketAddr::new(peer.ip().to_canonical(), peer.port());
    let link = std::fs::read_link(format!("/proc/self/fd/{}", stream.as_raw_fd()))?;
    let text = link.to_string_lossy();
    let inode = text
        .strip_prefix("socket:[")
        .and_then(|s| s.strip_suffix(']'))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other("cannot identify console socket inode"))?;
    let mut others = Vec::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        match std::fs::read_to_string(path) {
            Ok(table) => others.extend(competing_inodes(&table, peer, inode)),
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && path.ends_with('6')
                    && peer.is_ipv4() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(others)
}

#[cfg(not(target_os = "linux"))]
pub fn audit(_stream: &tokio::net::TcpStream) -> std::io::Result<Vec<u64>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "TCP endpoint audit requires Linux procfs",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_filters_state_peer_and_own_socket() {
        let table = "header\n0: 0100007F:1234 0100007F:07D5 01 0 0 0 0 0 100\n1: 0100007F:1235 0100007F:07D5 01 0 0 0 0 0 101\n2: 0100007F:1236 0100007F:07D5 06 0 0 0 0 0 102\n3: 0100007F:1237 0100007F:07D6 01 0 0 0 0 0 103\nmalformed";
        assert_eq!(
            competing_inodes(table, "127.0.0.1:2005".parse().unwrap(), 100),
            vec![101]
        );
        assert_eq!(
            parse_address("0000000000000000FFFF00000100007F:07D5"),
            Some("127.0.0.1:2005".parse().unwrap())
        );
        assert_eq!(
            parse_address("00000000000000000000000001000000:07D5"),
            Some("[::1]:2005".parse().unwrap())
        );
    }

    #[tokio::test]
    async fn live_audit_excludes_self_and_detects_second_client() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let first = tokio::net::TcpStream::connect(address).await.unwrap();
        let (_server_first, _) = listener.accept().await.unwrap();
        assert!(audit(&first).unwrap().is_empty());
        let second = tokio::net::TcpStream::connect(address).await.unwrap();
        let (_server_second, _) = listener.accept().await.unwrap();
        assert_eq!(audit(&first).unwrap().len(), 1);
        assert_eq!(audit(&second).unwrap().len(), 1);
    }
}

#[derive(Debug)]
pub struct AuditFailure {
    pub code: &'static str,
    pub detail: String,
}

impl std::fmt::Display for AuditFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}
impl std::error::Error for AuditFailure {}
