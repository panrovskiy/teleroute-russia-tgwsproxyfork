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
        2 => "149.154.167.50",
        3 => "149.154.175.100",
        4 => "149.154.167.92",
        5 => "91.108.56.100",
        _ => return None,
    };
    Some(ip.parse().unwrap())
}

pub fn classify_ip(ip: IpAddr) -> Option<u16> {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if o[0] == 149 && o[1] == 154 {
                if o[2] == 167 { return Some(2); }
                if o[2] == 175 { return Some(1); }
            }
            if o[0] == 91 && o[1] == 108 { return Some(5); }
            None
        }
        IpAddr::V6(_) => None,
    }
}

pub fn is_probable_telegram_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h.ends_with(".telegram.org") || h.ends_with(".t.me") || h.contains("telegram")
}
