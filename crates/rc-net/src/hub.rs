use crate::http::{serve, serve_tls, Handler, Limits};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub fn serve_loop(
    listener: TcpListener,
    handler: &mut dyn Handler,
    stop: &AtomicBool,
    limits: Limits,
    max_bps: u64,
) {
    serve(listener, handler, stop, limits, max_bps);
}

/// 同一个 accept 循环，每条连接先握手再读请求。握手失败只关这一条连接
/// （`http::serve_tls` 的判定），一次错指纹不是服务停机。
pub fn serve_loop_tls(
    listener: TcpListener,
    handler: &mut dyn Handler,
    stop: &AtomicBool,
    limits: Limits,
    max_bps: u64,
    config: Arc<rustls::ServerConfig>,
) {
    serve_tls(listener, handler, stop, limits, max_bps, config);
}
