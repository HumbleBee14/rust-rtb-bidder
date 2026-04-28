use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

// ── BidRequest ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BidRequest {
    pub id: String,
    #[serde(default)]
    pub imp: Vec<Imp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<Site>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<App>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dooh: Option<Dooh>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<Device>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<User>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<AuctionType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmax: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wseat: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bseat: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cur: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wlang: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bcat: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub badv: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bapp: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regs: Option<Regs>,
    #[serde(default)]
    pub test: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuctionType(pub u8);

impl AuctionType {
    pub const FIRST_PRICE: Self = Self(1);
    pub const SECOND_PRICE: Self = Self(2);
    pub const FIXED_PRICE: Self = Self(3);
}

// ── Imp ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Imp {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub banner: Option<Banner>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<Video>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<Audio>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<Native>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pmp: Option<Pmp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub displaymanager: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub displaymanagerver: Option<String>,
    #[serde(default)]
    pub instl: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tagid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bidfloor: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bidfloorcur: Option<String>,
    #[serde(default)]
    pub clickbrowser: u8,
    #[serde(default)]
    pub secure: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iframebuster: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rwdd: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssai: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

// ── Banner ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Banner {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub format: Vec<Format>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub h: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wmax: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hmax: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wmin: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hmin: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub btype: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub battr: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pos: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mimes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topframe: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expdir: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcm: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Format {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub h: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wratio: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hratio: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wmin: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

// ── Video ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Video {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mimes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minduration: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maxduration: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protocols: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub h: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startdelay: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plcmt: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linearity: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipmin: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipafter: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub battr: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maxextended: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minbitrate: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maxbitrate: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boxingallowed: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub playbackmethod: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub playbackend: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delivery: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pos: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub companionad: Vec<Banner>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub companiontype: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

// ── Audio ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Audio {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mimes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minduration: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maxduration: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protocols: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startdelay: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub battr: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maxextended: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minbitrate: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maxbitrate: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delivery: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub companionad: Vec<Banner>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub companiontype: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maxseq: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feed: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stitched: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nvol: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

// ── Native ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Native {
    pub request: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ver: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub battr: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

// ── Pmp ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pmp {
    #[serde(default)]
    pub private_auction: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deals: Vec<Deal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deal {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bidfloor: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bidfloorcur: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<AuctionType>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wseat: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wadomain: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

// ── Site / App / Dooh ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Site {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cat: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sectioncat: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pagecat: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mobile: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub privacypolicy: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<Publisher>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct App {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storeurl: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cat: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sectioncat: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pagecat: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub privacypolicy: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paid: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<Publisher>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dooh {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub venuetype: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub venuetypetax: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<Publisher>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Publisher {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cat: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Content {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer: Option<Producer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cat: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prodq: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub videoquality: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contentrating: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub userrating: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qagmediarating: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub livestream: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sourcerelationship: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub len: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embeddable: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Vec<Data>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Producer {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cat: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

// ── Device ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ua: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geo: Option<Geo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dnt: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lmt: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub devicetype: Option<DeviceType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub make: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub osv: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hwv: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub h: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ppi: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pxratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub js: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geofetch: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flashver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carrier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mccmnc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connectiontype: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ifa: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub didsha1: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub didmd5: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dpidsha1: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dpidmd5: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub macsha1: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub macmd5: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceType(pub u8);

impl DeviceType {
    pub const MOBILE: Self = Self(1);
    pub const PC: Self = Self(2);
    pub const TV: Self = Self(3);
    pub const PHONE: Self = Self(4);
    pub const TABLET: Self = Self(5);
    pub const CONNECTED_DEVICE: Self = Self(6);
    pub const SET_TOP_BOX: Self = Self(7);
    pub const OOH: Self = Self(8);
}

// ── Geo ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Geo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accuracy: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lastfix: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipservice: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regionfips104: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metro: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utcoffset: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

// ── User ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buyeruid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yob: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gender: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub customdata: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geo: Option<Geo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data: Vec<Data>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eids: Option<Vec<Eid>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Data {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segment: Vec<Segment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Eid {
    pub source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uids: Vec<Uid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Uid {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atype: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

// ── Source / Regs ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fd: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pchain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schain: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Regs {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coppa: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gdpr: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub us_privacy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpp: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpp_sid: Vec<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

// ── BidResponse ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BidResponse {
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seatbid: Vec<SeatBid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bidid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cur: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub customdata: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nbr: Option<NoBidReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

impl BidResponse {
    pub fn no_bid(request_id: String, reason: NoBidReason) -> Self {
        Self {
            id: request_id,
            seatbid: vec![],
            bidid: None,
            cur: None,
            customdata: None,
            nbr: Some(reason),
            ext: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatBid {
    pub bid: Vec<Bid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seat: Option<String>,
    #[serde(default)]
    pub group: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bid {
    pub id: String,
    pub impid: String,
    pub price: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nurl: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub burl: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lurl: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adomain: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iurl: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tactic: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cat: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attr: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qagmediarating: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dealid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub h: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wratio: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hratio: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Value>,
}

// ── NoBidReason ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NoBidReason(pub u32);

impl NoBidReason {
    pub const UNKNOWN_ERROR: Self = Self(0);
    pub const TECHNICAL_ERROR: Self = Self(1);
    pub const INVALID_REQUEST: Self = Self(2);
    pub const KNOWN_WEB_SPIDER: Self = Self(3);
    pub const SUSPECTED_NON_HUMAN: Self = Self(4);
    pub const CLOUD_DATA_CENTER: Self = Self(5);
    pub const UNSUPPORTED_DEVICE: Self = Self(6);
    pub const BLOCKED_PUBLISHER: Self = Self(7);
    pub const UNMATCHED_USER: Self = Self(8);
    pub const DAILY_REACH_LIMIT: Self = Self(9);
    pub const BUSINESS_RULES: Self = Self(10);
    pub const NO_ELIGIBLE_BIDS: Self = Self(11);
    pub const BELOW_FLOOR: Self = Self(12);
    pub const REQUEST_BLOCKED_PRIVACY: Self = Self(13);
    pub const FREQUENCY_CAP: Self = Self(14);
    pub const BELOW_BID_FLOOR_PMP: Self = Self(15);
    pub const LOST_TO_HIGHER_BID: Self = Self(100);
    pub const LOST_TO_PMP_BID: Self = Self(101);
    pub const SEAT_BLOCKED: Self = Self(102);
    pub const PIPELINE_DEADLINE: Self = Self(103);
}

// ── AdFormat ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AdFormat {
    Banner,
    Video,
    Audio,
    Native,
}

impl AdFormat {
    pub fn from_imp(imp: &Imp) -> Option<Self> {
        if imp.banner.is_some() {
            Some(Self::Banner)
        } else if imp.video.is_some() {
            Some(Self::Video)
        } else if imp.audio.is_some() {
            Some(Self::Audio)
        } else if imp.native.is_some() {
            Some(Self::Native)
        } else {
            None
        }
    }
}

// ── AdEvent ──────────────────────────────────────────────────────────────────
// Sum type defined here even though Phase 5 publishes events.
// Locks in the schema so Phase 5 has no model changes to make.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum AdEvent {
    Bid(BidEvent),
    Win(WinEvent),
    Impression(ImpressionEvent),
    Click(ClickEvent),
    VideoQuartile(VideoQuartileEvent),
    Conversion(ConversionEvent),
    ViewabilityMeasured(ViewabilityEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BidEvent {
    pub request_id: String,
    pub imp_id: String,
    pub campaign_id: String,
    pub bid_price: f64,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinEvent {
    pub request_id: String,
    pub imp_id: String,
    pub campaign_id: String,
    pub clearing_price: f64,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpressionEvent {
    pub request_id: String,
    pub imp_id: String,
    pub campaign_id: String,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClickEvent {
    pub request_id: String,
    pub imp_id: String,
    pub campaign_id: String,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoQuartileEvent {
    pub request_id: String,
    pub imp_id: String,
    pub campaign_id: String,
    pub quartile: u8,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversionEvent {
    pub request_id: String,
    pub imp_id: String,
    pub campaign_id: String,
    pub conversion_type: String,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewabilityEvent {
    pub request_id: String,
    pub imp_id: String,
    pub campaign_id: String,
    pub viewable_fraction: f64,
    pub timestamp_ms: u64,
}

// ── misc ─────────────────────────────────────────────────────────────────────

pub type ExtMap = HashMap<String, Value>;
