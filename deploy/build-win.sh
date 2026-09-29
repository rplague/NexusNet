#!/usr/bin/env bash
#
# NexusService_Template Windows 交叉编译打包脚本
#
# 在 Linux 上用 mingw 交叉编译 + NSIS 生成 Windows 安装器与便携 zip。
# 包名 / 二进制名自动从 Cargo.toml 的 [package] 派生，规则与 build-deb.sh 一致：
#   name = "NexusService_Template"
#     -> PKG = nexusservice-template   （安装目录 / 服务名）
#     -> BIN = NexusService_Template   （<bin>.exe）
#
# 前置:
#   rustup target add x86_64-pc-windows-gnu
#   cargo build --release --target x86_64-pc-windows-gnu
#   需要: makensis (NSIS)、curl、unzip、sha256sum、zip
#
# 用法: ./build-win.sh
# 产物: ../target/packaging/<pkg>_<version>_windows_amd64_setup.exe
#       ../target/packaging/<name>-windows-amd64.exe
#       ../target/packaging/<pkg>_<version>_windows_amd64.zip
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "${SCRIPT_DIR}")"

# 仅读取 [package] 段内的字段，避免误取依赖表。
read_package_field() {
    awk -v key="$1" '
        /^\[/ { in_pkg = ($0 == "[package]"); next }
        in_pkg && $0 ~ "^" key " *=" {
            if (match($0, /"[^"]*"/)) {
                print substr($0, RSTART + 1, RLENGTH - 2)
                exit
            }
        }
    ' "${PROJECT_DIR}/Cargo.toml"
}

NAME="$(read_package_field name)"
VERSION="$(read_package_field version)"
[ -n "${NAME}" ] || { echo "[build-win] 无法从 Cargo.toml 提取包名" >&2; exit 1; }
[ -n "${VERSION}" ] || { echo "[build-win] 无法从 Cargo.toml 提取版本" >&2; exit 1; }

PKG="$(printf '%s' "${NAME}" | tr '[:upper:]_' '[:lower:]-')"
BIN="${NAME}"
TARGET="x86_64-pc-windows-gnu"

# NSSM：固定版本 + SHA256 校验，构建时下载缓存，不随仓库分发二进制。
NSSM_VERSION="2.24"
NSSM_URL="https://nssm.cc/release/nssm-${NSSM_VERSION}.zip"
NSSM_SHA256="727d1e42275c605e0f04aba98095c38a8e1e46def453cdffce42869428aa6743"

OUT_DIR="${PROJECT_DIR}/target/packaging"
STAGE="${PROJECT_DIR}/target/win-stage"
CACHE_DIR="${PROJECT_DIR}/target/nssm-cache"
NSSM_EXE="${CACHE_DIR}/nssm.exe"
BINARY="${PROJECT_DIR}/target/${TARGET}/release/${BIN}.exe"
SETUP="${OUT_DIR}/${PKG}_${VERSION}_windows_amd64_setup.exe"
PORTABLE="${OUT_DIR}/${NAME}-windows-amd64.exe"
ZIP="${OUT_DIR}/${PKG}_${VERSION}_windows_amd64.zip"

echo "[build-win] 包名=${PKG} 二进制=${BIN} 版本=${VERSION} 目标=${TARGET}"

[ -f "${BINARY}" ] || {
    echo "[build-win] 未找到 ${BINARY}，请先: cargo build --release --target ${TARGET}" >&2
    exit 1
}
command -v makensis >/dev/null 2>&1 || { echo "[build-win] 需要 makensis (NSIS)" >&2; exit 1; }

mkdir -p "${OUT_DIR}" "${STAGE}" "${CACHE_DIR}"

# 获取并校验 NSSM
if [ ! -f "${NSSM_EXE}" ]; then
    echo "[build-win] 下载 NSSM ${NSSM_VERSION}"
    tmp_zip="${CACHE_DIR}/nssm-${NSSM_VERSION}.zip"
    curl -fsSL -o "${tmp_zip}" "${NSSM_URL}"
    echo "${NSSM_SHA256}  ${tmp_zip}" | sha256sum -c - >/dev/null
    unzip -p "${tmp_zip}" "nssm-${NSSM_VERSION}/win64/nssm.exe" > "${NSSM_EXE}"
    rm -f "${tmp_zip}"
fi

# 渲染安装器脚本
sed -e "s|@PKG@|${PKG}|g" \
    -e "s|@BIN@|${BIN}|g" \
    -e "s|@VERSION@|${VERSION}|g" \
    -e "s|@BIN_EXE@|${BINARY}|g" \
    -e "s|@NSSM_EXE@|${NSSM_EXE}|g" \
    -e "s|@OUTFILE@|${SETUP}|g" \
    "${SCRIPT_DIR}/win/service.nsi" > "${STAGE}/service.nsi"

echo "[build-win] makensis 编译安装器"
makensis -V2 "${STAGE}/service.nsi" >/dev/null

# 便携裸 exe 与 zip
install -m 0644 "${BINARY}" "${PORTABLE}"
rm -f "${ZIP}"
( cd "${OUT_DIR}" && zip -q -j "${ZIP}" "$(basename "${PORTABLE}")" )

echo "[build-win] 产物:"
ls -lh "${SETUP}" "${PORTABLE}" "${ZIP}"
