use std::net::IpAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DcInfo {
    pub id: u16,
    pub media: bool,
}

impl DcInfo {
    pub fn new(id: u16, media: bool) -> Self { Self { id, media } }
}

pub fn name(id: u16) -> &'static str {
    match id { 1 => "pluto", 2 => "venus", 3 => "aurora", 4 => "vesta", 5 => "flora", _ => "dc" }
}

pub fn default_ipv4(id: u16) -> Option<IpAddr> {
    let ip = match id {
        1 => "149.154.175.50",
        2 => "149.154.167.51",
        3 => "149.154.175.100",
        4 => "149.154.167.91",
        5 => "149.154.171.5",
        _ => return None,
    };
    Some(ip.parse().unwrap())
}

pub fn test_ipv4(id: u16) -> Option<IpAddr> {
    let ip = match id {
        1 => "149.154.175.10",
        2 => "149.154.167.40",
        3 => "149.154.175.117",
        _ => return None,
    };
    Some(ip.parse().expect("constant IPv4 address"))
}

/// WSS bridge targets from Flowseal's default DC redirect configuration.
/// The known-good redirect is only for DC4. Redirecting DC2 to the same
/// IP can make media transfers fail for some Telegram accounts; DC2 should
/// use its own default address instead.
pub fn websocket_target_ipv4(id: u16) -> Option<IpAddr> {
    match id {
        4 => Some("149.154.167.220".parse().expect("constant IPv4 address")),
        _ => default_ipv4(id),
    }
}

pub fn websocket_target_ipv4_for(id: u16, test_dc: bool) -> Option<IpAddr> {
    if test_dc { test_ipv4(id) } else { websocket_target_ipv4(id) }
}

pub fn classify_ip(ip: IpAddr) -> Option<u16> {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            match o {
                [149, 154, 167, 51] => Some(2),
                [149, 154, 175, 50] => Some(1),
                [149, 154, 175, 100] => Some(3),
                [149, 154, 167, 91] => Some(4),
                [149, 154, 171, 5] => Some(5),
                _ => None,
            }
        }
        IpAddr::V6(_) => None,
    }
}

pub fn is_probable_telegram_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h.ends_with(".telegram.org") || h.ends_with(".t.me") || h.contains("telegram")
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_dc4_uses_the_redirect_ip() {
        assert_eq!(
            websocket_target_ipv4(2),
            Some("149.154.167.51".parse().unwrap())
        );
        assert_eq!(
            websocket_target_ipv4(4),
            Some("149.154.167.220".parse().unwrap())
        );
        assert_eq!(
            websocket_target_ipv4(3),
            Some("149.154.175.100".parse().unwrap())
        );
    }

    #[test]
    fn test_dc_address_override_still_wins() {
        assert_eq!(
            websocket_target_ipv4_for(2, true),
            Some("149.154.167.40".parse().unwrap())
        );
    }
}
