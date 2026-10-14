use crate::voice::IceServer;
use std::{collections::HashMap, net::IpAddr, sync::Arc};
use tokio::sync::{Mutex, oneshot, watch};
use tokio::time::{Duration, timeout};
use tokio::{net::TcpListener, task::JoinHandle};

const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Tunnels TURN TCP connections through the selected HTTP/SOCKS proxy.
///
/// WebRTC itself does not understand HTTP CONNECT or SOCKS proxies for ICE.
/// The legacy client solved that by presenting loopback TURN endpoints to the
/// ICE engine and forwarding each local TCP connection through the proxy.
pub struct TurnTcpForwarder {
    tasks: Vec<JoinHandle<()>>,
    shutdown: watch::Sender<bool>,
}

impl TurnTcpForwarder {
    pub async fn prepare(
        servers: &[IceServer],
        proxy: Option<&str>,
    ) -> (Vec<IceServer>, Option<Self>) {
        let Some(proxy) = proxy.filter(|value| !value.trim().is_empty()) else {
            return (servers.to_vec(), None);
        };

        let mut listeners = HashMap::<String, (u16, Arc<ForwardedEndpoint>)>::new();
        let mut tasks = Vec::new();
        let (shutdown, _) = watch::channel(false);
        let mut wrapped = Vec::with_capacity(servers.len());
        for server in servers {
            let mut urls = Vec::with_capacity(server.urls.len());
            for url in &server.urls {
                let Some(target) = parse_turn_target(url) else {
                    urls.push(url.clone());
                    continue;
                };
                if target.is_loopback() {
                    urls.push(url.clone());
                    continue;
                }
                if target.is_ipv6() {
                    continue;
                }

                let key = format!("{}:{}", target.host, target.port);
                let (local_port, _) = if let Some((port, endpoint)) = listeners.get(&key) {
                    (*port, Arc::clone(endpoint))
                } else {
                    let Ok(listener) = TcpListener::bind(("127.0.0.1", 0)).await else {
                        urls.push(url.clone());
                        continue;
                    };
                    let Ok(local_port) = listener.local_addr().map(|addr| addr.port()) else {
                        urls.push(url.clone());
                        continue;
                    };
                    let proxy = proxy.to_owned();
                    let host = target.host.clone();
                    let port = target.port;
                    let endpoint = Arc::new(ForwardedEndpoint::new());
                    let (preconnect_tx, preconnect_rx) = oneshot::channel();
                    endpoint.set_preconnect(preconnect_rx).await;
                    let warmup_proxy = proxy.clone();
                    let warmup_host = host.clone();
                    tasks.push(tokio::spawn(async move {
                        let result = timeout(
                            DIAL_TIMEOUT,
                            crate::transport::open_tcp_tunnel(
                                Some(&warmup_proxy),
                                &warmup_host,
                                port,
                            ),
                        )
                        .await
                        .ok()
                        .and_then(Result::ok);
                        let _ = preconnect_tx.send(result);
                    }));
                    let listener_shutdown = shutdown.subscribe();
                    let listener_endpoint = Arc::clone(&endpoint);
                    tasks.push(tokio::spawn(async move {
                        accept_loop(
                            listener,
                            proxy,
                            host,
                            port,
                            listener_endpoint,
                            listener_shutdown,
                        )
                        .await;
                    }));
                    listeners.insert(key, (local_port, endpoint.clone()));
                    (local_port, endpoint)
                };
                urls.push(if target.tls {
                    format!("turns:127.0.0.1:{local_port}")
                } else {
                    "turn:127.0.0.1:".to_owned() + &local_port.to_string() + "?transport=tcp"
                });
            }
            if !urls.is_empty() {
                wrapped.push(IceServer {
                    urls,
                    username: server.username.clone(),
                    credential: server.credential.clone(),
                });
            }
        }

        if tasks.is_empty() {
            (wrapped, None)
        } else {
            (wrapped, Some(Self { tasks, shutdown }))
        }
    }
}

impl Drop for TurnTcpForwarder {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

struct ForwardedEndpoint {
    preconnect: Mutex<Option<oneshot::Receiver<Option<tokio::net::TcpStream>>>>,
}

impl ForwardedEndpoint {
    fn new() -> Self {
        Self {
            preconnect: Mutex::new(None),
        }
    }

    async fn set_preconnect(&self, receiver: oneshot::Receiver<Option<tokio::net::TcpStream>>) {
        *self.preconnect.lock().await = Some(receiver);
    }

    async fn claim_preconnect(&self) -> Option<tokio::net::TcpStream> {
        let receiver = self.preconnect.lock().await.take()?;
        receiver.await.ok().flatten()
    }
}

async fn accept_loop(
    listener: TcpListener,
    proxy: String,
    host: String,
    port: u16,
    endpoint: Arc<ForwardedEndpoint>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let accepted = tokio::select! {
            _ = shutdown.changed() => return,
            result = listener.accept() => result,
        };
        let Ok((client, _)) = accepted else { return };
        let proxy = proxy.clone();
        let host = host.clone();
        let endpoint = Arc::clone(&endpoint);
        let mut client_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let upstream = match endpoint.claim_preconnect().await {
                Some(stream) => Some(stream),
                None => timeout(
                    DIAL_TIMEOUT,
                    crate::transport::open_tcp_tunnel(Some(&proxy), &host, port),
                )
                .await
                .ok()
                .and_then(Result::ok),
            };
            if let Some(upstream) = upstream {
                let mut client = client;
                let mut upstream = upstream;
                tokio::select! {
                    _ = client_shutdown.changed() => {}
                    _ = tokio::io::copy_bidirectional(&mut client, &mut upstream) => {}
                }
            }
        });
    }
}

#[derive(Clone)]
struct TurnTarget {
    host: String,
    port: u16,
    tls: bool,
}

impl TurnTarget {
    fn is_loopback(&self) -> bool {
        self.host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
            || self.host.eq_ignore_ascii_case("localhost")
    }

    fn is_ipv6(&self) -> bool {
        self.host.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv6()) || self.host.starts_with('[')
    }
}

fn parse_turn_target(raw: &str) -> Option<TurnTarget> {
    let (scheme, remainder) = raw.trim().split_once(':')?;
    let tls = match scheme.to_ascii_lowercase().as_str() {
        "turn" => false,
        "turns" => true,
        _ => return None,
    };
    let authority = remainder
        .split_once('?')
        .map_or(remainder, |(value, _)| value);
    if authority.is_empty() {
        return None;
    }
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let (host, rest) = stripped.split_once(']')?;
        let port = rest.strip_prefix(':').and_then(|value| value.parse().ok());
        (
            host.to_owned(),
            port.unwrap_or(if tls { 5349 } else { 3478 }),
        )
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if let Ok(port) = port.parse() {
            (host.to_owned(), port)
        } else {
            (authority.to_owned(), if tls { 5349 } else { 3478 })
        }
    } else {
        (authority.to_owned(), if tls { 5349 } else { 3478 })
    };
    Some(TurnTarget { host, port, tls })
}

#[cfg(test)]
mod tests {
    use super::{TurnTcpForwarder, parse_turn_target};
    use crate::voice::IceServer;

    #[test]
    fn parses_turn_defaults_and_transport_query() {
        let target = parse_turn_target("turn:relay.example?transport=tcp").unwrap();
        assert_eq!(target.host, "relay.example");
        assert_eq!(target.port, 3478);
        assert!(!target.tls);
    }

    #[test]
    fn parses_turns_and_bracketed_ipv6() {
        let target = parse_turn_target("turns:[2001:db8::1]:443").unwrap();
        assert_eq!(target.host, "2001:db8::1");
        assert_eq!(target.port, 443);
        assert!(target.tls);
        assert!(target.is_ipv6());
    }

    #[test]
    fn ignores_non_turn_servers() {
        assert!(parse_turn_target("stun:stun.example:3478").is_none());
    }

    #[tokio::test]
    async fn proxy_preparation_wraps_turn_and_filters_ipv6() {
        let servers = vec![
            IceServer {
                urls: vec![
                    "turn:relay.example:3478".into(),
                    "turn:[2001:db8::1]:3478".into(),
                    "stun:stun.example:3478".into(),
                ],
                username: Some("user".into()),
                credential: Some("secret".into()),
            },
            IceServer {
                urls: vec!["turn:[2001:db8::2]:3478".into()],
                username: None,
                credential: None,
            },
        ];
        let (wrapped, forwarder) =
            TurnTcpForwarder::prepare(&servers, Some("proxy.example:8080")).await;
        assert_eq!(wrapped.len(), 1);
        assert_eq!(wrapped[0].urls.len(), 2);
        assert!(
            wrapped[0]
                .urls
                .iter()
                .any(|url| url.starts_with("turn:127.0.0.1:"))
        );
        assert!(wrapped[0].urls.iter().any(|url| url.starts_with("stun:")));
        assert_eq!(wrapped[0].username.as_deref(), Some("user"));
        assert!(forwarder.is_some());
    }
}
