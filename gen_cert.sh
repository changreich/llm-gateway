#!/bin/bash
# 生成自签名 TLS 证书用于测试
#
# 使用方法:
#   ./gen_cert.sh [输出目录]
#
# 输出:
#   cert.pem - 证书文件
#   key.pem  - 私钥文件
#
# 配置方式 (二选一):
#   1. config.lua:
#      tls = {
#          enabled = true,
#          cert = "certs/cert.pem",
#          key = "certs/key.pem",
#          https_listen = "0.0.0.0:9443",
#      }
#
#   2. 环境变量:
#      LLM_TLS_ENABLED=true
#      LLM_TLS_CERT="certs/cert.pem"
#      LLM_TLS_KEY="certs/key.pem"
#      LLM_HTTPS_LISTEN="0.0.0.0:9443"

OUTPUT_DIR="${1:-./certs}"
DAYS="${CERT_DAYS:-365}"
DOMAIN="${CERT_DOMAIN:-localhost}"

mkdir -p "$OUTPUT_DIR"

echo "Generating self-signed certificate for $DOMAIN..."
echo "Output directory: $OUTPUT_DIR"
echo "Valid for $DAYS days"

# 使用 OpenSSL 生成自签名证书
openssl req -x509 -newkey rsa:2048 -keyout "$OUTPUT_DIR/key.pem" -out "$OUTPUT_DIR/cert.pem" \
    -days "$DAYS" -nodes \
    -subj "/CN=$DOMAIN/O=LLM Gateway/OU=Test/C=US" \
    -addext "subjectAltName=DNS:$DOMAIN,DNS:localhost,IP:127.0.0.1"

if [ $? -eq 0 ]; then
    echo ""
    echo "Certificate generated successfully!"
    echo ""
    echo "Files:"
    echo "  Certificate: $OUTPUT_DIR/cert.pem"
    echo "  Private Key: $OUTPUT_DIR/key.pem"
    echo ""
    echo "To use with LLM Gateway, add to config.lua:"
    echo "  tls = {"
    echo "      enabled = true,"
    echo "      cert = \"$OUTPUT_DIR/cert.pem\","
    echo "      key = \"$OUTPUT_DIR/key.pem\","
    echo "      https_listen = \"0.0.0.0:9443\","
    echo "  }"
    echo ""
    echo "Or set environment variables:"
    echo "  export LLM_TLS_ENABLED=true"
    echo "  export LLM_TLS_CERT=\"$OUTPUT_DIR/cert.pem\""
    echo "  export LLM_TLS_KEY=\"$OUTPUT_DIR/key.pem\""
    echo "  export LLM_HTTPS_LISTEN=\"0.0.0.0:9443\""
else
    echo "Failed to generate certificate. Make sure OpenSSL is installed."
    exit 1
fi
