#!/usr/bin/env bash
#
# NexusNet systemd 安装脚本（幂等，可重复执行）
#
# 作用：
#   1. 构建 release 二进制
#   2. 创建专用系统用户 nexusnet
#   3. 创建标准目录（/etc/nexusnet、/var/lib/nexusnet、/var/log/nexusnet）
#   4. 安装二进制到 /usr/local/bin/NexusNet
#   5. 安装 systemd 单元与配套配置
#   6. 迁移已有节点身份（可选）
#   7. 启用并启动服务
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
UNIT_NAME="nexusnet"
SERVICE_USER="nexusnet"
SERVICE_GROUP="nexusnet"

BIN_SRC="${PROJECT_DIR}/target/release/NexusNet"
BIN_DST="/usr/local/bin/${UNIT_NAME}"
UNIT_SRC="${SCRIPT_DIR}/${UNIT_NAME}.service"
UNIT_DST="/etc/systemd/system/${UNIT_NAME}.service"
TMPFILES_SRC="${SCRIPT_DIR}/${UNIT_NAME}.tmpfiles.conf"
TMPFILES_DST="/etc/tmpfiles.d/${UNIT_NAME}.conf"
SYSUSERS_SRC="${SCRIPT_DIR}/${UNIT_NAME}.sysusers"
SYSUSERS_DST="/etc/sysusers.d/${UNIT_NAME}.conf"

CONFIG_DIR="/etc/${UNIT_NAME}"
DATA_DIR="/var/lib/${UNIT_NAME}"
LOG_DIR="/var/log/${UNIT_NAME}"
CONFIG_FILE="${CONFIG_DIR}/config.toml"
KEYPAIR_FILE="${DATA_DIR}/keypair.bin"

log() { printf '[install] %s\n' "$*"; }
err() { printf '[install] ERROR: %s\n' "$*" >&2; }

# --- 1. 构建 release 二进制 ---
if [ -x "${BIN_DST}" ] && [ "${BIN_SRC}" -nt "${BIN_DST}" ]; then
    log "检测到新的源码构建，重新构建..."
fi
log "构建 release 二进制..."
(cd "${PROJECT_DIR}" && cargo build --release)
[ -x "${BIN_SRC}" ] || { err "构建产物不存在: ${BIN_SRC}"; exit 1; }

# --- 2. 创建专用用户 ---
if id "${SERVICE_USER}" >/dev/null 2>&1; then
    log "用户 ${SERVICE_USER} 已存在，跳过创建"
else
    if command -v systemd-sysusers >/dev/null 2>&1; then
        install -m 0644 "${SYSUSERS_SRC}" "${SYSUSERS_DST}"
        systemd-sysusers "${SYSUSERS_DST}" || true
        systemd-sysusers || true
    else
        useradd --system --no-create-home --shell /usr/sbin/nologin "${SERVICE_USER}"
    fi
fi
[ "$(id -u ${SERVICE_USER})" -ge 0 ] || { err "用户创建失败"; exit 1; }

# --- 3. 创建标准目录 ---
if command -v systemd-tmpfiles >/dev/null 2>&1; then
    install -m 0644 "${TMPFILES_SRC}" "${TMPFILES_DST}"
    systemd-tmpfiles --create "${TMPFILES_DST}"
else
    mkdir -p "${CONFIG_DIR}" "${DATA_DIR}" "${LOG_DIR}"
    chown "${SERVICE_USER}:${SERVICE_GROUP}" "${CONFIG_DIR}" "${DATA_DIR}" "${LOG_DIR}"
    chmod 0755 "${CONFIG_DIR}"; chmod 0700 "${DATA_DIR}"; chmod 0750 "${LOG_DIR}"
fi

# --- 4. 安装二进制 ---
install -o root -g root -m 0755 "${BIN_SRC}" "${BIN_DST}"
log "已安装二进制到 ${BIN_DST}"

# --- 5. 安装 systemd 单元 ---
install -m 0644 "${UNIT_SRC}" "${UNIT_DST}"
install -m 0644 "${TMPFILES_SRC}" "${TMPFILES_DST}"
install -m 0644 "${SYSUSERS_SRC}" "${SYSUSERS_DST}"
log "已安装 systemd 相关配置"

# --- 6. 迁移已有节点身份（防止 PeerId 变更）---
if [ ! -f "${KEYPAIR_FILE}" ] && [ -f "${PROJECT_DIR}/keypair.bin" ]; then
    install -o "${SERVICE_USER}" -g "${SERVICE_GROUP}" -m 0600 "${PROJECT_DIR}/keypair.bin" "${KEYPAIR_FILE}"
    log "已迁移节点身份到 ${KEYPAIR_FILE}"
fi
mkdir -p "${DATA_DIR}"
chown "${SERVICE_USER}:${SERVICE_GROUP}" "${DATA_DIR}" 2>/dev/null || true

# --- 7. 启用并启动服务 ---
systemctl daemon-reload
systemctl enable --now "${UNIT_NAME}" 2>/dev/null || {
    err "无法立即启动服务，请检查: systemctl status ${UNIT_NAME}"
    exit 1
}

log "完成。可运行 'systemctl status ${UNIT_NAME}' 与 'journalctl -u ${UNIT_NAME} -f' 查看"
