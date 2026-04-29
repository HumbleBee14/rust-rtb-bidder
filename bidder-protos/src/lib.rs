pub mod events {
    include!(concat!(env!("OUT_DIR"), "/bidder.events.v1.rs"));
}

pub mod adx {
    include!(concat!(env!("OUT_DIR"), "/bidder.adx.v1.rs"));
}
