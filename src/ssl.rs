//! TLS/SSL 证书管理模块
//!
//! 支持 PEM 格式证书和私钥加载，用于 HTTPS 服务
//!
//! 配置来源 (优先级从高到低):
//! 1. 环境变量: LLM_TLS_ENABLED, LLM_TLS_CERT, LLM_TLS_KEY, LLM_HTTPS_LISTEN
//! 2. config.lua: tls.enabled, tls.cert, tls.key, tls.https_listen

use pingora_error::{Error, ErrorType, Result};
use std::path::{Path, PathBuf};

/// TLS 证书配置
#[derive(Clone, Debug)]
pub struct TlsConfig {
    /// 证书文件路径 (PEM 格式)
    pub cert_path: PathBuf,
    /// 私钥文件路径 (PEM 格式)
    pub key_path: PathBuf,
}

impl TlsConfig {
    /// 从指定路径创建 TLS 配置
    pub fn new<P: AsRef<Path>>(cert_path: P, key_path: P) -> Self {
        Self {
            cert_path: cert_path.as_ref().to_path_buf(),
            key_path: key_path.as_ref().to_path_buf(),
        }
    }

    /// 验证证书和私钥文件是否存在
    pub fn validate(&self) -> Result<()> {
        if !self.cert_path.exists() {
            return Err(Error::explain(
                ErrorType::InternalError,
                format!("Certificate file not found: {:?}", self.cert_path),
            ));
        }
        if !self.key_path.exists() {
            return Err(Error::explain(
                ErrorType::InternalError,
                format!("Private key file not found: {:?}", self.key_path),
            ));
        }
        Ok(())
    }
}

/// TLS 管理器
pub struct SslManager {
    config: TlsConfig,
}

impl SslManager {
    /// 创建新的 SSL 管理器
    pub fn new(config: TlsConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self { config })
    }

    /// 获取证书路径
    pub fn cert_path(&self) -> &Path {
        &self.config.cert_path
    }

    /// 获取私钥路径
    pub fn key_path(&self) -> &Path {
        &self.config.key_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tls_config_validate() {
        let config = TlsConfig::new("/nonexistent/cert.pem", "/nonexistent/key.pem");
        assert!(config.validate().is_err());
    }
}
