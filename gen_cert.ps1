# 生成自签名 TLS 证书用于测试
#
# 使用方法:
#   ./gen_cert.ps1 [-OutputDir <目录>]
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

param(
    [string]$OutputDir = ".\certs",
    [int]$Days = 365,
    [string]$Domain = "localhost"
)

# 创建输出目录
if (-not (Test-Path $OutputDir)) {
    New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null
}

$certPath = Join-Path $OutputDir "cert.pem"
$keyPath = Join-Path $OutputDir "key.pem"

Write-Host "Generating self-signed certificate for $Domain..."
Write-Host "Output directory: $OutputDir"
Write-Host "Valid for $Days days"

# 检查 OpenSSL 是否可用
$openssl = Get-Command openssl -ErrorAction SilentlyContinue

if ($openssl) {
    # 使用 OpenSSL 生成证书
    $subject = "/CN=$Domain/O=LLM Gateway/OU=Test/C=US"
    $san = "subjectAltName=DNS:$Domain,DNS:localhost,IP:127.0.0.1"

    & openssl req -x509 -newkey rsa:2048 -keyout $keyPath -out $certPath `
        -days $Days -nodes `
        -subj $subject `
        -addext $san

    if ($LASTEXITCODE -eq 0) {
        Write-Host ""
        Write-Host "Certificate generated successfully!" -ForegroundColor Green
    } else {
        Write-Host "OpenSSL failed, trying PowerShell..." -ForegroundColor Yellow
        $openssl = $null
    }
}

if (-not $openssl) {
    # 使用 PowerShell 生成自签名证书
    Write-Host "Using PowerShell to generate certificate..."

    $cert = New-SelfSignedCertificate `
        -DnsName @($Domain, "localhost") `
        -CertStoreLocation "Cert:\CurrentUser\My" `
        -FriendlyName "LLM Gateway Test Certificate" `
        -NotAfter (Get-Date).AddDays($Days)

    # 导出证书和私钥
    $certBytes = $cert.Export([System.Security.Cryptography.X509Certificates.X509ContentType]::Cert)
    [System.IO.File]::WriteAllBytes($certPath, $certBytes)

    # 导出私钥 (需要转换为 PEM 格式)
    $rsa = [System.Security.Cryptography.X509Certificates.RSACertificateExtensions]::GetRSAPrivateKey($cert)
    $rsaParams = $rsa.ExportParameters($true)

    # 使用 .NET 导出 PKCS#8 私钥
    $pkcs8 = New-Object System.Security.Cryptography.Pkcs.Pkcs8PrivateKeyInfo($rsaParams)
    $keyBytes = $pkcs8.Encode()

    # 转换为 PEM 格式
    $keyPem = "-----BEGIN PRIVATE KEY-----`n"
    $keyPem += [System.Convert]::ToBase64String($keyBytes, [System.Base64FormattingOptions]::InsertLineBreaks)
    $keyPem += "`n-----END PRIVATE KEY-----"
    [System.IO.File]::WriteAllText($keyPath, $keyPem)

    # 从证书存储中删除临时证书
    Remove-Item "Cert:\CurrentUser\My\$($cert.Thumbprint)" -Force

    Write-Host ""
    Write-Host "Certificate generated successfully!" -ForegroundColor Green
}

Write-Host ""
Write-Host "Files:"
Write-Host "  Certificate: $certPath"
Write-Host "  Private Key: $keyPath"
Write-Host ""
Write-Host "To use with LLM Gateway, add to config.lua:"
Write-Host "  tls = {"
Write-Host "      enabled = true,"
Write-Host "      cert = `"$certPath`","
Write-Host "      key = `"$keyPath`","
Write-Host "      https_listen = `"0.0.0.0:9443`","
Write-Host "  }"
Write-Host ""
Write-Host "Or set environment variables:"
Write-Host "  `$env:LLM_TLS_ENABLED = 'true'"
Write-Host "  `$env:LLM_TLS_CERT = '$certPath'"
Write-Host "  `$env:LLM_TLS_KEY = '$keyPath'"
Write-Host "  `$env:LLM_HTTPS_LISTEN = '0.0.0.0:9443'"
