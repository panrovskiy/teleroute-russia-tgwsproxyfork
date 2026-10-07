use crate::telegram::dc;
use std::net::IpAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind { TelegramWss, TelegramTcp, Direct, TelegramUdpDirect, TelegramUdpRelay }

#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub kind: RouteKind,
    pub dc: Option<u16>,
    pub media: bool,
}

pub fn decide_tcp(host: &str, ip: Option<IpAddr>, prefer_wss: bool) -> RouteDecision {
    if dc::is_probable_telegram_host(host) || ip.and_then(dc::classify_ip).is_some() {
        let dc_id = ip.and_then(dc::classify_ip);
        return RouteDecision { kind: if prefer_wss { RouteKind::TelegramWss } else { RouteKind::TelegramTcp }, dc: dc_id, media: false };
    }
    RouteDecision { kind: RouteKind::Direct, dc: None, media: false }
}

pub fn decide_udp(is_telegram_process: bool, relay_available: bool, direct_allowed: bool) -> RouteKind {
    if !is_telegram_process { return RouteKind::Direct; }
    if relay_available { return RouteKind::TelegramUdpRelay; }
    if direct_allowed { return RouteKind::TelegramUdpDirect; }
    RouteKind::Direct
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn telegram_domain_prefers_wss() {
        let d = decide_tcp("venus.web.telegram.org", None, true);
        assert_eq!(d.kind, RouteKind::TelegramWss);
    }

    #[test]
    fn udp_prefers_relay_when_present() {
        assert_eq!(decide_udp(true, true, true), RouteKind::TelegramUdpRelay);
    }
}
