// `failure` 的 #[derive(Fail)] 会把 impl 生成到匿名常量里，触发 non_local_definitions；
// failure 已弃维护、无法改宏本身，这里整体压制该 lint。
#![allow(non_local_definitions)]

pub mod errors;
// pub mod http;
pub mod session;
pub mod webrtc;
pub mod whep;
pub mod whip;
pub mod opus2aac;
pub mod rtp_queue;
