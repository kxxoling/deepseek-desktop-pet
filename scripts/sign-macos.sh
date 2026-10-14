#!/bin/bash
# macOS 应用签名与公证（release workflow 调用）
# 用法：scripts/sign-macos.sh <path/to/ds-pet.app>
#
# 凭据来自 GitHub Secrets（未配置时退化为 ad-hoc 签名：本地可运行，不通过 Gatekeeper 分发）：
#   MACOS_CERTIFICATE_P12       Developer ID Application 证书（.p12，base64）
#   MACOS_CERTIFICATE_PASSWORD  证书密码
#   MACOS_KEYCHAIN_PASSWORD     临时 keychain 密码（任意随机串）
#   MACOS_NOTARY_KEY_ID         App Store Connect API Key ID
#   MACOS_NOTARY_ISSUER_ID      App Store Connect Issuer ID
#   MACOS_NOTARY_KEY            API Key 内容（.p8，base64）
set -euo pipefail

APP="$1"
[ -d "$APP" ] || { echo "用法：$0 <ds-pet.app>" >&2; exit 1; }

if [ -z "${MACOS_CERTIFICATE_P12:-}" ]; then
  echo "未配置签名凭据，使用 ad-hoc 签名（Gatekeeper 分发需配置 Secrets，见脚本头注释）" >&2
  codesign --force --deep --sign - "$APP"
  exit 0
fi

KEYCHAIN=ds-pet-build.keychain
CERT_FILE=$(mktemp /tmp/ds-pet-cert.XXXXXX.p12)
cleanup() {
  security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
  rm -f "$CERT_FILE"
}
trap cleanup EXIT

# 导入证书到临时 keychain
printf '%s' "$MACOS_CERTIFICATE_P12" | base64 --decode > "$CERT_FILE"
security create-keychain -p "$MACOS_KEYCHAIN_PASSWORD" "$KEYCHAIN"
security unlock-keychain -p "$MACOS_KEYCHAIN_PASSWORD" "$KEYCHAIN"
security import "$CERT_FILE" -k "$KEYCHAIN" -P "$MACOS_CERTIFICATE_PASSWORD" -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple: -k "$MACOS_KEYCHAIN_PASSWORD" "$KEYCHAIN"

IDENTITY=$(security find-identity -v -p codesigning "$KEYCHAIN" | grep "Developer ID Application" | head -1 | sed -E 's/.*"(.*)"/\1/')
[ -n "$IDENTITY" ] || { echo "keychain 中未找到 Developer ID Application 证书" >&2; exit 1; }

# hardened runtime 是公证的前提
codesign --force --deep --options runtime --timestamp --sign "$IDENTITY" "$APP"
echo "已签名：$IDENTITY"

# 公证（App Store Connect API key）
if [ -n "${MACOS_NOTARY_KEY_ID:-}" ]; then
  KEY_FILE=$(mktemp /tmp/ds-pet-notary.XXXXXX.p8)
  printf '%s' "$MACOS_NOTARY_KEY" | base64 --decode > "$KEY_FILE"
  xcrun notarytool store-credentials ds-pet-notary \
    --key "$KEY_FILE" --key-id "$MACOS_NOTARY_KEY_ID" --issuer "$MACOS_NOTARY_ISSUER_ID"
  rm -f "$KEY_FILE"
  # 公证需要压缩包形式提交；通过后把 ticket 订到 .app 本体
  ditto -c -k --keepParent "$APP" /tmp/ds-pet-notary-submit.zip
  xcrun notarytool submit /tmp/ds-pet-notary-submit.zip --keychain-profile ds-pet-notary --wait
  xcrun stapler staple "$APP"
  echo "已公证并 staple"
else
  echo "未配置公证凭据，跳过 notarization（Gatekeeper 首次运行仍需用户放行）" >&2
fi
