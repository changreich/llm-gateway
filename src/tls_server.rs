//! 原生 Rustls TLS 服务器实现 - HTTP 代理模式
//!
//! 替代 Pingora 的 add_tls，在 443 端口终止 TLS，将请求转发到 9443 HTTP 端口
//! 避免 Pingora TLS 层的问题

use log::{error, info};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use pingora_error::{Error, ErrorType, Result};

/// 加载 PEM 格式的证书和私钥
pub fn load_certs_and_key(cert_path: &str, key_path: &str) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert_data = std::fs::read(cert_path)
        .map_err(|e| Error::explain(ErrorType::InternalError, format!("Failed to read cert: {}", e)))?;
    let key_data = std::fs::read(key_path)
        .map_err(|e| Error::explain(ErrorType::InternalError, format!("Failed to read key: {}", e)))?;

    // 解析证书链
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut std::io::Cursor::new(&cert_data))
        .filter_map(|r| r.ok())
        .map(|cert| cert.into_owned())
        .collect();

    if certs.is_empty() {
        return Err(Error::explain(ErrorType::InternalError, "No certificates found in cert file"));
    }

    // 尝试解析 PKCS8 格式私钥
    let key = match rustls_pemfile::private_key(&mut std::io::Cursor::new(&key_data)) {
        Ok(Some(k)) => k,
        Ok(None) => {
            return Err(Error::explain(ErrorType::InternalError, "No private key found in key file"));
        }
        Err(e) => {
            return Err(Error::explain(ErrorType::InternalError, format!("Failed to parse private key: {}", e)));
        }
    };

    Ok((certs, key))
}

/// 构建 Rustls ServerConfig (兼容 TLS 1.2 和 1.3)
pub fn build_server_config(cert_path: &str, key_path: &str) -> Result<Arc<ServerConfig>> {
    let (certs, key) = load_certs_and_key(cert_path, key_path)?;

    // 设置 ALPN 协议 (HTTP/2 和 HTTP/1.1)
    let mut alpn_protocols = vec![];
    alpn_protocols.push(b"h2".to_vec());
    alpn_protocols.push(b"http/1.1".to_vec());

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| Error::explain(ErrorType::InternalError, format!("Failed to build TLS config: {}", e)))?;

    let mut config = Arc::new(config);

    // 使用 Arc::get_mut 设置 ALPN
    if let Some(cfg) = Arc::get_mut(&mut config) {
        cfg.alpn_protocols = alpn_protocols;
    }

    Ok(config)
}

/// 启动 TLS 代理服务器
///
/// TLS 端口接收 HTTPS 请求，终止 TLS 后转发到 HTTP 后端
pub async fn start_tls_proxy(
    tls_listen: &str,
    cert_path: &str,
    key_path: &str,
    http_backend: &str,
) -> Result<()> {
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    // 加载证书构建 TLS 配置
    let tls_config = build_server_config(cert_path, key_path)?;
    let acceptor = TlsAcceptor::from(tls_config);

    // 绑定 TLS 端口
    let listener = TcpListener::bind(tls_listen).await
        .map_err(|e| Error::explain(ErrorType::InternalError, format!("Failed to bind TLS port {}: {}", tls_listen, e)))?;

    info!("TLS proxy listening on {} -> {}", tls_listen, http_backend);

    // 获取后端地址的副本
    let backend_addr = http_backend.to_string();

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let peer_addr = peer.to_string();
                let acceptor = acceptor.clone();
                let backend = backend_addr.clone();

                tokio::spawn(async move {
                    handle_tls_connection(stream, acceptor, &backend, peer_addr).await;
                });
            }
            Err(e) => {
                error!("Failed to accept TLS connection: {}", e);
            }
        }
    }
}

/// 处理单个 TLS 连接
async fn handle_tls_connection(
    stream: TcpStream,
    acceptor: tokio_rustls::TlsAcceptor,
    backend: &str,
    peer_addr: String,
) {
    // 执行 TLS 握手
    let tls_stream = match acceptor.accept(stream).await {
        Ok(s) => s,
        Err(e) => {
            error!("TLS handshake failed from {}: {}", peer_addr, e);
            return;
        }
    };

    info!("TLS connection established from {}", peer_addr);

    // 连接到后端 HTTP 服务
    let mut backend_stream = match TcpStream::connect(backend).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to connect to backend {}: {}", backend, e);
            return;
        }
    };

    // 双向转发数据
    if let Err(e) = forward_data(tls_stream, &mut backend_stream).await {
        error!("Proxy error for {}: {}", peer_addr, e);
    } else {
        info!("Proxy session ended for {}", peer_addr);
    }
}

/// 双向转发数据
async fn forward_data(
    client: impl AsyncRead + AsyncWrite + Unpin + Send,
    backend: &mut TcpStream,
) -> std::io::Result<()> {
    // 使用 split 模式进行双向转发
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let (mut backend_read, mut backend_write) = tokio::io::split(backend);

    // 并发转发两个方向
    let c2b = tokio::io::copy(&mut client_read, &mut backend_write);
    let b2c = tokio::io::copy(&mut backend_read, &mut client_write);

    // 任一方向完成就结束
    tokio::select! {
        result = c2b => {
            if let Err(e) = result {
                return Err(e);
            }
        }
        result = b2c => {
            if let Err(e) = result {
                return Err(e);
            }
        }
    }

    // 尝试优雅关闭
    let _ = client_write.shutdown().await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_certs() {
        // 需要实际证书文件才能测试
    }
}
