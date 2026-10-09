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
