#!/usr/bin/env bash
#
# NexusNet deb 包构建脚本
#
# 直接使用 dpkg-deb --build 组装 .deb（无需 cargo-deb / 网络），
# 在 Debian / Ubuntu 系主机上即可运行（需 dpkg-deb）。
#
# 用法: ./build-deb.sh
# 产物: ../target/packaging/nexusnet_<version>_<arch>.deb
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "${SCRIPT_DIR}")"
PKG="nexusnet"

# 从 Cargo.toml 提取版本（以 version = "x.y.z" 为准）
VERSION="$(awk -F'"' '/^version *=/{print $2; exit}' "${PROJECT_DIR}/Cargo.toml")"
[ -n "${VERSION}" ] || { echo "[build-deb] 无法从 Cargo.toml 提取版本" >&2; exit 1; }
ARCH="$(dpkg --print-architecture)"

OUT_DIR="${PROJECT_DIR}/target/packaging"
STAGE="${OUT_DIR}/${PKG}_${VERSION}_${ARCH}"
BINARY="${PROJECT_DIR}/target/release/NexusNet"
DEB="${OUT_DIR}/${PKG}_${VERSION}_${ARCH}.deb"

echo "[build-deb] 版本=${VERSION} 架构=${ARCH}"

# 前置检查
[ -x "${BINARY}" ] || { echo "[build-deb] 未找到二进制，请先: cargo build --release" >&2; exit 1; }

# 清理并准备暂存目录
rm -rf "${STAGE}"
mkdir -p "${STAGE}/DEBIAN" \
         "${STAGE}/usr/bin" \
         "${STAGE}/lib/systemd/system" \
         "${STAGE}/usr/lib/tmpfiles.d" \
         "${STAGE}/usr/lib/sysusers.d"

# 二进制
install -m 0755 "${BINARY}" "${STAGE}/usr/bin/NexusNet"

# 配置文件
install -m 0644 "${SCRIPT_DIR}/nexusnet.service"     "${STAGE}/lib/systemd/system/${PKG}.service"
install -m 0644 "${SCRIPT_DIR}/nexusnet.tmpfiles.conf" "${STAGE}/usr/lib/tmpfiles.d/${PKG}.conf"
install -m 0644 "${SCRIPT_DIR}/nexusnet.sysusers"    "${STAGE}/usr/lib/sysusers.d/${PKG}.conf"

# 维护脚本
install -m 0755 "${PROJECT_DIR}/deb/postinst" "${STAGE}/DEBIAN/postinst"
install -m 0755 "${PROJECT_DIR}/deb/prerm"    "${STAGE}/DEBIAN/prerm"
install -m 0755 "${PROJECT_DIR}/deb/postrm"   "${STAGE}/DEBIAN/postrm"

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
Description: NexusNet - OAHD 计划的核心网络层
 基于 libp2p 的去中心化 P2P 节点网络。通过 Kademlia DHT 实现节点自动发现与服务注册查询；
 利用边车模式将远程服务请求经 TCP 转发到本地业务进程，业务端语言无关。
EOF

# 打包（root-owner-group 使包内文件属主归 root，无需 fakeroot）
dpkg-deb --build --root-owner-group "${STAGE}" "${DEB}" >/dev/null

echo "[build-deb] 产物: ${DEB}"
echo "[build-deb] 包内容预览:"
dpkg-deb -c "${DEB}"
