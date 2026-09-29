#!/usr/bin/env bash
#
# NexusNet deb 包构建脚本
#
# 直接使用 dpkg-deb --build 组装 .deb，
# 在 Debian / Ubuntu 系主机上即可运行（需 dpkg-deb）。
#
# 包名 / 二进制名自动从 Cargo.toml 的 [package] 派生：
#   name = "NexusNet"
#     -> PKG  = nexusnet   （Debian 包名 / systemd 单元名）
#     -> BIN  = NexusNet   （安装到 /usr/bin/）
# 修改 Cargo.toml 的 name 后，本脚本与 systemd 单元会自动跟随，无需改脚本。
#
# 用法: ./build-deb.sh
# 产物: ../target/packaging/<pkg>_<version>_<arch>.deb
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
[ -n "${NAME}" ] || { echo "[build-deb] 无法从 Cargo.toml 提取包名" >&2; exit 1; }
[ -n "${VERSION}" ] || { echo "[build-deb] 无法从 Cargo.toml 提取版本" >&2; exit 1; }

# Debian 包名规则：仅小写字母、数字、'-'、'+'、'.'；将大写转小写、'_' 转 '-'
PKG="$(printf '%s' "${NAME}" | tr '[:upper:]_' '[:lower:]-')"
BIN="${NAME}"
UNIT_NAME="${PKG}"
SERVICE_USER="nexusnet"

ARCH="$(dpkg --print-architecture)"

OUT_DIR="${PROJECT_DIR}/target/packaging"
STAGE="${OUT_DIR}/${PKG}_${VERSION}_${ARCH}"
BINARY="${PROJECT_DIR}/target/release/${BIN}"
DEB="${OUT_DIR}/${PKG}_${VERSION}_${ARCH}.deb"

echo "[build-deb] 包名=${PKG} 二进制=${BIN} 版本=${VERSION} 架构=${ARCH}"

# 前置检查
[ -x "${BINARY}" ] || { echo "[build-deb] 未找到二进制 ${BINARY}，请先: cargo build --release" >&2; exit 1; }

# 渲染打包素材中的占位符
render() {
    sed -e "s/@PKG@/${PKG}/g" \
        -e "s/@BIN@/${BIN}/g" \
        -e "s/@USER@/${SERVICE_USER}/g" "$1"
}

# 清理并准备暂存目录
rm -rf "${STAGE}"
mkdir -p "${STAGE}/DEBIAN" \
         "${STAGE}/usr/bin" \
         "${STAGE}/lib/systemd/system" \
         "${STAGE}/usr/lib/tmpfiles.d"

# 二进制
install -m 0755 "${BINARY}" "${STAGE}/usr/bin/${BIN}"

# 打包素材
render "${SCRIPT_DIR}/service.service"       > "${STAGE}/lib/systemd/system/${UNIT_NAME}.service"
render "${SCRIPT_DIR}/service.tmpfiles.conf" > "${STAGE}/usr/lib/tmpfiles.d/${PKG}.conf"
chmod 0644 "${STAGE}/lib/systemd/system/${UNIT_NAME}.service" \
           "${STAGE}/usr/lib/tmpfiles.d/${PKG}.conf"

# 维护脚本
render "${PROJECT_DIR}/deb/postinst" > "${STAGE}/DEBIAN/postinst"
render "${PROJECT_DIR}/deb/prerm"    > "${STAGE}/DEBIAN/prerm"
render "${PROJECT_DIR}/deb/postrm"   > "${STAGE}/DEBIAN/postrm"
chmod 0755 "${STAGE}/DEBIAN/postinst" "${STAGE}/DEBIAN/prerm" "${STAGE}/DEBIAN/postrm"

# 计算 Installed-Size（以 KB 计）
INSTALLED_SIZE="$(du -sk --exclude=DEBIAN "${STAGE}" | cut -f1)"

# 生成 control
cat > "${STAGE}/DEBIAN/control" <<EOF
Package: ${PKG}
Version: ${VERSION}
Section: net
Priority: optional
Architecture: ${ARCH}
Maintainer: OAHD
Depends: libc6 (>= 2.31)
Installed-Size: ${INSTALLED_SIZE}
Description: ${NAME} - OAHD 计划的核心网络层
  基于 libp2p 的去中心化 P2P 节点网络。通过 Kademlia DHT 实现节点自动发现与服务注册查询；
  利用边车模式将远程服务请求经 TCP 转发到本地业务进程，业务端语言无关。
EOF

# 打包（root-owner-group 使包内文件属主归 root，无需 fakeroot）
dpkg-deb --build --root-owner-group "${STAGE}" "${DEB}" >/dev/null

echo "[build-deb] 产物: ${DEB}"
echo "[build-deb] 包内容预览:"
dpkg-deb -c "${DEB}"
